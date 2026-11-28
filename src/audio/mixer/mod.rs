//! Returns run after the input paths.  Each return taps a sender at the
//! slowest live input path's latency minus that sender's, counting every
//! live input path, so a send on or off does not move the alignment.  A
//! feed change waits until the path gain is 0.
//!
//! The metronome is added after the safety ceiling and before mute.  The
//! click-free copy, taken before the tick, is what a take records unless
//! "Record the click" is on.
//!
//! A chunk with no chain runtime plays the chain input delayed by its
//! latency, so the sum keeps its phase.  DSP time is one clock read per
//! stage: no allocation, no lock.  The total peak is the worst block.

use std::sync::atomic::Ordering;
use std::time::Instant;

use super::calibration::CalibrationEmitter;
use super::click_level_gain;
use super::delay::{DelayLine, DelayTap, TAP_FADE_SECONDS};
use super::denormal::DenormalGuard;
use super::metronome::{Metronome, CLICK_LEVEL_DEFAULT_DB};
use super::output_map::{
    DeviceFrame, DeviceMapper, PathLines, PathTiming, MAX_ALIGN_DELAY, MAX_EXTRA_OUTPUTS,
};
use super::path::{LiveSlots, MAX_PATHS};
use super::plugins::MAX_ALIGN;
use super::resampler::FrameResampler;
use super::rt_guard::RtGuard;
use super::shared::Shared;

#[cfg(test)]
mod align_tests;
pub(crate) mod drift;
mod path;
pub(crate) mod ramp;
#[cfg(test)]
mod rt_tests;

use drift::{DRIFT_FILTER_SECONDS, DRIFT_KI, DRIFT_KP, DRIFT_LIMIT_PPM, DRIFT_LIMIT_RATIO};
use path::{DspMeter, PathProcessor};

pub(crate) use ramp::Ramp;
use ramp::{fade_down, fade_up, CONTROL_RAMP_SECONDS, DROPOUT_FADE_SECONDS};

/// An empty ring repeats the last frame (resampler continuity) and sets
/// `missed`.
pub(crate) fn take_ring_frame(shared: &Shared, missed: &mut bool, last: (f32, f32)) -> (f32, f32) {
    match shared.pop_frame() {
        Some(pair) => pair,
        None => {
            *missed = true;
            last
        }
    }
}

pub(crate) struct OutputMixer {
    output_rate: u32,
    source_rate: u32,
    resampler: FrameResampler,
    last: (f32, f32),
    fade_gain: f32,
    held: (f32, f32),
    /// A full ring before the first frame starts at full level.
    played: bool,
    /// The rest of this block fades the held frame; trickle is ignored.
    dropping: bool,
    priming: bool,
    started: bool,
    // Steers the ratio from ring fill instead of splicing frames.  The
    // integral is the learned clock offset.  `drift_error` is `None` until
    // the first reading after priming.
    drift_ppm: f32,
    drift_integral: f32,
    drift_error: Option<f32>,
    /// Switching to or from the 1:1 bypass resets the resampler.
    resampling: bool,
    stable_since: Instant,
    paths: [PathProcessor; MAX_PATHS],
    live: LiveSlots,
    /// One line per path, after the master.  Each device taps it.
    lines: PathLines,
    /// Sender frames after path gain.  Written only while a send is on.
    send_lines: [DelayLine; MAX_PATHS],
    /// `[sender][return]`: the tap onto the slowest sender.
    send_taps: [[DelayTap; MAX_PATHS]; MAX_PATHS],
    /// `[sender][return]` send level.
    send_levels: [[Ramp; MAX_PATHS]; MAX_PATHS],
    /// Mute and level as one ramp, so either can move without a step.
    gain: Ramp,
    /// The first block snaps, so the bridge does not fade from defaults.
    gain_adopted: bool,
    device: DeviceMapper,
    /// Duplex pairs, or a generic extra output's channels into its ring.
    /// Only the first `live_extra_outputs` are written.
    extra_outputs: [DeviceMapper; MAX_EXTRA_OUTPUTS],
    live_extra_outputs: usize,
    /// Ring-path feeds this block, with the ring generation latched at
    /// the start.
    feeding: [Option<u64>; MAX_EXTRA_OUTPUTS],
    total_dsp: DspMeter,
    /// Clock-output duck and click.  Only this callback touches it.
    calibration: CalibrationEmitter,
    /// One click for every device.  An extra-output mixer has none: the
    /// tick is already in the lanes.
    metronome: Metronome,
    /// A measurement ducks each mapper after its click line, not here.
    click_gain: Ramp,
    click_adopted: bool,
    /// Rate the beat phase was scaled to.  0 until the first block.
    click_rate: u32,
}

