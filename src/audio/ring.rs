//! Lock-free stereo SPSC ring: one producer, one consumer, no allocation in
//! the push/pop path.  Used for the clock bridge (input → mixer) and for each
//! generic follower (finished mix → follower device).

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Interleaved stereo frames as `f32` bit patterns.  The producer alone writes
/// the slots at `head`; the consumer alone writes `tail`, so the two never
/// need a lock or touch the same index.
pub(crate) struct StereoRing {
    slots: Vec<AtomicU32>,
    head: AtomicU64,
    tail: AtomicU64,
    pub(crate) capacity_frames: usize,
}

impl StereoRing {
    /// Capacity is `sample_rate * 2` frames, floored at 4096 — the same sizing
    /// the clock bridge has always used.
    pub(crate) fn new(sample_rate: u32) -> Self {
        let capacity_frames = (sample_rate as usize * 2).max(4096);
        Self {
            slots: (0..capacity_frames * 2)
                .map(|_| AtomicU32::new(0))
                .collect(),
            head: AtomicU64::new(0),
            tail: AtomicU64::new(0),
            capacity_frames,
        }
    }

    /// Clears head and tail.  Only safe when no callback is touching the ring
    /// (fresh session, or between follower close and reopen).
    pub(crate) fn reset(&self) {
        self.head.store(0, Ordering::Relaxed);
        self.tail.store(0, Ordering::Relaxed);
    }

    /// Frames ready to read (`head - tail`), saturated at capacity.
    pub(crate) fn frames(&self) -> usize {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Relaxed);
        head.saturating_sub(tail).min(self.capacity_frames as u64) as usize
    }

    /// Producer side.  Returns `false` when the ring is full (caller counts an
    /// overrun and drops the frame).
    pub(crate) fn push(&self, left: f32, right: f32) -> bool {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        if head.wrapping_sub(tail) >= self.capacity_frames as u64 {
            return false;
        }
        let index = (head % self.capacity_frames as u64) as usize * 2;
        self.slots[index].store(left.to_bits(), Ordering::Relaxed);
        self.slots[index + 1].store(right.to_bits(), Ordering::Relaxed);
        self.head.store(head.wrapping_add(1), Ordering::Release);
        true
    }

    /// Consumer side.  `None` when empty.
    pub(crate) fn pop(&self) -> Option<(f32, f32)> {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        if tail == head {
            return None;
        }
        let index = (tail % self.capacity_frames as u64) as usize * 2;
        let left = f32::from_bits(self.slots[index].load(Ordering::Relaxed));
        let right = f32::from_bits(self.slots[index + 1].load(Ordering::Relaxed));
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        Some((left, right))
    }
}

#[cfg(test)]
mod tests {
    use super::StereoRing;

    #[test]
    fn push_pop_round_trips_stereo_frames() {
        let ring = StereoRing::new(48_000);
        assert!(ring.push(0.5, -0.25));
        assert!(ring.push(1.0, 0.0));
        assert_eq!(ring.frames(), 2);
        assert_eq!(ring.pop(), Some((0.5, -0.25)));
        assert_eq!(ring.pop(), Some((1.0, 0.0)));
        assert_eq!(ring.pop(), None);
        assert_eq!(ring.frames(), 0);
    }

    #[test]
    fn a_full_ring_refuses_the_next_push() {
        let ring = StereoRing::new(48_000);
        for _ in 0..ring.capacity_frames {
            assert!(ring.push(0.1, 0.2));
        }
        assert!(!ring.push(0.3, 0.4));
        assert_eq!(ring.frames(), ring.capacity_frames);
        assert_eq!(ring.pop(), Some((0.1, 0.2)));
        assert!(ring.push(0.3, 0.4));
    }
}
