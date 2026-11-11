//! Turning the ring into output frames, shared by every playback backend.
//!
//! The order, per frame:
//! 1. The clock side (ring, underrun recovery, drift trimming, rate
//!    conversion) runs once in [`OutputMixer`].  The PipeWire duplex path
//!    skips it and takes the frame straight from the graph buffers.
//! 2. The split: that one stereo frame goes into every live path slot
//!    ([`PathProcessor`]: routing, EQs, gate, plugin inserts).
//! 3. The path gain: level, mute, solo and a removed path's fade-out, as
//!    one ramp per path.  If any active path is soloed, the others fall
//!    silent.  A path that goes live while the mixer plays fades in from
//!    silence (only the mixer's first block adopts the gains outright), and
//!    a path whose plugins were restored but not opened yet stays silent
//!    rather than playing dry.  A removed path whose fade reached silence is
//!    moved to inactive here, at the end of the block.
//! 4. The master (level, mute, monitor) scales each path's frame, before
//!    the alignment: it is shared by every path, but a master change reaches
//!    a delayed path up to its alignment delay later on that device.  (Kept
//!    there so one path stays bit-identical to the chain before paths.)
//! 5. Alignment: each path's frame goes into that path's delay line; each
//!    device mapper reads every path through its own tap, at the path's
//!    alignment delay on that device (see `output_map`, "Alignment").  The
//!    chain latencies are read once per block.
//! 6. The sum: each device mapper maps every live path with that path's
//!    channel map, level and polarity on that device and adds them per
//!    channel, then applies the device level, the safety ceiling and the
//!    mute.  A plain stereo sum
//!    of the paths, after the master but not aligned, feeds the master
//!    meters and the scope.
//!    On the ring path, each generic follower's mapped frame goes into
//!    that follower's ring; its own stream plays it (see `follower`).
//!
//! The plugin chain of a path never drops out of the timing: a chunk whose
//! chain lock the UI holds (rare: only structural edits take it) plays the
//! chain's input delayed by its latency instead, so the sum keeps its
//! phase; that chunk loses the inserts' sound (see
//! [`PathProcessor::run_inserts`] and the `audio::path::plugins` module docs).
//!
//! Each path's chain (front, inserts, back) runs as one piece per block
//! and is timed with one monotonic clock read per path and stage (the
//! vDSO / QPC clock `Instant` reads, as the drift relax timer already
//! does; no allocation, no lock).  The smoothed time per block and the
//! block's length are published per path (`PathControls::dsp_time`, with a
//! peak that decays over [`path::DSP_PEAK_SECONDS`]) for Diagnostics.  The
//! sum of every path's time in the same block is smoothed and peaked the
//! same way and published as the total (`Shared::total_dsp`) for the Live
//! overload issue: its peak is the worst block, not the sum of the paths'
//! peaks, which may come from blocks seconds apart.  Both backends (the
//! ring and the PipeWire duplex graph) publish it, through `finish_block`.
//!
//! One path at 0 dB (path and device level), unmuted, normal polarity is
//! bit-identical to the chain before paths existed, while the sample stays
//! under the safety ceiling: every unity gain is a copy, not a multiply.

use std::sync::atomic::Ordering;
use std::time::Instant;

use super::calibration::CalibrationEmitter;
use super::delay::DelayLine;
use super::denormal::DenormalGuard;
use super::output_map::{
    DeviceFrame, DeviceMapper, PathLines, PathTiming, MAX_ALIGN_DELAY, MAX_FOLLOWERS,
};
use super::path::{LiveSlots, MAX_PATHS};
use super::resampler::FrameResampler;
use super::shared::Shared;

#[cfg(test)]
mod align_tests;
mod path;
pub(crate) mod ramp;

use path::{DspMeter, PathProcessor};

pub(crate) use ramp::Ramp;
use ramp::{fade_down, fade_up, CONTROL_RAMP_SECONDS, DROPOUT_FADE_SECONDS};