impl OutputMixer {
    pub(crate) fn new(output_rate: u32, device_channels: usize) -> Self {
        let output_rate = output_rate.max(1);
        Self {
            output_rate,
            source_rate: output_rate,
            resampler: FrameResampler::new(output_rate, output_rate),
            last: (0.0, 0.0),
            fade_gain: 1.0,
            held: (0.0, 0.0),
            played: false,
            dropping: false,
            // Wait for the ring to reach its target before draining it.
            priming: true,
            started: false,
            drift_ppm: 0.0,
            drift_integral: 0.0,
            drift_error: None,
            resampling: false,
            stable_since: Instant::now(),
            paths: std::array::from_fn(|slot| PathProcessor::new(output_rate, slot)),
            live: LiveSlots::default(),
            lines: std::array::from_fn(|_| DelayLine::new(MAX_ALIGN_DELAY as usize)),
            send_lines: std::array::from_fn(|_| DelayLine::new(MAX_ALIGN as usize)),
            send_taps: [[DelayTap::default(); MAX_PATHS]; MAX_PATHS],
            send_levels: std::array::from_fn(|_| std::array::from_fn(|_| Ramp::new(0.0))),
            gain: Ramp::new(1.0),
            gain_adopted: false,
            device: DeviceMapper::new(device_channels),
            // A duplex extra output is a stereo pair.  A generic one takes
            // its device's count when its ring starts.
            extra_outputs: std::array::from_fn(|_| DeviceMapper::new(2)),
            live_extra_outputs: 0,
            feeding: [None; MAX_EXTRA_OUTPUTS],
            total_dsp: DspMeter::default(),
            calibration: CalibrationEmitter::new(),
            metronome: Metronome::new(),
            click_gain: Ramp::new(click_level_gain(CLICK_LEVEL_DEFAULT_DB)),
            click_adopted: false,
            click_rate: 0,
        }
    }

    /// Parks every chain so a test can take one before the next block.
    #[cfg(test)]
    pub(crate) fn park_chains_for_test(&mut self) {
        for path in &mut self.paths {
            path.release_chain();
        }
    }

    /// Publishes the device channel count.  Call outside the callback.
    pub(crate) fn publish_device_channels(&self, shared: &Shared) {
        self.device.publish_channels(&shared.clock_output);
    }

