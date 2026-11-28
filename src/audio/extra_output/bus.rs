//! Up to 4 channels the ring also carries click-free lanes for the take.
//! Wider devices do not fit both in [`FRAME_LANES`], so the take keeps
//! the click.  The ring is allocated with the bus (two seconds); the
//! bridge-buffer ceiling is half of that.
//!
//! The layout generation is read at the start of a clock block.  A reopen
//! stops that block from pushing further.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use super::super::output_map::{frame_lanes, DeviceFrame, FRAME_LANES};
use super::super::ring::{FrameRing, LaneFrame};
use super::super::types::BufferMode;

pub(crate) fn carries_record(channels: usize) -> bool {
    frame_lanes(channels) * 2 <= FRAME_LANES
}

pub(crate) fn ring_lanes(channels: usize) -> usize {
    let lanes = frame_lanes(channels);
    if carries_record(channels) {
        lanes * 2
    } else {
        lanes
    }
}

pub(crate) struct ExtraOutputBus {
    pub(crate) ring: FrameRing,
    /// The clock side maps onto this many channels.  Set before the stream
    /// opens.
    channels: AtomicU32,
    /// Bumped when the ring is emptied or laid out again.  The clock side
    /// reads it at the start of a block and stops pushing once it moves.
    generation: AtomicU64,
    pub(crate) target_frames: AtomicU64,
    pub(crate) base_frames: AtomicU64,
    /// Producer block, in source frames (the clock output's step).
    pub(crate) input_block: AtomicU64,
    /// Consumer block, in source frames (the device step, converted).
    pub(crate) output_block: AtomicU64,
    pub(crate) bridge_buffer_mode: AtomicU32,
    pub(crate) fixed_bridge_buffer_ms: AtomicU32,
    pub(crate) source_rate: AtomicU32,
    /// The device's opened rate, not [`Self::source_rate`].  0 until a
    /// stream publishes one.  A recording of this endpoint uses this rate.
    playback_rate: AtomicU32,
    /// The buffer the driver really runs, not the one requested.  0 until
    /// the first callback.
    device_block: AtomicU32,
    pub(crate) dropouts: AtomicU64,
    pub(crate) overruns: AtomicU64,
    /// Open failed or the device vanished.  The bridge keeps running.
    pub(crate) missing: AtomicBool,
    /// The integral is at its limit: the clock is further off than the loop
    /// can steer.
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

    /// Settings stay.  The UI re-pushes the bridge buffer every frame.
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

    /// Only while the slot's ring-active flag is off: nothing is draining
    /// or feeding the ring.
    pub(crate) fn set_device_channels(&self, channels: usize) {
        let channels = channels.max(1);
        self.channels
            .store(channels.min(u32::MAX as usize) as u32, Ordering::Relaxed);
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.ring.set_lanes(ring_lanes(channels));
        self.lift_to_floor();
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    pub(crate) fn device_channels(&self) -> usize {
        self.channels.load(Ordering::Relaxed) as usize
    }

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

    pub(crate) fn fill(&self) -> f32 {
        let target = self.target_frames().max(1) as f32;
        (self.ring_frames() as f32 / target).clamp(0.0, 1.0)
    }

    pub(crate) fn note_device_block(&self, frames: usize) {
        self.device_block
            .store(frames.min(u32::MAX as usize) as u32, Ordering::Relaxed);
    }

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

    /// 0 clears it.  The producer rate stays on [`Self::set_source_rate`].
    pub(crate) fn set_playback_rate(&self, rate: u32) {
        self.playback_rate.store(rate, Ordering::Relaxed);
    }

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
