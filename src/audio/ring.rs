//! Lock-free multi-lane SPSC ring: one producer, one consumer, no allocation
//! in the push/pop path.  Each generic follower has one: the clock callback
//! pushes the follower's finished device channels, the follower's own stream
//! drains them.  (The clock bridge's input ring lives in `shared`.)

use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use super::output_map::FRAME_LANES;

/// One frame of a [`FrameRing`]: up to [`FRAME_LANES`] lanes, only the
/// ring's own lane count is used.
pub(crate) type LaneFrame = [f32; FRAME_LANES];

/// A multi-lane SPSC ring: frames of `lanes` samples each, as `f32` bit
/// patterns.  The producer alone writes the slots at `head`; the consumer
/// alone writes `tail`, so the two never need a lock or touch the same index.
///
/// The storage is allocated once, when it is created, for
/// `sample_rate * 2` frames (floored at 4096) of [`FRAME_LANES`] lanes: two
/// seconds at the engine rate whatever the device's channel count, so a
/// Safe cushion (two follower blocks) and the excess purge line above it
/// fit at eight channels and 192 kHz alike.  About 3.5 MB at 48 kHz.
///
/// The lane count changes only through [`Self::set_lanes`], while no
/// callback touches the ring.  Each push and pop reads it once and every
/// frame has room for all [`FRAME_LANES`] lanes, so even a racing change
/// could only scramble samples, never index past the storage.
pub(crate) struct FrameRing {
    slots: Vec<AtomicU32>,
    capacity_frames: usize,
    lanes: AtomicUsize,
    head: AtomicU64,
    tail: AtomicU64,
}

impl FrameRing {
    /// Storage for `sample_rate * 2` frames (floored at 4096, the sizing
    /// the clock bridge has always used) of every lane; starts with two
    /// lanes.
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

    /// Sets the lanes per frame (1..=[`FRAME_LANES`]) and empties the ring.
    /// Only safe when no callback is touching it.
    pub(crate) fn set_lanes(&self, lanes: usize) {
        self.lanes
            .store(lanes.clamp(1, FRAME_LANES), Ordering::Relaxed);
        self.reset();
    }

    pub(crate) fn lanes(&self) -> usize {
        self.lanes.load(Ordering::Relaxed)
    }

    /// Frames the ring holds, at any lane count.
    pub(crate) fn capacity_frames(&self) -> usize {
        self.capacity_frames
    }

    /// Clears head and tail.  Only safe when no callback is touching the ring.
    pub(crate) fn reset(&self) {
        self.head.store(0, Ordering::Relaxed);
        self.tail.store(0, Ordering::Relaxed);
    }

    /// Frames ready to read (`head - tail`), saturated at capacity.
    pub(crate) fn frames(&self) -> usize {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Relaxed);
        head.saturating_sub(tail).min(self.capacity_frames() as u64) as usize
    }

    /// Producer side: the first `lanes` samples of `frame` (missing ones are
    /// silence).  Returns `false` when the ring is full (the caller counts an
    /// overrun and drops the frame).
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

    /// Consumer side: fills the first `lanes` samples of `frame`.  `false`
    /// (and `frame` untouched) when empty.
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
mod tests {
    use super::{FrameRing, LaneFrame, FRAME_LANES};

    fn pop(ring: &FrameRing) -> Option<(f32, f32)> {
        let mut frame: LaneFrame = [0.0; FRAME_LANES];
        ring.pop(&mut frame).then_some((frame[0], frame[1]))
    }

    #[test]
    fn push_pop_round_trips_stereo_frames() {
        let ring = FrameRing::new(48_000);
        assert!(ring.push(&[0.5, -0.25]));
        assert!(ring.push(&[1.0, 0.0]));
        assert_eq!(ring.frames(), 2);
        assert_eq!(pop(&ring), Some((0.5, -0.25)));
        assert_eq!(pop(&ring), Some((1.0, 0.0)));
        assert_eq!(pop(&ring), None);
        assert_eq!(ring.frames(), 0);
    }

    #[test]
    fn a_full_ring_refuses_the_next_push() {
        let ring = FrameRing::new(48_000);
        assert_eq!(ring.capacity_frames(), 96_000);
        for _ in 0..ring.capacity_frames() {
            assert!(ring.push(&[0.1, 0.2]));
        }
        assert!(!ring.push(&[0.3, 0.4]));
        assert_eq!(ring.frames(), ring.capacity_frames());
        assert_eq!(pop(&ring), Some((0.1, 0.2)));
        assert!(ring.push(&[0.3, 0.4]));
    }

    #[test]
    fn a_frame_ring_carries_every_lane_it_was_set_up_for() {
        let ring = FrameRing::new(48_000);
        ring.set_lanes(6);
        assert_eq!(ring.capacity_frames(), 96_000);
        assert!(ring.push(&[0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 9.0]));
        // A short frame pads with silence.
        assert!(ring.push(&[1.0]));
        let mut frame: LaneFrame = [7.0; FRAME_LANES];
        assert!(ring.pop(&mut frame));
        assert_eq!(&frame[..6], &[0.1, 0.2, 0.3, 0.4, 0.5, 0.6]);
        // Lanes past the ring's count are left alone.
        assert_eq!(frame[6], 7.0);
        assert!(ring.pop(&mut frame));
        assert_eq!(&frame[..6], &[1.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        assert!(!ring.pop(&mut frame));
    }

    #[test]
    fn a_full_frame_ring_refuses_the_next_push() {
        let ring = FrameRing::new(48_000);
        ring.set_lanes(FRAME_LANES);
        let capacity = ring.capacity_frames();
        for _ in 0..capacity {
            assert!(ring.push(&[0.25; FRAME_LANES]));
        }
        assert!(!ring.push(&[0.5; FRAME_LANES]));
        assert_eq!(ring.frames(), capacity);
        // Changing the lane count starts over, empty.
        ring.set_lanes(2);
        assert_eq!(ring.frames(), 0);
        assert_eq!(ring.capacity_frames(), 96_000);
    }
}
