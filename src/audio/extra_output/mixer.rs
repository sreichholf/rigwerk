//! Playback for one generic extra output: ring → resampler → drift → device.
//! No EQ, gate, plugins, master, sum or channel map — the clock output's
//! callback already ran those and pushed the extra output's finished device
//! channels into its ring.  This side only crosses the clock: it follows the
//! fill, converts the rate and fades around a dropout, on every lane.
//! Those lanes include the click-free half when the ring carries one
//! ([`carries_record`]), and the frame is split back into both.  A wider
//! device has none, so its take records the click.
//!
//! What the Live Issues strip reads comes back through the slot's
//! [`ExtraOutputBus`](super::ExtraOutputBus) as atomics: the dropout count and
//! whether the drift integral is pinned at [`DRIFT_LIMIT_PPM`].  Closing a
//! large step of the target (a new bridge buffer) can wind the integral to its
//! limit with nothing wrong with the clock, so the pin is not published
//! while such a step explains it.

use std::sync::atomic::Ordering;
use std::time::Instant;

use super::super::denormal::DenormalGuard;
use super::super::mixer::ramp::{fade_down, fade_up, DROPOUT_FADE_SECONDS};
use super::super::output_map::{frame_lanes, DeviceFrame};
use super::super::resampler::FrameResampler;
use super::super::ring::LaneFrame;
use super::super::rt_guard::RtGuard;
use super::super::shared::Shared;
use super::bus::{carries_record, ring_lanes};

/// The shared loop's limit, re-exported for the Live Issues strip.
pub(crate) use super::super::mixer::drift::DRIFT_LIMIT_PPM;

use super::super::mixer::drift::{
    DRIFT_FILTER_SECONDS, DRIFT_KI, DRIFT_KP, DRIFT_LIMIT_RATIO, DRIFT_STEP_SECONDS,
};

const SILENCE: LaneFrame = [0.0; super::super::output_map::FRAME_LANES];

fn take_extra_output_frame(
    shared: &Shared,
    slot: usize,
    missed: &mut bool,
    last: &LaneFrame,
) -> LaneFrame {
    let mut frame = *last;
    if !shared.extra_output_bus(slot).pop_frame(&mut frame) {
        *missed = true;
    }
    frame
}

/// Turns one extra output ring into device frames.
pub(crate) struct ExtraOutputMixer {
    slot: usize,
    output_rate: u32,
    source_rate: u32,
    /// The device's channel count, and the ring lanes that carry them
    /// (see [`ring_lanes`]).
    channels: usize,
    lanes: usize,
    /// Whether the second half of those lanes is the click-free frame.
    /// Fixed when the extra output opens.
    split: bool,
    resampler: FrameResampler<LaneFrame>,
    last: LaneFrame,
    fade_gain: f32,
    held: LaneFrame,
    played: bool,
    dropping: bool,
    priming: bool,
    started: bool,
    drift_ppm: f32,
    drift_integral: f32,
    drift_error: Option<f32>,
    /// The target the loop steered to last block.
    steer_target: usize,
    /// The ring error (seconds) a large target step left, while the loop
    /// closes it: until the error crosses zero or outgrows the step.
    step_error: Option<f32>,
    resampling: bool,
    stable_since: Instant,
}

impl ExtraOutputMixer {
    pub(crate) fn new(slot: usize, output_rate: u32, device_channels: usize) -> Self {
        let output_rate = output_rate.max(1);
        let channels = device_channels.max(1);
        let lanes = ring_lanes(channels);
        let mut resampler = FrameResampler::new(output_rate, output_rate);
        resampler.set_lanes(lanes);
        Self {
            slot,
            output_rate,
            source_rate: output_rate,
            channels,
            lanes,
            split: carries_record(channels),
            resampler,
            last: SILENCE,
            fade_gain: 1.0,
            held: SILENCE,
            played: false,
            dropping: false,
            priming: true,
            started: false,
            drift_ppm: 0.0,
            drift_integral: 0.0,
            drift_error: None,
            steer_target: 0,
            step_error: None,
            resampling: false,
            stable_since: Instant::now(),
        }
    }

