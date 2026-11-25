use std::time::{Duration, Instant};

use super::Meters;

/// Two ticks in the same frame hit the floor, so the smoothing never runs
/// against an empty interval.
#[test]
fn two_ticks_in_a_row_report_the_floor() {
    let mut meters = Meters::new();
    meters.tick();
    let dt = meters.tick();
    assert!((dt - 0.001).abs() < 1e-6, "{dt}");
}

/// A stalled frame is capped, so one pass moves the meters by at most a tenth
/// of a second.
#[test]
fn a_long_gap_is_capped() {
    let mut meters = Meters::new();
    meters.last_tick = Instant::now() - Duration::from_secs(1);
    let dt = meters.tick();
    assert!((dt - 0.1).abs() < 1e-6, "{dt}");
}
