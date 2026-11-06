//! Per-follower ring and cushion.  The clock callback pushes the finished
//! stereo mix here; the follower's own cpal stream drains it with the same
//! ring → resampler → drift loop the clock path uses, without the chain.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use super::super::ring::StereoRing;
use super::super::types::CushionMode;

/// One generic follower's playback bus: SPSC ring plus its own cushion.
pub(crate) struct FollowerBus {
    pub(crate) ring: StereoRing,
    pub(crate) target_frames: AtomicU64,
    pub(crate) base_frames: AtomicU64,
    /// Producer block size in source frames (the clock output's step).
    pub(crate) input_block: AtomicU64,
    /// Consumer block size in source frames (follower device step, converted).
    pub(crate) output_block: AtomicU64,
    pub(crate) cushion_mode: AtomicU32,
    pub(crate) manual_cushion_ms: AtomicU32,
    /// Rate the producer writes at (the clock device's output rate).
    pub(crate) source_rate: AtomicU32,
    pub(crate) underruns: AtomicU64,
    pub(crate) overruns: AtomicU64,
    /// Stream open failed or the device vanished; the bridge keeps running.
    pub(crate) missing: AtomicBool,
}

impl FollowerBus {
    pub(crate) fn new(sample_rate: u32) -> Self {
        Self {
            ring: StereoRing::new(sample_rate),
            target_frames: AtomicU64::new((sample_rate as u64 / 187).max(256)),
            base_frames: AtomicU64::new((sample_rate as u64 / 187).max(256)),
            input_block: AtomicU64::new(0),
            output_block: AtomicU64::new(0),
            cushion_mode: AtomicU32::new(CushionMode::Safe as u32),
            manual_cushion_ms: AtomicU32::new(10.0f32.to_bits()),
            source_rate: AtomicU32::new(0),
            underruns: AtomicU64::new(0),
            overruns: AtomicU64::new(0),
            missing: AtomicBool::new(false),
        }
    }

    /// Clears ring counters and health for a fresh open.  Settings (cushion)
    /// stay; the UI re-pushes them every frame.
    pub(crate) fn reset_playback(&self) {
        self.ring.reset();
        self.underruns.store(0, Ordering::Relaxed);
        self.overruns.store(0, Ordering::Relaxed);
        self.input_block.store(0, Ordering::Relaxed);
        self.output_block.store(0, Ordering::Relaxed);
        self.missing.store(false, Ordering::Relaxed);
        self.lift_to_floor();
    }

    pub(crate) fn push_frame(&self, left: f32, right: f32) {
        if !self.ring.push(left, right) {
            self.overruns.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn pop_frame(&self) -> Option<(f32, f32)> {
        self.ring.pop()
    }

    pub(crate) fn ring_frames(&self) -> usize {
        self.ring.frames()
    }

    pub(crate) fn target_frames(&self) -> usize {
        self.target_frames.load(Ordering::Relaxed) as usize
    }

    pub(crate) fn set_cushion(&self, mode: CushionMode, manual_ms: f32) {
        let mode_changed = self.cushion_mode.swap(mode as u32, Ordering::Relaxed) != mode as u32;
        let ms_changed = self
            .manual_cushion_ms
            .swap(manual_ms.to_bits(), Ordering::Relaxed)
            != manual_ms.to_bits();
        if mode_changed || (ms_changed && mode == CushionMode::Manual) {
            self.target_frames
                .store(self.floor_frames() as u64, Ordering::Relaxed);
        }
    }

    pub(crate) fn set_source_rate(&self, rate: u32) {
        self.source_rate.store(rate, Ordering::Relaxed);
        self.lift_to_floor();
    }

    pub(crate) fn set_target_frames(&self, frames: usize) {
        self.base_frames
            .store(frames.max(64) as u64, Ordering::Relaxed);
        self.target_frames
            .store(self.floor_frames() as u64, Ordering::Relaxed);
    }

    pub(crate) fn cushion_mode(&self) -> CushionMode {
        CushionMode::from_u32(self.cushion_mode.load(Ordering::Relaxed))
    }

    pub(crate) fn floor_frames(&self) -> usize {
        let base = self.base_frames.load(Ordering::Relaxed) as usize;
        let input = self.input_block.load(Ordering::Relaxed) as usize;
        let output = self.output_block.load(Ordering::Relaxed) as usize;
        let floor = match self.cushion_mode() {
            CushionMode::Safe => base.max(input).max(output * 2),
            CushionMode::LowLatency => {
                if input + output == 0 {
                    base
                } else {
                    input + output
                }
            }
            CushionMode::Manual => {
                let ms = f32::from_bits(self.manual_cushion_ms.load(Ordering::Relaxed));
                let rate = match self.source_rate.load(Ordering::Relaxed) {
                    0 => 48_000,
                    rate => rate,
                };
                (ms.max(0.0) * rate as f32 / 1000.0).round() as usize
            }
        };
        floor.clamp(32, self.ring.capacity_frames / 2)
    }

    pub(crate) fn lift_to_floor(&self) {
        let floor = self.floor_frames() as u64;
        let current = self.target_frames.load(Ordering::Relaxed);
        if self.cushion_mode() == CushionMode::Manual {
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

    pub(crate) fn raise_target_after_underrun(&self) {
        let step = match self.cushion_mode() {
            CushionMode::Safe => 256,
            CushionMode::LowLatency => (self.output_block.load(Ordering::Relaxed) / 4).max(64),
            CushionMode::Manual => return,
        };
        let floor = self.floor_frames() as u64;
        let current = self.target_frames.load(Ordering::Relaxed);
        let ceiling = (self.ring.capacity_frames / 2) as u64;
        let next = (current + step).min(ceiling).max(floor);
        if next != current {
            self.target_frames.store(next, Ordering::Relaxed);
        }
    }

    pub(crate) fn relax_after(&self) -> Duration {
        match self.cushion_mode() {
            CushionMode::LowLatency => Duration::from_secs(2),
            _ => Duration::from_secs(4),
        }
    }

    pub(crate) fn relax_target(&self) {
        let step = match self.cushion_mode() {
            CushionMode::Safe => 64,
            CushionMode::LowLatency => 32,
            CushionMode::Manual => return,
        };
        let floor = self.floor_frames() as u64;
        let current = self.target_frames.load(Ordering::Relaxed);
        if current > floor {
            self.target_frames
                .store(current.saturating_sub(step).max(floor), Ordering::Relaxed);
        }
    }
}
