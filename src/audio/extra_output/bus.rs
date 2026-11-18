//! Per-extra output ring and bridge buffer.  The clock callback pushes the extra output's
//! finished device channels here (its own sum of the paths, aligned and
//! mapped); the extra output's own cpal stream drains them with the same
//! ring → resampler → drift loop the clock path uses, without the chain.
//!
//! A device of up to 4 channels gets twice its lanes: the played frame,
//! then the click-free frame a take records ([`carries_record`]).  The
//! layout is fixed when the ring is laid out for the device.  Past 4
//! channels both would not fit in [`FRAME_LANES`], so the ring carries
//! the played frame only and that extra output's take keeps the click.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use super::super::output_map::{frame_lanes, DeviceFrame, FRAME_LANES};
use super::super::ring::{FrameRing, LaneFrame};
use super::super::types::BufferMode;

/// Whether a `channels`-channel extra output's ring also carries the
/// click-free lanes: both halves fit in [`FRAME_LANES`] (4 channels or
/// fewer).
pub(crate) fn carries_record(channels: usize) -> bool {
    frame_lanes(channels) * 2 <= FRAME_LANES
}

/// Ring lanes per frame for a `channels`-channel extra output: its
/// [`frame_lanes`], doubled when it [`carries_record`].
pub(crate) fn ring_lanes(channels: usize) -> usize {
    let lanes = frame_lanes(channels);
    if carries_record(channels) {
        lanes * 2
    } else {
        lanes
    }
}

/// One generic extra output's playback bus: SPSC ring plus its own bridge buffer.
pub(crate) struct ExtraOutputBus {
    /// [`ring_lanes`] per frame: one lane per device channel (see
    /// [`frame_lanes`]), then the click-free ones when it [`carries_record`].
    pub(crate) ring: FrameRing,
    /// The device's channel count, set before the stream opens; the clock
    /// side maps the extra output onto this many channels.
    channels: AtomicU32,
    /// Bumped whenever the ring is emptied or laid out anew.  The clock side
    /// reads it at the start of a block and stops pushing once it moves, so
    /// a long block never runs on into a reopened ring.
    generation: AtomicU64,
    pub(crate) target_frames: AtomicU64,
    pub(crate) base_frames: AtomicU64,
    /// Producer block size in source frames (the clock output's step).
    pub(crate) input_block: AtomicU64,
    /// Consumer block size in source frames (extra output device step, converted).
    pub(crate) output_block: AtomicU64,
    pub(crate) bridge_buffer_mode: AtomicU32,
    pub(crate) fixed_bridge_buffer_ms: AtomicU32,
    /// Rate the producer writes at (the clock output's output rate).
    pub(crate) source_rate: AtomicU32,
    /// Rate the extra output device plays at, published when its stream opens.
    /// 0 until then.  This is not [`Self::source_rate`]: a cable whose rate
    /// differs from the clock plays the resampled frame, and a recording
    /// of that endpoint has to use this rate.
    playback_rate: AtomicU32,
    /// Frames the extra output device asked for in its last callback: the
    /// buffer its driver really runs at, not the one requested.  0 until
    /// the stream's first callback.
    device_block: AtomicU32,
    pub(crate) dropouts: AtomicU64,
    pub(crate) overruns: AtomicU64,
    /// Stream open failed or the device vanished; the bridge keeps running.
    pub(crate) missing: AtomicBool,
    /// The drift loop's integral sits at its limit: the extra output's clock is
    /// further off than the loop can steer.  Written once per extra output block.
    pub(crate) drift_at_limit: AtomicBool,
}

impl ExtraOutputBus {
    pub(crate) fn new(sample_rate: u32) -> Self {
        Self {
            ring: FrameRing::new(sample_rate),
            channels: AtomicU32::new(2),
            generation: AtomicU64::new(0),
            target_frames: AtomicU64::new((sample_rate as u64 / 187).max(256)),
            base_frames: AtomicU64::new((sample_rate as u64 / 187).max(256)),
            input_block: AtomicU64::new(0),
            output_block: AtomicU64::new(0),
            bridge_buffer_mode: AtomicU32::new(BufferMode::Auto as u32),
            fixed_bridge_buffer_ms: AtomicU32::new(10.0f32.to_bits()),
            source_rate: AtomicU32::new(0),
            playback_rate: AtomicU32::new(0),
            device_block: AtomicU32::new(0),
            dropouts: AtomicU64::new(0),
            overruns: AtomicU64::new(0),
            missing: AtomicBool::new(false),
            drift_at_limit: AtomicBool::new(false),
        }
    }