    /// Publishes the device's channel count for the UI (the meters come from
    /// the clock side's mapper for this slot).
    pub(crate) fn publish_channels(&self, shared: &Shared) {
        shared.extra_outputs[self.slot].set_channels(self.channels);
    }

    /// Follows the clock output's rate, which is what feeds this ring.  The
    /// worker that publishes the new rate plans the kernel; this callback only
    /// installs it, and plays the current pair until it is ready (a small
    /// pitch error for those blocks, not a rebuild inside the block).
    pub(crate) fn follow_source_rate(&mut self, shared: &Shared) {
        let source = self.followed_source(shared);
        if source == self.source_rate {
            return;
        }
        shared.extra_kernels[self.slot].try_adopt(&mut self.resampler);
        if self.resampler.pair() == (source, self.output_rate) {
            self.source_rate = source;
            shared.extra_output_bus(self.slot).set_source_rate(source);
            self.drift_ppm = 0.0;
            self.drift_integral = 0.0;
            self.drift_error = None;
            self.step_error = None;
        }
    }

    /// The rate the clock side feeds this ring at, as `Shared` publishes it.
    fn followed_source(&self, shared: &Shared) -> u32 {
        match shared.output_rate.load(Ordering::Relaxed) {
            0 => match shared
                .extra_output_bus(self.slot)
                .source_rate
                .load(Ordering::Relaxed)
            {
                0 => self.output_rate,
                rate => rate,
            },
            rate => rate,
        }
    }

    /// Builds the filter for the rate `Shared` already published.  Call it when
    /// the stream opens, before it drains the ring; a callback adopts a
    /// planned table instead.
    pub(crate) fn configure_source_rate(&mut self, shared: &Shared) {
        let source = self.followed_source(shared);
        if source == self.source_rate {
            return;
        }
        self.source_rate = source;
        self.resampler.configure(source, self.output_rate);
        shared.extra_output_bus(self.slot).set_source_rate(source);
        self.drift_ppm = 0.0;
        self.drift_integral = 0.0;
        self.drift_error = None;
        self.step_error = None;
    }

    /// For tests: how many kernels the resampler built.
    #[cfg(test)]
    pub(crate) fn configure_calls(&self) -> usize {
        self.resampler.configure_calls()
    }

    /// The rate pair the resampler is playing, for tests.
    #[cfg(test)]
    pub(crate) fn resampler_pair(&self) -> (u32, u32) {
        self.resampler.pair()
    }