    /// Duplex extra outputs are stereo pairs.  A generic extra output's
    /// stream publishes its own count.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn publish_extra_output_channels(&self, shared: &Shared) {
        let count = shared.extra_output_count();
        for index in 0..count {
            self.extra_outputs[index].publish_channels(&shared.extra_outputs[index]);
        }
    }

    /// Grows every path's scratch before the callback, so a block never
    /// allocates.  A larger host block still renders; debug asserts it.
    pub(crate) fn prepare(&mut self, frames: usize) {
        for path in &mut self.paths {
            path.prepare(frames);
        }
    }

    /// Installs the kernel the rate publisher planned.  Until it arrives
    /// the mixer keeps its pair: the callback must not build one.
    pub(crate) fn follow_source_rate(&mut self, shared: &Shared) {
        let source = self.followed_source(shared);
        if source == self.source_rate {
            return;
        }
        shared.clock_kernel.try_adopt(&mut self.resampler);
        if self.resampler.pair() == (source, self.output_rate) {
            self.source_rate = source;
            self.drift_ppm = 0.0;
            self.drift_integral = 0.0;
            self.drift_error = None;
        }
    }

    fn followed_source(&self, shared: &Shared) -> u32 {
        match shared.source_rate.load(Ordering::Relaxed) {
            0 => self.output_rate,
            rate => rate,
        }
    }

    /// Builds the source filter on the setup thread.  A callback adopts a
    /// planned table instead.
    pub(crate) fn configure_source_rate(&mut self, shared: &Shared) {
        let source = self.followed_source(shared);
        if source == self.source_rate {
            return;
        }
        self.source_rate = source;
        self.resampler.configure(source, self.output_rate);
        self.drift_ppm = 0.0;
        self.drift_integral = 0.0;
        self.drift_error = None;
    }

    #[cfg(test)]
    pub(crate) fn configure_calls(&self) -> usize {
        self.resampler.configure_calls()
    }

    #[cfg(test)]
    pub(crate) fn resampler_pair(&self) -> (u32, u32) {
        self.resampler.pair()
    }

    /// Trims the ratio from ring fill.  The integral holds the clock offset.
    fn steer_drift(&mut self, shared: &Shared, target: usize, seconds: f32, drift: bool) -> f32 {
        if !drift {
            self.drift_ppm = 0.0;
            self.drift_integral = 0.0;
            self.drift_error = None;
            return 0.0;
        }
        if self.priming {
            // The integral stays.  Clearing it makes a small buffer
            // drop out again.
            self.drift_ppm = 0.0;
            self.drift_error = None;
            return 0.0;
        }
        // Seconds of fill, so the gains do not depend on rate or block size.
        let raw = (shared.ring_frames() as f32 - target as f32) / self.source_rate as f32;
        let error = match self.drift_error {
            Some(filtered) => {
                let weight = (seconds / DRIFT_FILTER_SECONDS).min(1.0);
                filtered + (raw - filtered) * weight
            }
            None => raw,
        };
        self.drift_error = Some(error);
        self.drift_integral = (self.drift_integral + DRIFT_KI * error * seconds)
            .clamp(-DRIFT_LIMIT_RATIO, DRIFT_LIMIT_RATIO);
        let correction = DRIFT_KP * error + self.drift_integral;
        self.drift_ppm = (correction * 1_000_000.0).clamp(-DRIFT_LIMIT_PPM, DRIFT_LIMIT_PPM);
        self.drift_ppm
    }

    /// Applies UI changes for this block.  `frames` must fit
    /// [`Self::prepare`]: the scratch grows inside the callback.
    fn begin_block(&mut self, shared: &Shared, frames: usize) {
        let samples = CONTROL_RAMP_SECONDS * self.output_rate as f32;
        self.live = shared.live_slots();
        for slot in 0..MAX_PATHS {
            if !self.live.contains(slot) {
                self.paths[slot].release_chain();
                self.paths[slot].skip_idle(frames);
            }
        }
        let any_solo = shared.any_solo();
        // Only the mixer's first block adopts the path gains outright.
        let fade_in = self.gain_adopted;
        let mut timing = PathTiming {
            latency: [0; MAX_PATHS],
            rate: self.output_rate,
        };
        for slot in self.live.iter() {
            let path = &mut self.paths[slot];
            // Past the prepared size: release grows, debug stops.  Split
            // with `render_chunk` when a driver can hand over more.
            debug_assert!(
                frames <= path.block.len(),
                "a block of {frames} frames does not fit the {} prepared",
                path.block.len()
            );
            let restarted = path.begin_block(
                &shared.paths[slot],
                self.output_rate,
                samples,
                any_solo,
                fade_in,
            );
            if restarted {
                // A reused slot never replays what it held before.
                self.lines[slot].reset();
                self.send_lines[slot].reset();
            }
            // Unclamped, so the report can flag a chain past MAX_ALIGN.
            timing.latency[slot] = path.latency_raw;
            path.reserve_block(frames);
        }
        self.begin_sends(shared, samples, &mut timing);
        let silent = shared.mute.load(Ordering::Relaxed);
        let gain = if silent { 0.0 } else { shared.output_gain() };
        if self.gain_adopted {
            self.gain.set_target(gain, samples);
        } else {
            self.gain.snap(gain);
            self.gain_adopted = true;
        }
        // A measurement ducks the tick after the click line, so one
        // already waiting fades with the ones not yet generated.
        let ducking = shared.calibration.state().ducking();
        let click_gain = f32::from_bits(shared.metronome_gain.load(Ordering::Relaxed));
        if self.click_adopted {
            self.click_gain.set_target(click_gain, samples);
        } else {
            self.click_gain.snap(click_gain);
            self.click_adopted = true;
        }
        if self.click_rate != 0 && self.click_rate != self.output_rate {
            let ratio = self.output_rate as f64 / self.click_rate as f64;
            self.metronome.scale(ratio);
        }
        self.click_rate = self.output_rate;
        self.metronome.latch(
            shared.metronome_running.load(Ordering::Relaxed),
            f32::from_bits(shared.metronome_bpm.load(Ordering::Relaxed)),
            super::metronome::ClickRhythm::from_u8(shared.metronome_rhythm.load(Ordering::Relaxed)),
            self.output_rate,
        );
        self.device
            .begin_paths(&shared.clock_output, samples, self.live, &timing, frames);
        self.device.duck_click(ducking, samples);
        // Duplex extra outputs are in-graph; a generic one goes to its
        // ring.  One without a stream maps nothing and its meters read 0.
        self.live_extra_outputs = shared.extra_output_count();
        let direct = shared.direct.load(Ordering::Relaxed);
        for index in 0..self.live_extra_outputs {
            let feeding = shared.feeds_extra_output(index);
            self.feeding[index] = feeding.then(|| shared.extra_output_bus(index).generation());
            let channels = if feeding {
                shared.extra_output_bus(index).device_channels()
            } else {
                2
            };
            self.extra_outputs[index].set_channels(channels);
            self.extra_outputs[index].begin_paths(
                &shared.extra_outputs[index],
                samples,
                self.live,
                &timing,
                frames,
            );
            self.extra_outputs[index].duck_click(ducking, samples);
            if !feeding && !direct {
                // Nothing plays it, so there is no alignment to show.
                // Duplex plays every extra output in-graph.
                shared.extra_outputs[index].clear_alignment();
            }
        }
        for extra_output in &shared.extra_outputs[self.live_extra_outputs..] {
            extra_output.clear_alignment();
        }
        // A slot that is not live maps nothing; one that comes back must
        // not play the ticks it queued before.
        for mapper in &mut self.extra_outputs[self.live_extra_outputs..] {
            mapper.silence_click_line();
        }
    }

    /// Lines each return's senders up and adds that wait to its latency.
    fn begin_sends(&mut self, shared: &Shared, samples: f32, timing: &mut PathTiming) {
        let fade = TAP_FADE_SECONDS * self.output_rate as f32;
        for ret in 0..MAX_PATHS {
            let live_return = self.live.contains(ret) && self.paths[ret].is_return;
            // Every live input path counts, sending or not, so a send on
            // or off does not move the alignment.
            let mut wait = 0;
            // A send that was silent starts at its delay instead of fading
            // its tap there.
            let was_off: [bool; MAX_PATHS] =
                std::array::from_fn(|sender| self.send_levels[sender][ret].is_settled_at(0.0));
            for sender in 0..MAX_PATHS {
                let level = &mut self.send_levels[sender][ret];
                let sends = live_return
                    && sender != ret
                    && self.live.contains(sender)
                    && !self.paths[sender].is_return;
                if !sends {
                    level.snap(0.0);
                    continue;
                }
                let gain = shared.paths[sender].send_gain(ret);
                level.set_target(gain, samples);
                wait = wait.max(self.paths[sender].latency());
            }
            if !live_return {
                continue;
            }
            for (sender, off) in was_off.into_iter().enumerate() {
                let tap = &mut self.send_taps[sender][ret];
                let delay = wait.saturating_sub(self.paths[sender].latency());
                if off {
                    tap.snap(delay);
                } else {
                    tap.set_target(delay, fade);
                }
            }
            timing.latency[ret] = timing.latency[ret].saturating_add(wait);
        }
    }

    /// Each return's block from its senders, after path gain.  Runs after
    /// [`PathProcessor::fill_gains`].
    fn mix_sends(&mut self, frames: usize) {
        let live = self.live;
        let Self {
            paths,
            send_lines,
            send_taps,
            send_levels,
            ..
        } = self;
        let mut sending = [false; MAX_PATHS];
        for sender in live.iter() {
            sending[sender] = !paths[sender].is_return
                && send_levels[sender]
                    .iter()
                    .any(|level| !level.is_settled_at(0.0));
            if !sending[sender] {
                // Nothing reads it; a send turned on later starts clean.
                send_lines[sender].reset();
            }
        }
        for index in 0..frames {
            for sender in live.iter() {
                if !sending[sender] {
                    continue;
                }
                let path = &paths[sender];
                let gain = path.gains[index];
                let [left, right, _, _] = path.block[index];
                send_lines[sender].push([left * gain, right * gain]);
            }
            for ret in live.iter() {
                if !paths[ret].is_return {
                    continue;
                }
                let mut sum = [0.0f32; 2];
                for sender in live.iter() {
                    let level = &mut send_levels[sender][ret];
                    if !sending[sender] || level.is_settled_at(0.0) {
                        continue;
                    }
                    let level = level.next();
                    let frame = send_taps[sender][ret].next(&send_lines[sender], 0);
                    sum[0] += frame[0] * level;
                    sum[1] += frame[1] * level;
                }
                paths[ret].stage_input(index, sum[0], sum[1]);
            }
        }
    }

    /// One input frame into every live input path.  A return is staged
    /// in [`Self::finish_block`].
    #[inline]
    fn stage_front(&mut self, index: usize, left: f32, right: f32) {
        for slot in self.live.iter() {
            if !self.paths[slot].is_return {
                self.paths[slot].stage_input(index, left, right);
            }
        }
    }

    /// Fronts run before any plugin can change the float mode.  Returns
    /// run after the paths they hear.  The tick is added in each mapper,
    /// after the ceiling.
    fn finish_block(
        &mut self,
        shared: &Shared,
        frames: usize,
        now: Instant,
        mut write: impl WriteFrame,
    ) {
        let live = self.live;
        // Every front runs before any plugin can touch the float mode.  The
        // returns run after the paths they hear.
        let mut mark = Instant::now();
        for slot in live.iter() {
            if !self.paths[slot].is_return {
                self.paths[slot].run_front(frames);
                mark = self.paths[slot].add_dsp_time(mark, Instant::now());
            }
        }
        for slot in live.iter() {
            if !self.paths[slot].is_return {
                self.paths[slot].run_effects(&shared.paths[slot], frames);
                mark = self.paths[slot].add_dsp_time(mark, Instant::now());
            }
        }

        // A plugin may have cleared FTZ; the POST gate, post EQ and master
        // still need it.  Dropped before the caller's guard, so the
        // original mode returns last.  Callers hold the `RtGuard`.
        let _post_denormals = DenormalGuard::new();
        mark = Instant::now();
        let mut total_ns = 0u64;
        for slot in live.iter() {
            if !self.paths[slot].is_return {
                self.paths[slot].finish_block(&shared.paths[slot], frames);
                mark = self.paths[slot].add_dsp_time(mark, Instant::now());
                total_ns +=
                    self.paths[slot].publish_dsp(&shared.paths[slot], frames, self.output_rate);
            }
        }
        for slot in live.iter() {
            self.paths[slot].fill_gains(frames);
        }
        if live.iter().any(|slot| self.paths[slot].is_return) {
            let start = Instant::now();
            self.mix_sends(frames);
            mark = Instant::now();
            total_ns += mark.saturating_duration_since(start).as_nanos() as u64;
            for slot in live.iter() {
                if self.paths[slot].is_return {
                    self.paths[slot].run_front(frames);
                    mark = self.paths[slot].add_dsp_time(mark, Instant::now());
                }
            }
            for slot in live.iter() {
                if self.paths[slot].is_return {
                    self.paths[slot].run_effects(&shared.paths[slot], frames);
                    mark = self.paths[slot].add_dsp_time(mark, Instant::now());
                }
            }
            // The returns' effects may have cleared FTZ again.
            let _return_denormals = DenormalGuard::new();
            mark = Instant::now();
            for slot in live.iter() {
                if self.paths[slot].is_return {
                    self.paths[slot].finish_block(&shared.paths[slot], frames);
                    mark = self.paths[slot].add_dsp_time(mark, Instant::now());
                    total_ns +=
                        self.paths[slot].publish_dsp(&shared.paths[slot], frames, self.output_rate);
                }
            }
        }
        // The total's peak is the peak of each block's sum: paths that peak
        // in different blocks must not add up.
        shared
            .total_dsp
            .store(self.total_dsp.push(total_ns, frames, self.output_rate));

        let extra_outputs = &mut self.extra_outputs[..self.live_extra_outputs];
        let mut peak_l = 0.0f32;
        let mut peak_r = 0.0f32;
        // The click is after the device map, so its level ignores the
        // faders.  Meters see the same duck.
        let (duck_target, duck_step) =
            self.calibration
                .begin(&shared.calibration, self.output_rate, now);
        for index in 0..frames {
            // Master before the maps, so one path is `(x * gain) * weight`.
            let gain = self.gain.next();
            let (duck, click) = self
                .calibration
                .next(&shared.calibration, duck_target, duck_step);
            // Starts from the first path's frame, so one path passes bit
            // for bit.
            let mut sum: Option<[f32; 4]> = None;
            for slot in live.iter() {
                let path = &mut self.paths[slot];
                let mut out = path.block[index];
                // Unity copies.
                let path_gain = path.gains[index];
                if path_gain != 1.0 {
                    for value in out.iter_mut() {
                        *value *= path_gain;
                    }
                }
                for value in out.iter_mut() {
                    *value *= gain;
                }
                self.lines[slot].push([out[0], out[1]]);
                sum = Some(match sum {
                    None => out,
                    Some(mut sum) => {
                        for (total, value) in sum.iter_mut().zip(out) {
                            *total += value;
                        }
                        sum
                    }
                });
            }
            let mut out = sum.unwrap_or([0.0; 4]);
            if duck != 1.0 || click != 0.0 {
                for value in out.iter_mut() {
                    *value *= duck;
                }
                out[0] += click;
                out[1] += click;
            }
            peak_l = peak_l.max(out[0].abs());
            peak_r = peak_r.max(out[1].abs());
            shared.push_scope_out(out[0], out[1]);
            // Not in the path sum.  The same sample goes to every device.
            // A sounding click finishes; the duck ramp, not a gate, takes
            // it out.  Exact 0 is not multiplied.
            let voice = self.metronome.next();
            let tick_gain = self.click_gain.next();
            let tick = if voice == 0.0 || tick_gain == 0.0 {
                0.0
            } else {
                voice * tick_gain
            };
            let mapped = self.device.mapped_channels();
            let clock = self
                .device
                .map_paths(&self.lines, tick)
                .ducked(duck, click, mapped);
            write(&clock, extra_outputs, index, &self.lines, out, tick);
        }

        // Beat and rhythm move together, so a pending change cannot show
        // a beat the new bar does not have.
        shared.publish_metronome_beat(self.metronome.beat(), self.metronome.rhythm() as u8);

        shared.frames.fetch_add(frames as u64, Ordering::Relaxed);
        shared.out_l.store(peak_l.to_bits(), Ordering::Relaxed);
        shared.out_r.store(peak_r.to_bits(), Ordering::Relaxed);
        self.device.end_block(&shared.clock_output);
        for index in 0..self.live_extra_outputs {
            self.extra_outputs[index].end_block(&shared.extra_outputs[index]);
        }
        shared.note_extra_output_producer_block(frames);
        // A silent removed path keeps running until its taps and sends
        // have played out.
        for slot in live.iter() {
            let path = &self.paths[slot];
            if !path.fading {
                self.paths[slot].drain = None;
                continue;
            }
            if !path.gain.is_settled_at(0.0) {
                continue;
            }
            let drain = match path.drain {
                Some(left) => left.saturating_sub(frames as u32),
                None => {
                    let sends = live
                        .iter()
                        .filter(|&ret| {
                            self.paths[ret].is_return
                                && !self.send_levels[slot][ret].is_settled_at(0.0)
                        })
                        .map(|ret| self.send_taps[slot][ret].longest())
                        .fold(0, u32::max);
                    self.extra_outputs[..self.live_extra_outputs]
                        .iter()
                        .map(|mapper| mapper.tap_delay(slot))
                        .fold(self.device.tap_delay(slot), u32::max)
                        .max(sends)
                }
            };
            self.paths[slot].drain = Some(drain);
            if drain == 0 {
                shared.paths[slot].finish_fade();
            }
        }
    }

    /// [`Self::render_direct`] once per live device: index 0 is the clock
    /// output, then each extra output.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn render_direct_devices(
        &mut self,
        shared: &Shared,
        input_left: &[f32],
        input_right: &[f32],
        mut write: impl FnMut(usize, usize, &DeviceFrame),
    ) {
        self.render_direct_with(
            shared,
            input_left,
            input_right,
            |clock, extra_outputs, index, paths, _, tick| {
                write(0, index, clock);
                for (extra_output, mapper) in extra_outputs.iter_mut().enumerate() {
                    write(extra_output + 1, index, mapper.map_paths(paths, tick));
                }
            },
        );
    }

    /// The direct path's rate.  Allocation-free: coefficients redesign on
    /// the next block.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn set_direct_rate(&mut self, rate: u32) {
        let rate = rate.max(1);
        if rate == self.output_rate {
            return;
        }
        self.output_rate = rate;
        self.source_rate = rate;
        for path in &mut self.paths {
            path.invalidate_settings();
        }
    }

    /// One block from the same graph cycle.  Both sides share one clock,
    /// so there is no ring, priming, drift or resampling.
    #[allow(dead_code)] // Single-device direct path; duplex fans out via devices.
    pub(crate) fn render_direct(
        &mut self,
        shared: &Shared,
        input_left: &[f32],
        input_right: &[f32],
        mut write: impl FnMut(usize, &DeviceFrame),
    ) {
        self.render_direct_with(
            shared,
            input_left,
            input_right,
            |clock, _, index, _, _, _| write(index, clock),
        );
    }

    #[cfg(test)]
    fn render_direct_taps(
        &mut self,
        shared: &Shared,
        input_left: &[f32],
        input_right: &[f32],
        mut write: impl FnMut(usize, [f32; 4]),
    ) {
        self.render_direct_with(shared, input_left, input_right, |_, _, index, _, out, _| {
            write(index, out)
        });
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn render_direct_with(
        &mut self,
        shared: &Shared,
        input_left: &[f32],
        input_right: &[f32],
        write: impl WriteFrame,
    ) {
        let _denormals = DenormalGuard::new();
        // Nothing in this block may allocate (see `audio::rt_guard`).
        let _no_allocations = RtGuard::enter();
        // Stamped at entry, so calibration measures the frames, not when
        // the effects finished.
        let now = Instant::now();
        let frames = input_left.len().min(input_right.len());
        self.begin_block(shared, frames);
        for index in 0..frames {
            self.stage_front(index, input_left[index], input_right[index]);
        }
        self.finish_block(shared, frames, now, write);
        // There is no clock offset to report on a single clock.
        self.drift_ppm = 0.0;
        shared.drift_ppm.store(0.0f32.to_bits(), Ordering::Relaxed);
    }

    /// One host block, mapped onto the device.  A callback that splits a
    /// larger block publishes it once with [`Self::note_host_block`] and
    /// renders the pieces with [`Self::render_chunk`].
    pub(crate) fn render(
        &mut self,
        shared: &Shared,
        frames: usize,
        mut write: impl FnMut(usize, &DeviceFrame),
    ) {
        self.note_host_block(shared, frames);
        self.render_with(shared, frames, |clock, _, index, _, _, _| {
            write(index, clock)
        });
    }

    /// One scratch-sized piece.  Publishes nothing: the floor follows the
    /// whole host block ([`Self::note_host_block`]).
    pub(crate) fn render_chunk(
        &mut self,
        shared: &Shared,
        frames: usize,
        mut write: impl FnMut(usize, &DeviceFrame),
    ) {
        self.render_with(shared, frames, |clock, _, index, _, _, _| {
            write(index, clock)
        });
    }

    /// The bridge-buffer floor follows the whole host block, in source
    /// frames.  The first block is skipped: a start-up fill is several
    /// times the steady step.
    pub(crate) fn note_host_block(&mut self, shared: &Shared, frames: usize) {
        let block_source_frames =
            (frames as u64 * self.source_rate as u64).div_ceil(self.output_rate as u64) as usize;
        if self.started {
            shared.note_output_block(block_source_frames);
        }
        self.started = true;
    }

    /// [`Self::render`] handing over the four taps.  Only 0/1 carry the mix.
    #[cfg(test)]
    fn render_taps(
        &mut self,
        shared: &Shared,
        frames: usize,
        mut write: impl FnMut(usize, [f32; 4]),
    ) {
        self.note_host_block(shared, frames);
        self.render_with(shared, frames, |_, _, index, _, out, _| write(index, out));
    }

    fn render_with(&mut self, shared: &Shared, frames: usize, mut write: impl WriteFrame) {
        let _denormals = DenormalGuard::new();
        // Nothing in this block may allocate (see `audio::rt_guard`).
        let _no_allocations = RtGuard::enter();
        let now = Instant::now();
        self.follow_source_rate(shared);
        self.begin_block(shared, frames);
        let drift = shared.drift_enabled.load(Ordering::Relaxed);
        let block_source_frames =
            (frames as u64 * self.source_rate as u64).div_ceil(self.output_rate as u64) as usize;
        let target = shared.target_frames();

        // A buffer cut far above the target is purged at once.  The
        // resampler is not reset: its window crossfades the jump.
        if !self.priming {
            let excess_line = target * 2 + block_source_frames;
            let fill = shared.ring_frames();
            if fill > excess_line {
                for _ in 0..fill - target {
                    if shared.pop_frame().is_none() {
                        break;
                    }
                }
            }
        }

        // Do not drain until the ring has filled, or playback dropouts.
        // After a dropout, wait until the held frame has faded, so a refill
        // does not jump off it.  A ring full before anything played starts
        // at full level.
        let faded = self.fade_gain == 0.0 || !self.played;
        if self.priming && faded && shared.ring_frames() >= target {
            self.priming = false;
        }

        // Trim the step instead of splicing.  A 1:1 bridge with drift on
        // still interpolates, so the path exists when the clocks differ.
        let seconds = frames as f32 / self.output_rate as f32;
        let drift_ppm = self.steer_drift(shared, target, seconds, drift);
        self.resampler.set_correction_ppm(drift_ppm);
        let needs_resample = self.resampler.active() || drift;
        if needs_resample != self.resampling {
            // Entering must not reuse bypass history.  Leaving skips the
            // frames fetched ahead (half the kernel), once.
            self.resampler.reset();
            self.resampling = needs_resample;
        }

        let mut missed = false;
        let fade_step = 1.0 / (DROPOUT_FADE_SECONDS * self.output_rate as f32).max(1.0);
        for index in 0..frames {
            let (left, right) = if self.priming || self.dropping {
                self.fade_gain = if self.held == (0.0, 0.0) {
                    0.0
                } else {
                    fade_down(self.fade_gain, fade_step)
                };
                let out = (self.held.0 * self.fade_gain, self.held.1 * self.fade_gain);
                // Once silent, resume inside this block when the buffer
                // is back.
                if self.priming && self.fade_gain == 0.0 && shared.ring_frames() >= target {
                    self.priming = false;
                }
                out
            } else {
                let last = self.last;
                let mut dry = false;
                let frame = if needs_resample {
                    self.resampler
                        .next(|| take_ring_frame(shared, &mut dry, last))
                } else {
                    take_ring_frame(shared, &mut dry, last)
                };
                self.last = frame;
                if dry {
                    missed = true;
                    self.dropping = true;
                    self.held = frame;
                    self.fade_gain = fade_down(self.fade_gain, fade_step);
                } else {
                    self.played = true;
                    if self.fade_gain < 1.0 {
                        self.fade_gain = fade_up(self.fade_gain, fade_step);
                    }
                }
                (frame.0 * self.fade_gain, frame.1 * self.fade_gain)
            };
            self.stage_front(index, left, right);
        }
        let feeding = self.feeding;
        self.finish_block(
            shared,
            frames,
            now,
            |clock, extra_outputs, index, paths, out, tick| {
                write(clock, extra_outputs, index, paths, out, tick);
                for (slot, mapper) in extra_outputs.iter_mut().enumerate() {
                    if let Some(generation) = feeding[slot] {
                        shared.push_extra_output_frame(
                            slot,
                            generation,
                            mapper.map_paths(paths, tick),
                        );
                    }
                }
            },
        );

        if missed {
            shared.dropouts.fetch_add(1, Ordering::Relaxed);
            // Grow the target instead of stuttering on every callback.
            shared.raise_target_after_dropout();
            self.priming = true;
            // The held frame keeps fading through the priming silence.
            self.dropping = false;
            self.resampler.reset();
            // The integral (the learned clock offset) survives the dropout;
            // only the filtered fill starts over.
            self.drift_ppm = 0.0;
            self.drift_error = None;
            self.stable_since = Instant::now();
        } else if !self.priming && self.stable_since.elapsed() >= shared.relax_after() {
            shared.relax_target();
            self.stable_since = Instant::now();
        }
        // Report the ratio trim the resampler actually ran with, not a
        // synthetic frame count.
        shared
            .drift_ppm
            .store(self.drift_ppm.to_bits(), Ordering::Relaxed);
    }
}

/// Clock frame (ducked, with any calibration click), live extra-output
/// mappers, path lines, the unaligned tap sum, and the tick.  Each mapper
/// adds the tick after the ceiling; a zero tick writes nothing.
trait WriteFrame: FnMut(&DeviceFrame, &mut [DeviceMapper], usize, &PathLines, [f32; 4], f32) {}

impl<F> WriteFrame for F where
    F: FnMut(&DeviceFrame, &mut [DeviceMapper], usize, &PathLines, [f32; 4], f32)
{
}

pub(crate) fn route_frame(left: f32, right: f32, routes: &[[f32; 4]; 2]) -> [f32; 4] {
    let mut out = [0.0f32; 4];
    for (tap, value) in out.iter_mut().enumerate() {
        *value = left * routes[0][tap] + right * routes[1][tap];
    }
    out
}

#[cfg(test)]
mod tests;