    /// Clears ring counters and health for a fresh open.  Settings (bridge buffer)
    /// stay; the UI re-pushes them every frame.
    pub(crate) fn reset_playback(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.ring.reset();
        self.dropouts.store(0, Ordering::Relaxed);
        self.overruns.store(0, Ordering::Relaxed);
        self.input_block.store(0, Ordering::Relaxed);
        self.output_block.store(0, Ordering::Relaxed);
        self.device_block.store(0, Ordering::Relaxed);
        self.missing.store(false, Ordering::Relaxed);
        self.drift_at_limit.store(false, Ordering::Relaxed);
        self.lift_to_floor();
    }

    /// Sizes the ring for a `channels`-channel device and empties it.  Only
    /// while no stream drains it and the clock side does not feed it (the
    /// slot's ring-active flag is off).
    pub(crate) fn set_device_channels(&self, channels: usize) {
        let channels = channels.max(1);
        self.channels
            .store(channels.min(u32::MAX as usize) as u32, Ordering::Relaxed);
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.ring.set_lanes(ring_lanes(channels));
        self.lift_to_floor();
    }

    /// The ring's layout generation (see the field).
    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// The device channel count the ring is laid out for.
    pub(crate) fn device_channels(&self) -> usize {
        self.channels.load(Ordering::Relaxed) as usize
    }

    /// Producer side: one mapped device frame, with its click-free lanes
    /// after the played ones when the ring [`carries_record`].
    pub(crate) fn push_frame(&self, frame: &DeviceFrame) {
        let wet = frame.lanes();
        let count = wet.len();
        if count * 2 <= FRAME_LANES && carries_record(self.device_channels()) {
            let mut both = [0.0f32; FRAME_LANES];
            both[..count].copy_from_slice(wet);
            both[count..count * 2].copy_from_slice(frame.record_lanes());
            self.push_lanes(&both[..count * 2]);
        } else {
            self.push_lanes(wet);
        }
    }

