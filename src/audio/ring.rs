//! Lock-free multi-lane SPSC ring.  No allocation on the push/pop path.
//! The clock callback pushes an extra output's finished channels; that
//! output's stream drains them.  The clock bridge's input ring lives in
//! `shared`.

use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use super::output_map::FRAME_LANES;

/// One [`FrameRing`] frame.  Only the ring's own lane count is live.
pub(crate) type LaneFrame = [f32; FRAME_LANES];

/// SPSC: the producer alone writes slots at `head`, the consumer alone
/// writes `tail`.  Samples are `f32` bits.  Storage is allocated once,
/// `sample_rate * 2` frames (at least 4096) of [`FRAME_LANES`], so two
/// seconds cover an Auto bridge buffer and the purge line above it at
/// eight channels and 192 kHz.
///
/// [`Self::set_lanes`] runs only while no callback touches the ring.  A
/// push or pop reads the count once, and every frame has room for
/// [`FRAME_LANES`], so a racing change can scramble samples but cannot
/// index past the storage.
pub(crate) struct FrameRing {
    slots: Vec<AtomicU32>,
    capacity_frames: usize,
    lanes: AtomicUsize,
    head: AtomicU64,
    tail: AtomicU64,
}

impl FrameRing {
    pub(crate) fn new(sample_rate: u32) -> Self {
        let capacity_frames = (sample_rate as usize * 2).max(4096);
        Self {
            slots: (0..capacity_frames * FRAME_LANES)
                .map(|_| AtomicU32::new(0))
                .collect(),
            capacity_frames,
            lanes: AtomicUsize::new(2),
            head: AtomicU64::new(0),
            tail: AtomicU64::new(0),
        }
    }

    /// Only while no callback is touching the ring.
    pub(crate) fn set_lanes(&self, lanes: usize) {
        self.lanes
            .store(lanes.clamp(1, FRAME_LANES), Ordering::Relaxed);
        self.reset();
    }

    pub(crate) fn lanes(&self) -> usize {
        self.lanes.load(Ordering::Relaxed)
    }

    /// Frame capacity.  It does not change with the lane count.
    pub(crate) fn capacity_frames(&self) -> usize {
        self.capacity_frames
    }

    /// Only while no callback is touching the ring.
    pub(crate) fn reset(&self) {
        self.head.store(0, Ordering::Relaxed);
        self.tail.store(0, Ordering::Relaxed);
    }

    pub(crate) fn frames(&self) -> usize {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Relaxed);
        head.saturating_sub(tail).min(self.capacity_frames() as u64) as usize
    }

    /// `false` when full: the caller counts an overrun and drops the frame.
    pub(crate) fn push(&self, frame: &[f32]) -> bool {
        let lanes = self.lanes();
        let capacity = self.capacity_frames as u64;
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        if head.wrapping_sub(tail) >= capacity {
            return false;
        }
        let index = (head % capacity) as usize * lanes;
        for (lane, slot) in self.slots[index..index + lanes].iter().enumerate() {
            let sample = frame.get(lane).copied().unwrap_or(0.0);
            slot.store(sample.to_bits(), Ordering::Relaxed);
        }
        self.head.store(head.wrapping_add(1), Ordering::Release);
        true
    }

    /// `false` when empty, and `frame` is left untouched.
    pub(crate) fn pop(&self, frame: &mut LaneFrame) -> bool {
        let lanes = self.lanes();
        let capacity = self.capacity_frames as u64;
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        if tail == head {
            return false;
        }
        let index = (tail % capacity) as usize * lanes;
        for (sample, slot) in frame.iter_mut().zip(&self.slots[index..index + lanes]) {
            *sample = f32::from_bits(slot.load(Ordering::Relaxed));
        }
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        true
    }
}

#[cfg(test)]
mod tests;
