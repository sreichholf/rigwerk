//! Ring, resampler, drift, dropout fade.  The clock callback already
//! mapped the frame.  With drift steering off and a matching rate, this
//! plays those samples bit for bit.
//!
//! A large step of the bridge-buffer target can pin the integral with
//! nothing wrong with the clock, so that pin is not published while the
//! step explains it.

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

pub(crate) struct ExtraOutputMixer {
    slot: usize,
    output_rate: u32,
    source_rate: u32,
    channels: usize,
    lanes: usize,
    /// Click-free lanes in the second half.  Fixed when the extra output
    /// opens.
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
    steer_target: usize,
    /// Ring error (seconds) left by a large target step, until it crosses
    /// zero or outgrows the step.
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

    /// The count only.  Meters come from the clock side's mapper.
    pub(crate) fn publish_channels(&self, shared: &Shared) {
        shared.extra_outputs[self.slot].set_channels(self.channels);
    }

    /// Installs a kernel planned off this thread.  Until it is ready the
    /// current pair plays, a small pitch error rather than a rebuild in
    /// the block.
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

    /// Before the stream drains the ring.  A callback adopts a planned
    /// table instead of building one.
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

    #[cfg(test)]
    pub(crate) fn configure_calls(&self) -> usize {
        self.resampler.configure_calls()
    }

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

    /// True when the pin is the clock's.  A step of at least
    /// [`DRIFT_STEP_SECONDS`] explains it until the error crosses zero or
    /// outgrows the step.  Call after [`Self::steer_drift`].
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

    pub(crate) fn render(
        &mut self,
        shared: &Shared,
        frames: usize,
        mut write: impl FnMut(usize, &DeviceFrame),
    ) {
        let _denormals = DenormalGuard::new();
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
