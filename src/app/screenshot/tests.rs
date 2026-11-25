//! Tests for the size parsing, the ending → exit-code table, the run deadline,
//! the ending latch and the capture deadline.  The rest of the frame loop
//! (settling, capture, close) draws a window, so it is not covered here.

use super::*;

use crate::app::blank_app;
use crate::ui::widgets::{run_frame, test_input};

#[test]
fn parses_a_size() {
    assert_eq!(parse_inner_size("1440x900"), Some(Vec2::new(1440.0, 900.0)));
    assert_eq!(parse_inner_size("1040X700"), Some(Vec2::new(1040.0, 700.0)));
    assert_eq!(
        parse_inner_size("  1440 x 900 \n"),
        Some(Vec2::new(1440.0, 900.0))
    );
}

#[test]
fn accepts_the_limits() {
    assert_eq!(parse_inner_size("640x480"), Some(MIN_INNER_SIZE));
    assert_eq!(parse_inner_size("7680x4320"), Some(MAX_INNER_SIZE));
}

#[test]
fn rejects_a_size_out_of_range() {
    for value in ["639x480", "640x479", "7681x4320", "7680x4321", "0x0"] {
        assert_eq!(parse_inner_size(value), None, "{value}");
    }
}

#[test]
fn rejects_what_is_not_a_size() {
    for value in [
        "",
        "1440",
        "1440x",
        "x900",
        "1440*900",
        "1440x900x2",
        "1440.5x900",
        "-1440x900",
        "widexhigh",
        "99999999999x900",
        "1440×1080",
    ] {
        assert_eq!(parse_inner_size(value), None, "{value}");
    }
}

#[test]
fn the_default_is_in_range() {
    assert_eq!(DEFAULT_INNER_SIZE, Vec2::new(1440.0, 1080.0));
    assert_eq!(parse_inner_size("1440x1080"), Some(DEFAULT_INNER_SIZE));
}

#[test]
fn an_unset_size_stays_the_default() {
    assert_eq!(inner_size_from(None), DEFAULT_INNER_SIZE);
}

#[test]
fn a_blank_or_rejected_size_falls_back_to_the_default() {
    for value in ["", "   ", "1x1", "1440", "1440×1080"] {
        assert_eq!(
            inner_size_from(Some(value)),
            DEFAULT_INNER_SIZE,
            "{value:?}"
        );
    }
}

#[test]
fn a_size_in_range_replaces_the_default() {
    assert_eq!(inner_size_from(Some("1040x700")), Vec2::new(1040.0, 700.0));
    assert_eq!(
        inner_size_from(Some(" 1280X900 ")),
        Vec2::new(1280.0, 900.0)
    );
}

#[test]
fn reads_numbered_files_and_skips_the_rest() {
    assert_eq!(
        parse_screenshot_files(
            "T",
            " 2=/a/Drive Pedal.nam ; ;x.nam; 2 = /a/Amp.nam;one=/b.nam"
        ),
        vec![
            (2, "/a/Drive Pedal.nam".to_owned()),
            (2, "/a/Amp.nam".to_owned())
        ]
    );
    assert!(parse_screenshot_files("T", "").is_empty());
}

#[test]
fn every_ending_has_its_own_exit_code() {
    assert_eq!(End::Saved.exit_code(), 0);
    assert_eq!(
        [
            End::ViewportMismatch.exit_code(),
            End::Timeout.exit_code(),
            End::SaveFailed.exit_code(),
        ],
        [1, 5, 3]
    );
    // `main` reports a usage error and a missing display with 2 before any
    // frame runs, so the capture deadline must not reuse it.
    assert_ne!(End::Timeout.exit_code(), 2);
    assert_eq!(EXIT_RUN_TIMED_OUT, 4);
}

#[test]
fn no_ending_is_not_an_exit_code() {
    assert_eq!(NO_ENDING, i32::MIN);
    for code in [
        End::Saved.exit_code(),
        End::ViewportMismatch.exit_code(),
        End::Timeout.exit_code(),
        End::SaveFailed.exit_code(),
        EXIT_RUN_TIMED_OUT,
    ] {
        assert_ne!(code, NO_ENDING);
    }
}

#[test]
fn the_run_deadline_sits_past_the_capture_deadline() {
    // The capture deadline needs live frames; the watchdog also bounds the
    // close, the drop and the engine stop.
    assert!(RUN_TIMEOUT > Duration::from_secs_f64(CAPTURE_TIMEOUT));
}

#[test]
fn the_first_ending_wins() {
    let exit = Arc::new(AtomicI32::new(NO_ENDING));
    let mut job = ScreenshotJob::new(PathBuf::from("shot.png"), Arc::clone(&exit));
    let ctx = egui::Context::default();
    job.end(&ctx, End::Timeout);
    job.end(&ctx, End::Saved);
    assert_eq!(exit.load(Ordering::Acquire), End::Timeout.exit_code());
}

#[test]
fn a_capture_that_never_arrives_ends_the_run_at_the_deadline() {
    let ctx = egui::Context::default();
    let exit = Arc::new(AtomicI32::new(NO_ENDING));
    let mut app = blank_app();
    // Skip the first frame's setup: this drives the deadline branch only.
    app.frames = 1;
    let mut job = ScreenshotJob::new(PathBuf::from("shot.png"), Arc::clone(&exit));
    job.requested = true;
    job.requested_at = Some(0.0);
    app.screenshot = Some(job);
    let mut input = test_input(None);
    input.time = Some(31.0);
    run_frame(&ctx, input, |_| app.handle_screenshot(&ctx));
    assert_eq!(exit.load(Ordering::Acquire), End::Timeout.exit_code());
    let job = app.screenshot.as_ref().expect("the job is still there");
    assert!(job.ended, "the deadline ended the run");
    assert_eq!(job.requested_at, None, "the ending cleared the request");
}
