//! Nothing here opens a device.

use super::{describe, explain_device_in_use};
use crate::audio::output::FIXED_BUFFER_REFUSED;
use crate::audio::types::OutputMode;

#[test]
fn a_shared_open_that_kept_its_buffer_says_standard_output() {
    assert_eq!(
        describe(None, true),
        (OutputMode::Shared, "Standard output".to_owned())
    );
}

#[test]
fn a_refused_fixed_buffer_is_named_after_the_standard_line() {
    let (mode, line) = describe(None, false);
    assert_eq!(mode, OutputMode::Shared);
    assert_eq!(line, format!("Standard output{FIXED_BUFFER_REFUSED}"));
}

/// `app/driver_check` splits the published line on the prefix.  The prefix
/// has to survive byte for byte, so this reads the line the way that file
/// does.
#[test]
fn the_mode_line_carries_the_refusal_the_driver_check_parses() {
    let exclusive = " · Exclusive refused: the card is taken";
    let (mode, line) = describe(Some(exclusive), false);
    assert_eq!(mode, OutputMode::Shared);
    assert_eq!(
        line,
        format!("Standard output{FIXED_BUFFER_REFUSED}{exclusive}")
    );
    assert_eq!(
        line.split("Exclusive refused: ").nth(1),
        Some("the card is taken")
    );

    let smallest = " · Shared, smallest buffer refused: no step below 480";
    let (_, line) = describe(Some(smallest), true);
    assert_eq!(line.split("Exclusive refused: ").nth(1), None);
    assert_eq!(
        line.split("Shared, smallest buffer refused: ").nth(1),
        Some("no step below 480")
    );
}

#[test]
fn the_device_in_use_code_becomes_the_in_use_sentence() {
    let sentence = explain_device_in_use("Output device: -2004287478").expect("a known code");
    assert!(sentence.starts_with("This output is in use"), "{sentence}");
    assert_eq!(explain_device_in_use("-2004287478"), Some(sentence));
}

#[test]
fn an_unrelated_error_keeps_its_own_text() {
    assert_eq!(
        explain_device_in_use("Output device: no such endpoint"),
        None
    );
    assert_eq!(
        explain_device_in_use("BuildStream failed: -2004287477"),
        None
    );
}