/// The most drift steering may trim the conversion ratio.  500 ppm is under
/// one cent of pitch.  Real clock offsets between two interfaces are a few
/// tens of ppm, so the loop normally runs far inside this limit.
const DRIFT_LIMIT_PPM: f32 = 500.0;
/// The same limit in ratio units, for the integral term.
const DRIFT_LIMIT_RATIO: f32 = DRIFT_LIMIT_PPM * 1e-6;
/// Time constant of the low-pass on the ring-fill error.  The fill is read
/// once per output block and jumps by a whole input block whenever the input
/// delivers, so the raw reading carries a sawtooth of up to one input block
/// that says nothing about the clocks.
const DRIFT_FILTER_SECONDS: f32 = 0.5;
/// Proportional gain, in ratio per second of filtered fill error (60 ppm per
/// millisecond).  Low enough that the filtered block sawtooth moves the ratio
/// by tens of ppm, not across the whole range.
const DRIFT_KP: f32 = 0.06;
/// Integral gain, in ratio per second² of filtered fill error.  With the
/// proportional gain the loop is damped (about 0.87) with a settling time of
/// a few tens of seconds: the integral learns the steady clock offset and the
/// ratio then moves only by a few ppm.
const DRIFT_KI: f32 = 0.0012;

/// Takes the next frame from the bridge.  An empty ring returns the previous
/// frame (so the resampler's window stays continuous) and marks the callback
/// as late, so the mixer fades out and the buffer can grow for the next round.
pub(crate) fn take_ring_frame(shared: &Shared, missed: &mut bool, last: (f32, f32)) -> (f32, f32) {
    match shared.pop_frame() {
        Some(pair) => pair,
        None => {
            *missed = true;
            last
        }
    }
}

/// Turns the ring into output frames for any playback path (cpal, ASIO,
/// REAL-style WASAPI or PipeWire): underrun recovery, drift trimming and
/// conversion from whatever rate the input really runs at (the clock side,
/// once), then each active signal path ([`PathProcessor`]), the sum, the
/// master level and mute, and last each output device's channel map, level
/// and mute.
pub(crate) struct OutputMixer {
    output_rate: u32,
    source_rate: u32,
    resampler: FrameResampler,
    last: (f32, f32),
    /// Playback gain around an underrun, 0..=1.  It is 1 while the ring
    /// delivers.  When the ring runs dry the last frame is held and this
    /// falls to 0 over [`DROPOUT_FADE_SECONDS`] (into the re-priming silence
    /// if the block ends first); when priming ends it rises back to 1 over
    /// the same time.  It starts at 1, so a ring that is already full on the
    /// first block plays straight away; a bridge that primes for a while
    /// fades in from that silence like any other recovery.  Both ends snap,
    /// so float rounding never leaves a finished fade a hair short of 0 or 1.
    fade_gain: f32,
    /// The frame held while fading out after an underrun.
    held: (f32, f32),
    /// Whether any ring frame has been played yet.  Only a ring that is full
    /// before the first frame starts at full level instead of fading in.
    played: bool,
    /// The ring ran dry in this block.  The rest of the block fades the held
    /// frame out instead of picking up whatever trickles in meanwhile; the
    /// next block re-primes as before.
    dropping: bool,
    priming: bool,
    started: bool,
    // Drift correction steers the resampler ratio from the ring-fill error
    // instead of dropping or repeating frames.  `drift_ppm` is the value the
    // resampler was last trimmed by, reported to the UI as the clock offset;
    // `drift_integral` is the slow part of the loop, which holds the learned
    // clock offset; `drift_error` is the low-passed fill error, `None` until
    // the first reading after priming.
    drift_ppm: f32,
    drift_integral: f32,
    drift_error: Option<f32>,
    /// Whether the last block went through the resampler.  Switching between
    /// it and the 1:1 bypass resets it, so it never replays stale history.
    resampling: bool,
    stable_since: Instant,
    /// One chain per path slot; only the `live` ones run.
    paths: [PathProcessor; MAX_PATHS],
    /// The slots that run in the current block, read once at its start.
    live: LiveSlots,
    /// One delay line per path slot: each live path's frame after the
    /// master is written here, and every device mapper reads it through its
    /// own alignment tap.
    lines: PathLines,
    /// The master: mute, monitor and the output level folded into one
    /// effective gain, ramped per sample so they compose without a step.
    gain: Ramp,
    /// The first block adopts the live master gain directly.
    gain_adopted: bool,
    /// The clock device's channel map, level and mute, after the master.
    device: DeviceMapper,
    /// Follower devices: stereo port pairs on the PipeWire duplex graph,
    /// or a generic follower's device channels, mapped here and pushed into
    /// its ring.  Only the first `live_followers` slots are written each
    /// block.
    followers: [DeviceMapper; MAX_FOLLOWERS],
    /// `shared.follower_count()` as read at the start of the block.
    live_followers: usize,
    /// The generic followers whose rings this block feeds (ring path only;
    /// see [`Shared::feeds_follower`]), with the ring's layout generation
    /// as read at the start of the block.
    feeding: [Option<u64>; MAX_FOLLOWERS],
    /// Every live path's DSP time per block together (`Shared::total_dsp`).
    total_dsp: DspMeter,
    /// The measured-round-trip duck and click on the clock output.  Only this
    /// callback touches it; the armed state itself is on `Shared`.
    calibration: CalibrationEmitter,
}

