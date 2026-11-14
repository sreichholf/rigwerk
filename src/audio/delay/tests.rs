use super::{Declick, DelayLine, DelayTap};

#[test]
fn a_declick_starts_on_the_old_value_and_fades_to_the_new_signal() {
    let mut declick = Declick::default();
    declick.note(&[1.0, 1.0], &[-1.0, -1.0]);
    declick.start([0.0, 0.0], 4);
    assert_eq!(declick.apply([0.0, 0.0]), [1.0, -1.0]);
    assert_eq!(declick.apply([0.0, 0.0]), [0.75, -0.75]);
    declick.apply([0.0, 0.0]);
    declick.apply([0.0, 0.0]);
    assert!(!declick.is_active());
    assert_eq!(declick.apply([0.5, 0.5]), [0.5, 0.5]);
}

#[test]
fn a_line_delays_by_whole_frames_and_starts_silent() {
    let mut line = DelayLine::new(4);
    assert_eq!(line.max_delay(), 4);
    line.push([1.0, -1.0]);
    assert_eq!(line.read(0), [1.0, -1.0]);
    assert_eq!(line.read(1), [0.0, 0.0]);
    for value in 2..=10 {
        line.push([value as f32, 0.0]);
    }
    assert_eq!(line.read(0)[0], 10.0);
    assert_eq!(line.read(4)[0], 6.0);
    // Past the capacity: silence.
    assert_eq!(line.read(5)[0], 0.0);
    line.reset();
    assert_eq!(line.read(0), [0.0, 0.0]);
    line.push([3.0, 3.0]);
    assert_eq!(line.read(0), [3.0, 3.0]);
    assert_eq!(line.read(1), [0.0, 0.0]);
}

#[test]
fn an_empty_line_reads_silence() {
    let mut line = DelayLine::empty();
    line.push([1.0, 1.0]);
    assert_eq!(line.read(0), [0.0, 0.0]);
    assert_eq!(line.max_delay(), 0);
}

#[test]
fn a_tap_crossfades_to_a_new_delay_and_settles() {
    let mut line = DelayLine::new(64);
    let mut tap = DelayTap::new(0);
    let mut out = Vec::new();
    for index in 0..200 {
        // A ramp, so both read positions differ by a known amount.
        line.push([index as f32, 0.0]);
        if index == 50 {
            tap.set_target(10, 20.0);
        }
        out.push(tap.next(&line, 0)[0]);
    }
    assert_eq!(out[49], 49.0);
    // The fade moves from 0 to 10 frames behind over 20 samples: never a
    // jump bigger than the ramp's own step plus the fade's share.
    let largest = out
        .windows(2)
        .map(|pair| (pair[1] - pair[0]).abs())
        .fold(0.0f32, f32::max);
    assert!(largest <= 1.0, "largest step {largest}");
    assert!(tap.is_settled());
    assert_eq!(out[199], 189.0);
}

#[test]
fn a_settled_tap_no_longer_counts_the_delay_it_left() {
    let mut line = DelayLine::new(64);
    let mut tap = DelayTap::new(40);
    tap.set_target(4, 10.0);
    assert_eq!(tap.longest(), 40, "the old delay before the fade starts");
    line.push([0.0, 0.0]);
    tap.next(&line, 0);
    assert_eq!(tap.longest(), 40, "and while it runs");
    for _ in 0..20 {
        line.push([0.0, 0.0]);
        tap.next(&line, 0);
    }
    assert!(tap.is_settled());
    assert_eq!(tap.longest(), 4);
}

#[test]
fn a_change_during_a_fade_follows_once_it_ends() {
    let mut line = DelayLine::new(64);
    let mut tap = DelayTap::new(0);
    for index in 0..100 {
        line.push([index as f32, 0.0]);
        if index == 10 {
            tap.set_target(8, 10.0);
        }
        if index == 12 {
            tap.set_target(16, 10.0);
        }
        tap.next(&line, 0);
    }
    assert!(tap.is_settled());
    assert_eq!(tap.target(), 16);
    assert_eq!(tap.next(&line, 0)[0], 99.0 - 16.0);
}