    fn steer_drift(&mut self, shared: &Shared, target: usize, seconds: f32, drift: bool) -> f32 {
        let bus = shared.extra_output_bus(self.slot);
        if !drift {
            self.drift_ppm = 0.0;
            self.drift_integral = 0.0;
            self.drift_error = None;
            self.step_error = None;
            return 0.0;
        }
        if self.priming {
            self.drift_ppm = 0.0;
            self.drift_error = None;
            return 0.0;
        }
        let raw = (bus.ring_frames() as f32 - target as f32) / self.source_rate as f32;
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

    /// Follows a large step of the target outside priming (see
    /// [`DRIFT_STEP_SECONDS`]) until the ring error crosses zero, or grows
    /// past the step by more than `margin` seconds (the step no longer
    /// explains it), and returns whether the integral's pin is one the
    /// clock caused.  Call after [`Self::steer_drift`].
    fn drift_pinned(&mut self, shared: &Shared, target: usize, margin: f32) -> bool {
        let rate = self.source_rate as f32;
        let previous = std::mem::replace(&mut self.steer_target, target);
        if !self.priming
            && previous != 0
            && (target as f32 - previous as f32).abs() / rate >= DRIFT_STEP_SECONDS
        {
            let fill = shared.extra_output_bus(self.slot).ring_frames();
            self.step_error = Some((fill as f32 - target as f32) / rate);
        }
        if let (Some(step), Some(error)) = (self.step_error, self.drift_error) {
            if error.signum() != step.signum() || error.abs() > step.abs() + margin {
                self.step_error = None;
            }
        }
        self.drift_integral.abs() >= DRIFT_LIMIT_RATIO && self.step_error.is_none()
    }

    /// Renders `frames` onto the extra output device.
    pub(crate) fn render(
        &mut self,
        shared: &Shared,
        frames: usize,
        mut write: impl FnMut(usize, &DeviceFrame),
    ) {
        let _denormals = DenormalGuard::new();
        // Nothing in this block may allocate (see `audio::rt_guard`).
        let _no_allocations = RtGuard::enter();
        self.follow_source_rate(shared);
        let drift = shared.drift_enabled.load(Ordering::Relaxed);
        let block_source_frames =
            (frames as u64 * self.source_rate as u64).div_ceil(self.output_rate as u64) as usize;
        shared.extra_output_bus(self.slot).note_device_block(frames);
        if self.started {
            shared
                .extra_output_bus(self.slot)
                .note_output_block(block_source_frames);
        }
        self.started = true;
        let target = shared.extra_output_bus(self.slot).target_frames();

        if !self.priming {
            let excess_line = target * 2 + block_source_frames;
            let fill = shared.extra_output_bus(self.slot).ring_frames();
            if fill > excess_line {
                // The loop did not hold the ring: no step explains that.
                self.step_error = None;
                let mut skipped = SILENCE;
                for _ in 0..fill - target {
                    if !shared.extra_output_bus(self.slot).pop_frame(&mut skipped) {
                        break;
                    }
                }
            }
        }

        let faded = self.fade_gain == 0.0 || !self.played;
        if self.priming && faded && shared.extra_output_bus(self.slot).ring_frames() >= target {
            self.priming = false;
        }

        let seconds = frames as f32 / self.output_rate as f32;
        let drift_ppm = self.steer_drift(shared, target, seconds, drift);
        let margin = block_source_frames as f32 / self.source_rate as f32;
        let pinned = self.drift_pinned(shared, target, margin);
        shared
            .extra_output_bus(self.slot)
            .drift_at_limit
            .store(pinned, Ordering::Relaxed);
        self.resampler.set_correction_ppm(drift_ppm);
        let needs_resample = self.resampler.active() || drift;
        if needs_resample != self.resampling {
            self.resampler.reset();
            self.resampling = needs_resample;
        }

        let mut missed = false;
        let fade_step = 1.0 / (DROPOUT_FADE_SECONDS * self.output_rate as f32).max(1.0);
        let slot = self.slot;
        let lanes = self.lanes;
        for index in 0..frames {
            let (frame, gain) = if self.priming || self.dropping {
                self.fade_gain = if self.held[..lanes].iter().all(|value| *value == 0.0) {
                    0.0
                } else {
                    fade_down(self.fade_gain, fade_step)
                };
                if self.priming
                    && self.fade_gain == 0.0
                    && shared.extra_output_bus(slot).ring_frames() >= target
                {
                    self.priming = false;
                }
                (self.held, self.fade_gain)
            } else {
                let last = self.last;
                let mut dry = false;
                let frame = if needs_resample {
                    self.resampler
                        .next(|| take_extra_output_frame(shared, slot, &mut dry, &last))
                } else {
                    take_extra_output_frame(shared, slot, &mut dry, &last)
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
                (frame, self.fade_gain)
            };
            let mut out = frame;
            for value in &mut out[..lanes] {
                *value *= gain;
            }
            let mapped = if self.split {
                let (wet, record) = out[..lanes].split_at(frame_lanes(self.channels));
                DeviceFrame::from_both_lanes(wet, record, self.channels)
            } else {
                DeviceFrame::from_lanes(&out[..lanes], self.channels)
            };
            write(index, &mapped);
        }

        let bus = shared.extra_output_bus(self.slot);
        if missed {
            bus.dropouts.fetch_add(1, Ordering::Relaxed);
            bus.raise_target_after_dropout();
            self.priming = true;
            self.dropping = false;
            self.resampler.reset();
            self.drift_ppm = 0.0;
            self.drift_error = None;
            // Priming refills the ring to the new target: no step to close.
            self.step_error = None;
            self.stable_since = Instant::now();
        } else if !self.priming && self.stable_since.elapsed() >= bus.relax_after() {
            bus.relax_target();
            self.stable_since = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests;
