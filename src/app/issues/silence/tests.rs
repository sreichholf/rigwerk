use std::time::{Duration, Instant};

use super::{
    SilenceInput, SilenceWatch, SILENCE_CLEAR_AFTER, SILENCE_RAISE_ALONE,
    SILENCE_RAISE_WITH_UNDERRUNS, SILENCE_REST_RESET,
};

const LOUD: f32 = 0.1; // −20 dBFS
const QUIET_SENT: f32 = 0.001; // −60 dBFS: a rest
const HEARD: f32 = 0.01; // −40 dBFS
const SILENT: f32 = 0.0001; // −80 dBFS

fn input(sent: f32, monitor: f32, underruns: bool) -> SilenceInput {
    SilenceInput {
        armed: true,
        sent,
        monitor: Some(monitor),
        parked: false,
        underruns,
    }
}

/// Feeds `input` in 50 ms ticks for `span`, returning the time after.
fn feed(watch: &mut SilenceWatch, input: SilenceInput, from: Instant, span: Duration) -> Instant {
    let tick = Duration::from_millis(50);
    let mut now = from;
    while now < from + span {
        now += tick;
        watch.update(input, now);
    }
    now
}

#[test]
fn loud_but_unheard_with_underruns_raises_after_the_short_hold() {
    let mut watch = SilenceWatch::default();
    let start = Instant::now();
    watch.update(input(LOUD, SILENT, true), start);
    let now = feed(
        &mut watch,
        input(LOUD, SILENT, true),
        start,
        SILENCE_RAISE_WITH_UNDERRUNS - Duration::from_millis(100),
    );
    assert!(!watch.silent());
    feed(
        &mut watch,
        input(LOUD, SILENT, true),
        now,
        Duration::from_millis(100),
    );
    assert!(watch.silent());
}

#[test]
fn without_underruns_it_needs_the_longer_hold() {
    let mut watch = SilenceWatch::default();
    let start = Instant::now();
    watch.update(input(LOUD, SILENT, false), start);
    let now = feed(
        &mut watch,
        input(LOUD, SILENT, false),
        start,
        SILENCE_RAISE_WITH_UNDERRUNS + Duration::from_millis(500),
    );
    assert!(!watch.silent());
    feed(
        &mut watch,
        input(LOUD, SILENT, false),
        now,
        SILENCE_RAISE_ALONE - SILENCE_RAISE_WITH_UNDERRUNS,
    );
    assert!(watch.silent());
}

#[test]
fn a_rest_is_not_silence_and_does_not_clear_it() {
    let mut watch = SilenceWatch::default();
    let start = Instant::now();
    watch.update(input(QUIET_SENT, SILENT, true), start);
    let now = feed(
        &mut watch,
        input(QUIET_SENT, SILENT, true),
        start,
        Duration::from_secs(10),
    );
    assert!(!watch.silent(), "both quiet: a rest");
    let now = feed(
        &mut watch,
        input(LOUD, SILENT, true),
        now,
        SILENCE_RAISE_WITH_UNDERRUNS,
    );
    assert!(watch.silent());
    feed(
        &mut watch,
        input(QUIET_SENT, SILENT, false),
        now,
        Duration::from_secs(10),
    );
    assert!(watch.silent(), "a rest proves nothing either way");
}

#[test]
fn a_long_rest_drops_the_evidence_and_a_short_one_keeps_it() {
    let mut watch = SilenceWatch::default();
    let start = Instant::now();
    let almost = SILENCE_RAISE_ALONE - Duration::from_millis(200);
    watch.update(input(LOUD, SILENT, false), start);
    let now = feed(&mut watch, input(LOUD, SILENT, false), start, almost);
    let now = feed(
        &mut watch,
        input(QUIET_SENT, SILENT, false),
        now,
        SILENCE_REST_RESET,
    );
    let now = feed(
        &mut watch,
        input(LOUD, SILENT, false),
        now,
        Duration::from_millis(500),
    );
    assert!(
        !watch.silent(),
        "loud spells a long rest apart do not add up"
    );

    let now = feed(&mut watch, input(LOUD, SILENT, false), now, almost);
    let now = feed(
        &mut watch,
        input(QUIET_SENT, SILENT, false),
        now,
        SILENCE_REST_RESET - Duration::from_millis(500),
    );
    feed(
        &mut watch,
        input(LOUD, SILENT, false),
        now,
        Duration::from_millis(500),
    );
    assert!(watch.silent(), "a short rest between notes keeps it");
}

#[test]
fn monitor_signal_resets_the_evidence_and_clears_after_a_second() {
    let mut watch = SilenceWatch::default();
    let start = Instant::now();
    watch.update(input(LOUD, SILENT, true), start);
    let now = feed(
        &mut watch,
        input(LOUD, SILENT, true),
        start,
        Duration::from_secs(1),
    );
    let now = feed(
        &mut watch,
        input(LOUD, HEARD, true),
        now,
        Duration::from_millis(50),
    );
    let now = feed(
        &mut watch,
        input(LOUD, SILENT, true),
        now,
        Duration::from_secs(1),
    );
    assert!(
        !watch.silent(),
        "heard in between: the evidence starts over"
    );
    let now = feed(
        &mut watch,
        input(LOUD, SILENT, true),
        now,
        Duration::from_secs(1),
    );
    assert!(watch.silent());

    let now = feed(
        &mut watch,
        input(LOUD, HEARD, false),
        now,
        SILENCE_CLEAR_AFTER - Duration::from_millis(100),
    );
    assert!(watch.silent(), "not heard long enough yet");
    feed(
        &mut watch,
        input(LOUD, HEARD, false),
        now,
        Duration::from_millis(100),
    );
    assert!(!watch.silent());
}

#[test]
fn it_is_inert_without_monitor_data_or_when_parked_or_disarmed() {
    let start = Instant::now();
    let cases = [
        SilenceInput {
            monitor: None,
            ..input(LOUD, SILENT, true)
        },
        SilenceInput {
            parked: true,
            ..input(LOUD, SILENT, true)
        },
        SilenceInput {
            armed: false,
            ..input(LOUD, SILENT, true)
        },
    ];
    for case in cases {
        let mut watch = SilenceWatch::default();
        watch.update(case, start);
        feed(&mut watch, case, start, Duration::from_secs(10));
        assert!(!watch.silent(), "{case:?}");
    }

    let mut watch = SilenceWatch::default();
    watch.update(input(LOUD, SILENT, true), start);
    let now = feed(
        &mut watch,
        input(LOUD, SILENT, true),
        start,
        Duration::from_secs(2),
    );
    assert!(watch.silent());
    let lost = SilenceInput {
        monitor: None,
        ..input(LOUD, SILENT, true)
    };
    watch.update(lost, now + Duration::from_millis(50));
    assert!(!watch.silent(), "no monitor data: never guess");
}

#[test]
fn a_stalled_tick_counts_for_little() {
    let mut watch = SilenceWatch::default();
    let start = Instant::now();
    watch.update(input(LOUD, SILENT, true), start);
    watch.update(input(LOUD, SILENT, true), start + Duration::from_secs(5));
    assert!(!watch.silent());
}
