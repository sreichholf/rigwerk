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