    /// Producer side: one frame as lanes (missing lanes are silence).
    pub(crate) fn push_lanes(&self, lanes: &[f32]) {
        if !self.ring.push(lanes) {
            self.overruns.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn pop_frame(&self, frame: &mut LaneFrame) -> bool {
        self.ring.pop(frame)
    }

    pub(crate) fn ring_frames(&self) -> usize {
        self.ring.frames()
    }

    pub(crate) fn target_frames(&self) -> usize {
        self.target_frames.load(Ordering::Relaxed) as usize
    }

    /// The ring's fill against its bridge buffer, 0 to 1, as the clock's
    /// `Shared::ring_fill`.
    pub(crate) fn fill(&self) -> f32 {
        let target = self.target_frames().max(1) as f32;
        (self.ring_frames() as f32 / target).clamp(0.0, 1.0)
    }

    /// Extra output side, every callback: the frames the device asked for.
    pub(crate) fn note_device_block(&self, frames: usize) {
        self.device_block
            .store(frames.min(u32::MAX as usize) as u32, Ordering::Relaxed);
    }

    /// The frames the device asked for in its last callback (see the
    /// field).
    pub(crate) fn device_block(&self) -> u32 {
        self.device_block.load(Ordering::Relaxed)
    }

    pub(crate) fn set_bridge_buffer(&self, mode: BufferMode, fixed_ms: f32) {
        let mode_changed =
            self.bridge_buffer_mode.swap(mode as u32, Ordering::Relaxed) != mode as u32;
        let ms_changed = self
            .fixed_bridge_buffer_ms
            .swap(fixed_ms.to_bits(), Ordering::Relaxed)
            != fixed_ms.to_bits();
        if mode_changed || (ms_changed && mode == BufferMode::Fixed) {
            self.target_frames
                .store(self.floor_frames() as u64, Ordering::Relaxed);
        }
    }

    pub(crate) fn set_source_rate(&self, rate: u32) {
        self.source_rate.store(rate, Ordering::Relaxed);
        self.lift_to_floor();
    }

    /// Publishes the opened device rate (0 clears it).  The clock side's
    /// producer rate stays on [`Self::set_source_rate`].
    pub(crate) fn set_playback_rate(&self, rate: u32) {
        self.playback_rate.store(rate, Ordering::Relaxed);
    }

    /// The opened device rate, or 0 when no stream has published one.
    pub(crate) fn playback_rate(&self) -> u32 {
        self.playback_rate.load(Ordering::Relaxed)
    }

    pub(crate) fn set_target_frames(&self, frames: usize) {
        self.base_frames
            .store(frames.max(64) as u64, Ordering::Relaxed);
        self.target_frames
            .store(self.floor_frames() as u64, Ordering::Relaxed);
    }

    pub(crate) fn bridge_buffer_mode(&self) -> BufferMode {
        BufferMode::from_u32(self.bridge_buffer_mode.load(Ordering::Relaxed))
    }

    pub(crate) fn floor_frames(&self) -> usize {
        let base = self.base_frames.load(Ordering::Relaxed) as usize;
        let input = self.input_block.load(Ordering::Relaxed) as usize;
        let output = self.output_block.load(Ordering::Relaxed) as usize;
        let floor = match self.bridge_buffer_mode() {
            BufferMode::Auto => base.max(input).max(output * 2),
            BufferMode::Minimum => {
                if input + output == 0 {
                    base
                } else {
                    input + output
                }
            }
            BufferMode::Fixed => {
                let ms = f32::from_bits(self.fixed_bridge_buffer_ms.load(Ordering::Relaxed));
                let rate = match self.source_rate.load(Ordering::Relaxed) {
                    0 => 48_000,
                    rate => rate,
                };
                (ms.max(0.0) * rate as f32 / 1000.0).round() as usize
            }
        };
        floor.clamp(32, self.ring.capacity_frames() / 2)
    }

    pub(crate) fn lift_to_floor(&self) {
        let floor = self.floor_frames() as u64;
        let current = self.target_frames.load(Ordering::Relaxed);
        if self.bridge_buffer_mode() == BufferMode::Fixed {
            if current != floor {
                self.target_frames.store(floor, Ordering::Relaxed);
            }
        } else if current < floor {
            self.target_frames.store(floor, Ordering::Relaxed);
        }
    }

    pub(crate) fn note_input_block(&self, frames: usize) {
        if self.input_block.swap(frames as u64, Ordering::Relaxed) != frames as u64 {
            self.lift_to_floor();
        }
    }

    pub(crate) fn note_output_block(&self, frames: usize) {
        if self.output_block.swap(frames as u64, Ordering::Relaxed) != frames as u64 {
            self.lift_to_floor();
        }
    }

    pub(crate) fn raise_target_after_dropout(&self) {
        let step = match self.bridge_buffer_mode() {
            BufferMode::Auto => 256,
            BufferMode::Minimum => (self.output_block.load(Ordering::Relaxed) / 4).max(64),
            BufferMode::Fixed => return,
        };
        let floor = self.floor_frames() as u64;
        let current = self.target_frames.load(Ordering::Relaxed);
        let ceiling = (self.ring.capacity_frames() / 2) as u64;
        let next = (current + step).min(ceiling).max(floor);
        if next != current {
            self.target_frames.store(next, Ordering::Relaxed);
        }
    }

    pub(crate) fn relax_after(&self) -> Duration {
        match self.bridge_buffer_mode() {
            BufferMode::Minimum => Duration::from_secs(2),
            _ => Duration::from_secs(4),
        }
    }

    pub(crate) fn relax_target(&self) {
        let step = match self.bridge_buffer_mode() {
            BufferMode::Auto => 64,
            BufferMode::Minimum => 32,
            BufferMode::Fixed => return,
        };
        let floor = self.floor_frames() as u64;
        let current = self.target_frames.load(Ordering::Relaxed);
        if current > floor {
            self.target_frames
                .store(current.saturating_sub(step).max(floor), Ordering::Relaxed);
        }
    }
}
