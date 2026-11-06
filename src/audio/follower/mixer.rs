//! Playback for one generic follower: ring → resampler → drift → device.
//! No EQ, gate, plugins, master, sum or channel map — the clock device's
//! callback already ran those and pushed the follower's finished device
//! channels into its ring.  This side only crosses the clock: it follows the
//! fill, converts the rate and fades around an underrun, on every lane.

use std::sync::atomic::Ordering;
use std::time::Instant;

use super::super::denormal::DenormalGuard;
use super::super::mixer::ramp::{fade_down, fade_up, DROPOUT_FADE_SECONDS};
use super::super::output_map::{frame_lanes, DeviceFrame};
use super::super::resampler::FrameResampler;
use super::super::ring::LaneFrame;
use super::super::shared::Shared;

/// Same drift gains as [`super::super::mixer::OutputMixer`].
const DRIFT_LIMIT_PPM: f32 = 500.0;
const DRIFT_LIMIT_RATIO: f32 = DRIFT_LIMIT_PPM * 1e-6;
const DRIFT_FILTER_SECONDS: f32 = 0.5;
const DRIFT_KP: f32 = 0.06;
const DRIFT_KI: f32 = 0.0012;

const SILENCE: LaneFrame = [0.0; super::super::output_map::FRAME_LANES];

fn take_follower_frame(
    shared: &Shared,
    slot: usize,
    missed: &mut bool,
    last: &LaneFrame,
) -> LaneFrame {
    let mut frame = *last;
    if !shared.follower_bus(slot).pop_frame(&mut frame) {
        *missed = true;
    }
    frame
}

/// Turns one follower ring into device frames.
pub(crate) struct FollowerMixer {
    slot: usize,
    output_rate: u32,
    source_rate: u32,
    /// The device's channel count, and the ring lanes that carry them.
    channels: usize,
    lanes: usize,
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
    resampling: bool,
    stable_since: Instant,
}

impl FollowerMixer {
    pub(crate) fn new(slot: usize, output_rate: u32, device_channels: usize) -> Self {
        let output_rate = output_rate.max(1);
        let channels = device_channels.max(1);
        let lanes = frame_lanes(channels);
        let mut resampler = FrameResampler::new(output_rate, output_rate);
        resampler.set_lanes(lanes);
        Self {
            slot,
            output_rate,
            source_rate: output_rate,
            channels,
            lanes,
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
            resampling: false,
            stable_since: Instant::now(),
        }
    }

    /// Publishes the device's channel count for the UI (the meters come from
    /// the clock side's mapper for this slot).
    pub(crate) fn publish_channels(&self, shared: &Shared) {
        shared.followers[self.slot].set_channels(self.channels);
    }

    pub(crate) fn follow_source_rate(&mut self, shared: &Shared) {
        // Follower rings are fed at the clock device's output rate.
        let source = match shared.output_rate.load(Ordering::Relaxed) {
            0 => match shared
                .follower_bus(self.slot)
                .source_rate
                .load(Ordering::Relaxed)
            {
                0 => self.output_rate,
                rate => rate,
            },
            rate => rate,
        };
        if source != self.source_rate {
            self.source_rate = source;
            self.resampler.configure(source, self.output_rate);
            shared.follower_bus(self.slot).set_source_rate(source);
            self.drift_ppm = 0.0;
            self.drift_integral = 0.0;
            self.drift_error = None;
        }
    }

    fn steer_drift(&mut self, shared: &Shared, target: usize, seconds: f32, drift: bool) -> f32 {
        let bus = shared.follower_bus(self.slot);
        if !drift {
            self.drift_ppm = 0.0;
            self.drift_integral = 0.0;
            self.drift_error = None;
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

    /// Renders `frames` onto the follower device.
    pub(crate) fn render(
        &mut self,
        shared: &Shared,
        frames: usize,
        mut write: impl FnMut(usize, &DeviceFrame),
    ) {
        let _denormals = DenormalGuard::new();
        self.follow_source_rate(shared);
        let drift = shared.drift_enabled.load(Ordering::Relaxed);
        let block_source_frames =
            (frames as u64 * self.source_rate as u64).div_ceil(self.output_rate as u64) as usize;
        if self.started {
            shared
                .follower_bus(self.slot)
                .note_output_block(block_source_frames);
        }
        self.started = true;
        let target = shared.follower_bus(self.slot).target_frames();

        if !self.priming {
            let excess_line = target * 2 + block_source_frames;
            let fill = shared.follower_bus(self.slot).ring_frames();
            if fill > excess_line {
                let mut skipped = SILENCE;
                for _ in 0..fill - target {
                    if !shared.follower_bus(self.slot).pop_frame(&mut skipped) {
                        break;
                    }
                }
            }
        }

        let faded = self.fade_gain == 0.0 || !self.played;
        if self.priming && faded && shared.follower_bus(self.slot).ring_frames() >= target {
            self.priming = false;
        }

        let seconds = frames as f32 / self.output_rate as f32;
        let drift_ppm = self.steer_drift(shared, target, seconds, drift);
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
                    && shared.follower_bus(slot).ring_frames() >= target
                {
                    self.priming = false;
                }
                (self.held, self.fade_gain)
            } else {
                let last = self.last;
                let mut dry = false;
                let frame = if needs_resample {
                    self.resampler
                        .next(|| take_follower_frame(shared, slot, &mut dry, &last))
                } else {
                    take_follower_frame(shared, slot, &mut dry, &last)
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
            write(
                index,
                &DeviceFrame::from_lanes(&out[..lanes], self.channels),
            );
        }

        let bus = shared.follower_bus(self.slot);
        if missed {
            bus.underruns.fetch_add(1, Ordering::Relaxed);
            bus.raise_target_after_underrun();
            self.priming = true;
            self.dropping = false;
            self.resampler.reset();
            self.drift_ppm = 0.0;
            self.drift_error = None;
            self.stable_since = Instant::now();
        } else if !self.priming && self.stable_since.elapsed() >= bus.relax_after() {
            bus.relax_target();
            self.stable_since = Instant::now();
        }
    }
}