impl OutputMixer {
    /// A mixer for an output device with `device_channels` channels.
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
            paths: std::array::from_fn(|_| PathProcessor::new(output_rate)),
            live: LiveSlots::default(),
            lines: std::array::from_fn(|_| DelayLine::new(MAX_ALIGN_DELAY as usize)),
            gain: Ramp::new(1.0),
            gain_adopted: false,
            device: DeviceMapper::new(device_channels),
            // A PipeWire follower is a stereo port pair; a generic follower
            // takes its device's count when its ring starts.
            followers: std::array::from_fn(|_| DeviceMapper::new(2)),
            live_followers: 0,
            feeding: [None; MAX_FOLLOWERS],
            total_dsp: DspMeter::default(),
            calibration: CalibrationEmitter::new(),
        }
    }

    /// Publishes the output device's channel count.  Call when the stream
    /// opens, outside the callback, so the snapshot has it at once.
    pub(crate) fn publish_device_channels(&self, shared: &Shared) {
        self.device.publish_channels(&shared.clock_device);
    }

    /// Publishes each live follower's channel count as this mixer maps it:
    /// a stereo port pair on the PipeWire duplex graph.  (A generic
    /// follower's stream publishes its device's count when it opens.)
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn publish_follower_channels(&self, shared: &Shared) {
        let count = shared.follower_count();
        for index in 0..count {
            self.followers[index].publish_channels(&shared.followers[index]);
        }
    }

    /// Grows the per-block scratch of every path while the bridge is still
    /// opening, so the first callback (and the first block after a plugin or
    /// a path is added) never has to allocate.  A host that later hands
    /// over a larger block still works; that one block then grows the
    /// scratch.
    pub(crate) fn prepare(&mut self, frames: usize) {
        for path in &mut self.paths {
            path.prepare(frames);
        }
    }

    /// Follows the input's real rate.  It is only known once the input has
    /// opened, and an input that could not take the requested rate reports
    /// its own here.
    pub(crate) fn follow_source_rate(&mut self, shared: &Shared) {
        let source = match shared.source_rate.load(Ordering::Relaxed) {
            0 => self.output_rate,
            rate => rate,
        };
        if source != self.source_rate {
            self.source_rate = source;
            self.resampler.configure(source, self.output_rate);
            self.drift_ppm = 0.0;
            self.drift_integral = 0.0;
            self.drift_error = None;
        }
    }

    /// Steers the conversion ratio from the ring-fill error.  A fill above
    /// the target makes the resampler consume source frames a little faster,
    /// one below it a little slower; the integral term learns the steady
    /// clock offset.  Returns the applied correction in ppm, which is zero
    /// whenever drift is off or the ring is still priming.
    fn steer_drift(&mut self, shared: &Shared, target: usize, seconds: f32, drift: bool) -> f32 {
        if !drift {
            self.drift_ppm = 0.0;
            self.drift_integral = 0.0;
            self.drift_error = None;
            return 0.0;
        }
        if self.priming {
            // Nothing is drained while priming.  The learned clock offset in
            // the integral stays: the clocks have not changed, and keeping it
            // is what stops a small cushion from underrunning again.
            self.drift_ppm = 0.0;
            self.drift_error = None;
            return 0.0;
        }
        // Fill error as seconds of audio, so the gains do not depend on the
        // rate or the block size.  The loop works in ratio units and only
        // converts to ppm for the resampler and the readout.
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

    /// Picks up every UI change made since the last block: which slots
    /// are live, each live path's chain settings, the master gain and the
    /// device controls.  Makes room for `frames` in every live path.
    /// Shared by the ring path and the direct path.
    fn begin_block(&mut self, shared: &Shared, frames: usize) {
        let samples = CONTROL_RAMP_SECONDS * self.output_rate as f32;
        self.live = shared.live_slots();
        let any_solo = shared.any_solo();
        // Only the mixer's first block adopts the path gains outright.
        let fade_in = self.gain_adopted;
        let mut timing = PathTiming {
            latency: [0; MAX_PATHS],
            rate: self.output_rate,
        };
        for slot in self.live.iter() {
            let path = &mut self.paths[slot];
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
            }
            // Unclamped, so the report can flag a chain past MAX_ALIGN; the
            // alignment itself clamps.
            timing.latency[slot] = path.latency_raw;
            path.reserve_block(frames);
        }
        let silent = shared.mute.load(Ordering::Relaxed) || !shared.monitor.load(Ordering::Relaxed);
        let gain = if silent { 0.0 } else { shared.output_gain() };
        if self.gain_adopted {
            self.gain.set_target(gain, samples);
        } else {
            self.gain.snap(gain);
            self.gain_adopted = true;
        }
        self.device
            .begin_paths(&shared.clock_device, samples, self.live, &timing);
        // Every follower is summed and mapped here, like the clock device:
        // in-graph on the PipeWire duplex path, otherwise for a generic
        // follower's ring.  One without a running stream maps nothing and
        // its meters read 0.
        self.live_followers = shared.follower_count();
        let direct = shared.direct.load(Ordering::Relaxed);
        for index in 0..self.live_followers {
            let feeding = shared.feeds_follower(index);
            self.feeding[index] = feeding.then(|| shared.follower_bus(index).generation());
            let channels = if feeding {
                shared.follower_bus(index).device_channels()
            } else {
                2
            };
            self.followers[index].set_channels(channels);
            self.followers[index].begin_paths(
                &shared.followers[index],
                samples,
                self.live,
                &timing,
            );
            if !feeding && !direct {
                // Mapped, but nothing plays it: no alignment to show.  (The
                // duplex graph plays every follower in-graph.)
                shared.followers[index].clear_alignment();
            }
        }
        for follower in &shared.followers[self.live_followers..] {
            follower.clear_alignment();
        }
    }

    /// Splits one input frame into every live path.  Their chains run in
    /// [`Self::finish_block`].
    #[inline]
    fn stage_front(&mut self, index: usize, left: f32, right: f32) {
        for slot in self.live.iter() {
            self.paths[slot].stage_input(index, left, right);
        }
    }

    /// The chains for a block that `stage_front` filled: each live path's
    /// front (routing, pre EQ, PRE gate), inserts, POST gate and post EQ,
    /// each path timed (its DSP time, see [`PathProcessor::publish_dsp`]:
    /// one clock read per path and stage, no allocation, no lock) and the
    /// block's total over every path published, then the master level on
    /// each path's frame, the meters and the scope.  Each finished frame
    /// goes to `write` with the clock device's mapped frame (after its duck
    /// and any calibration click), the live followers' mappers, every live
    /// path's frame (the mappers sum them) and the stereo sum of the paths'
    /// taps, and the frame counter advances.
    fn finish_block(
        &mut self,
        shared: &Shared,
        frames: usize,
        now: Instant,
        mut write: impl WriteFrame,
    ) {
        let live = self.live;
        // Every front runs before any plugin can touch the float mode.
        let mut mark = Instant::now();
        for slot in live.iter() {
            self.paths[slot].run_front(frames);
            mark = self.paths[slot].add_dsp_time(mark, Instant::now());
        }
        for slot in live.iter() {
            self.paths[slot].run_inserts(&shared.paths[slot], frames);
            mark = self.paths[slot].add_dsp_time(mark, Instant::now());
        }

        // A plugin may have cleared flush-to-zero; the POST gate, post EQ
        // and the master still decay into silence and need it.  Dropped
        // before the caller's guard, so the thread's original mode still
        // comes back last.
        let _post_denormals = DenormalGuard::new();
        mark = Instant::now();
        let mut total_ns = 0u64;
        for slot in live.iter() {
            self.paths[slot].finish_block(&shared.paths[slot], frames);
            mark = self.paths[slot].add_dsp_time(mark, Instant::now());
            total_ns += self.paths[slot].publish_dsp(&shared.paths[slot], frames, self.output_rate);
        }
        // The total's peak is the peak of each block's sum: paths that peak
        // in different blocks must not add up.
        shared
            .total_dsp
            .store(self.total_dsp.push(total_ns, frames, self.output_rate));

        let followers = &mut self.followers[..self.live_followers];
        let mut peak_l = 0.0f32;
        let mut peak_r = 0.0f32;
        // An armed measurement ducks the clock output and plays its click
        // after the device map, so the fixed click level does not depend on
        // the master or the device fader.  The scope and meters see the same
        // duck, so the Live page shows the muted path while it measures.
        let (duck_target, duck_step) =
            self.calibration
                .begin(&shared.calibration, self.output_rate, now);
        for index in 0..frames {
            // Mute, monitor and the level are one ramp, so any of them can
            // move without a step.  It scales each path before the device
            // maps sum them, so one path is exactly `(x * gain) * weight`.
            let gain = self.gain.next();
            let (duck, click) = self
                .calibration
                .next(&shared.calibration, duck_target, duck_step);
            // The stereo sum for the meters and the scope.  It starts from
            // the first path's frame itself, so one path passes bit for bit.
            let mut sum: Option<[f32; 4]> = None;
            for slot in live.iter() {
                let path = &mut self.paths[slot];
                let mut out = path.block[index];
                // Path level, mute, solo and fade; unity copies.
                let path_gain = path.gain.next();
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
            let mapped = self.device.mapped_channels();
            let clock = self
                .device
                .map_paths(&self.lines)
                .ducked(duck, click, mapped);
            write(&clock, followers, index, &self.lines, out);
        }

        shared.frames.fetch_add(frames as u64, Ordering::Relaxed);
        shared.out_l.store(peak_l.to_bits(), Ordering::Relaxed);
        shared.out_r.store(peak_r.to_bits(), Ordering::Relaxed);
        self.device.end_block(&shared.clock_device);
        for index in 0..self.live_followers {
            self.followers[index].end_block(&shared.followers[index]);
        }
        shared.note_follower_producer_block(frames);
        // A removed path that has faded to silence keeps running until its
        // delay lines have played out on every device (the taps still hold
        // up to one alignment delay of it at full level), then stops from
        // the next block.
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
                // Went silent in this block: from here, the longest tap on
                // the clock device or a live follower still has to play out.
                None => self.followers[..self.live_followers]
                    .iter()
                    .map(|mapper| mapper.tap_delay(slot))
                    .fold(self.device.tap_delay(slot), u32::max),
            };
            self.paths[slot].drain = Some(drain);
            if drain == 0 {
                shared.paths[slot].finish_fade();
            }
        }
    }

    /// Like [`Self::render_direct`], but each finished frame is handed over
    /// once per live device: index 0 is the clock device, then each follower.
    /// Used by the PipeWire duplex/native multi-sink path.
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
            |clock, followers, index, paths, _| {
                write(0, index, clock);
                for (follower, mapper) in followers.iter_mut().enumerate() {
                    write(follower + 1, index, mapper.map_paths(paths));
                }
            },
        );
    }

    /// Follows the graph rate on the direct path, where input and output run
    /// in the same callback at one rate.  Cheap and allocation-free, so it can
    /// run on the audio thread: the EQ and gate redesign their coefficients on
    /// the next block and the control ramps take the new rate.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn set_direct_rate(&mut self, rate: u32) {
        let rate = rate.max(1);
        if rate == self.output_rate {
            return;
        }
        self.output_rate = rate;
        self.source_rate = rate;
        // The next block re-reads every path's EQs and gate and `update`
        // redesigns them for the new rate.
        for path in &mut self.paths {
            path.invalidate_settings();
        }
    }

    /// Renders one block straight from an input block that arrived in the
    /// same graph cycle: no ring, no cushion, no priming, no drift steering
    /// and no rate conversion, because both sides share one clock and rate.
    /// Everything after the ring read is the same chain as [`Self::render`].
    /// Each frame reaches `write` mapped onto the device's channels.
    #[allow(dead_code)] // Single-device direct path; duplex fans out via devices.
    pub(crate) fn render_direct(
        &mut self,
        shared: &Shared,
        input_left: &[f32],
        input_right: &[f32],
        mut write: impl FnMut(usize, &DeviceFrame),
    ) {
        self.render_direct_with(shared, input_left, input_right, |clock, _, index, _, _| {
            write(index, clock)
        });
    }

    /// [`Self::render_direct`] handing over the four mixer taps instead of
    /// the device frame.
    #[cfg(test)]
    fn render_direct_taps(
        &mut self,
        shared: &Shared,
        input_left: &[f32],
        input_right: &[f32],
        mut write: impl FnMut(usize, [f32; 4]),
    ) {
        self.render_direct_with(shared, input_left, input_right, |_, _, index, _, out| {
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
        // The same flush-to-zero mode as `render`, for the EQ, gate and
        // inserts of this block.
        let _denormals = DenormalGuard::new();
        // The block's clock stamp is taken here, at the callback's entry, so
        // the calibration measures from the frames' position and not from
        // when the inserts happened to finish.
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

    /// Renders `frames` output frames, handing each one to `write` mapped
    /// onto the device's channels.  The backend only copies the frame into
    /// its own buffer layout and sample format.
    pub(crate) fn render(
        &mut self,
        shared: &Shared,
        frames: usize,
        mut write: impl FnMut(usize, &DeviceFrame),
    ) {
        self.render_with(shared, frames, |clock, _, index, _, _| write(index, clock));
    }

    /// [`Self::render`] handing over the four mixer taps (only 0/1 carry the
    /// processed mix) instead of the device frame.
    #[cfg(test)]
    fn render_taps(
        &mut self,
        shared: &Shared,
        frames: usize,
        mut write: impl FnMut(usize, [f32; 4]),
    ) {
        self.render_with(shared, frames, |_, _, index, _, out| write(index, out));
    }

    fn render_with(&mut self, shared: &Shared, frames: usize, mut write: impl WriteFrame) {
        // Decaying EQ and gate states must not drop into the slow subnormal
        // range; the inserts run inside this block and get the same mode.
        let _denormals = DenormalGuard::new();
        let now = Instant::now();
        self.follow_source_rate(shared);
        self.begin_block(shared, frames);
        let drift = shared.drift_enabled.load(Ordering::Relaxed);
        // The ring holds input frames; one output block drains this many.
        // The very first block is skipped: a host fills its whole buffer once
        // at start-up, and sizing the cushion from that one block would keep
        // latency several times higher than the steady step needs.
        let block_source_frames =
            (frames as u64 * self.source_rate as u64).div_ceil(self.output_rate as u64) as usize;
        if self.started {
            shared.note_output_block(block_source_frames);
        }
        self.started = true;
        let target = shared.target_frames();

        // A smaller cushion chosen while playing takes effect at once instead
        // of being trimmed one frame per block.  Normal input bursts never
        // reach this far above the target.  The resampler is not reset here:
        // its window holds at most about a millisecond, so the purge becomes
        // a short crossfade through the kernel instead of a restart.
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

        // Do not drain the ring before the input has had time to fill it,
        // otherwise playback starts with a burst of underruns.
        // After an underrun the held frame must have faded out first (at most
        // DROPOUT_FADE_SECONDS), so a quick refill fades in from silence
        // instead of jumping from the half-faded held frame to new audio.
        // Only a ring that is full before anything played starts at full level.
        let faded = self.fade_gain == 0.0 || !self.played;
        if self.priming && faded && shared.ring_frames() >= target {
            self.priming = false;
        }

        // Steer the conversion ratio rather than splicing frames: the
        // resampler keeps running and its step is trimmed by a few ppm, so a
        // clock offset becomes a tiny pitch offset instead of a click.  With
        // drift off, the step is the exact base ratio.  A 1:1 bridge with
        // drift on still goes through the interpolator so that path exists
        // when the clocks start to differ.
        let seconds = frames as f32 / self.output_rate as f32;
        let drift_ppm = self.steer_drift(shared, target, seconds, drift);
        self.resampler.set_correction_ppm(drift_ppm);
        let needs_resample = self.resampler.active() || drift;
        if needs_resample != self.resampling {
            // Entering the resampler must not reuse history from before the
            // bypass; it restarts at the next ring frame.  Leaving it skips
            // the few frames it had fetched ahead (half its kernel), once,
            // when drift is switched off on a 1:1 bridge.
            self.resampler.reset();
            self.resampling = needs_resample;
        }

        let mut missed = false;
        let fade_step = 1.0 / (DROPOUT_FADE_SECONDS * self.output_rate as f32).max(1.0);
        for index in 0..frames {
            let (left, right) = if self.priming || self.dropping {
                // After an underrun: keep fading the held frame down, into
                // the priming silence if it outlasts this block.  A silent
                // held frame (or none yet) has nothing to fade.
                self.fade_gain = if self.held == (0.0, 0.0) {
                    0.0
                } else {
                    fade_down(self.fade_gain, fade_step)
                };
                let out = (self.held.0 * self.fade_gain, self.held.1 * self.fade_gain);
                // Once silent, resume as soon as the cushion is back, inside
                // this block, rather than waiting for the next one.
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
                    // The ring ran dry: hold this frame and fade it out
                    // from wherever the gain is, instead of holding it at
                    // full level and then dropping to silence.
                    missed = true;
                    self.dropping = true;
                    self.held = frame;
                    self.fade_gain = fade_down(self.fade_gain, fade_step);
                } else {
                    self.played = true;
                    if self.fade_gain < 1.0 {
                        // Back from an underrun: fade in instead of starting
                        // on a step.
                        self.fade_gain = fade_up(self.fade_gain, fade_step);
                    }
                }
                (frame.0 * self.fade_gain, frame.1 * self.fade_gain)
            };
            self.stage_front(index, left, right);
        }
        // Each generic follower gets its own sum of the paths, mapped onto
        // its device channels, through its ring (see `follower`).
        let feeding = self.feeding;
        self.finish_block(
            shared,
            frames,
            now,
            |clock, followers, index, paths, out| {
                write(clock, followers, index, paths, out);
                for (slot, mapper) in followers.iter_mut().enumerate() {
                    if let Some(generation) = feeding[slot] {
                        shared.push_follower_frame(slot, generation, mapper.map_paths(paths));
                    }
                }
            },
        );

        if missed {
            shared.underruns.fetch_add(1, Ordering::Relaxed);
            // Grow the target and rebuild the cushion instead of stuttering
            // on every callback.
            shared.raise_target_after_underrun();
            self.priming = true;
            // The held frame keeps fading through the priming silence.
            self.dropping = false;
            self.resampler.reset();
            // The integral (the learned clock offset) survives the underrun;
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

/// Receives one finished frame: the clock device's mapped frame (already
/// ducked and carrying any calibration click), the live followers' mappers,
/// the paths' delay lines (each holding the path's frame after the master as
/// its newest entry) for the mappers to align and sum, and the unaligned
/// stereo sum of the paths' four taps.
trait WriteFrame: FnMut(&DeviceFrame, &mut [DeviceMapper], usize, &PathLines, [f32; 4]) {}

impl<F> WriteFrame for F where
    F: FnMut(&DeviceFrame, &mut [DeviceMapper], usize, &PathLines, [f32; 4])
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
