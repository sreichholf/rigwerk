use super::{screenshot_size, SCREENSHOT_INNER_SIZE};
use egui::Vec2;

#[test]
fn the_default_screenshot_size_stays_1440_by_1080() {
    assert_eq!(SCREENSHOT_INNER_SIZE, Vec2::new(1440.0, 1080.0));
    assert_eq!(screenshot_size(None), SCREENSHOT_INNER_SIZE);
    assert_eq!(screenshot_size(Some("1440x1080")), SCREENSHOT_INNER_SIZE);
}

#[test]
fn a_screenshot_size_overrides_the_default() {
    assert_eq!(screenshot_size(Some("1040x700")), Vec2::new(1040.0, 700.0));
    assert_eq!(
        screenshot_size(Some(" 1280X900 ")),
        Vec2::new(1280.0, 900.0)
    );
    assert_eq!(
        screenshot_size(Some("1040 x 700")),
        Vec2::new(1040.0, 700.0)
    );
}

#[test]
fn a_bad_screenshot_size_is_rejected() {
    for value in [
        "",
        "   ",
        "1440",
        "1440x",
        "x1080",
        "0x1080",
        "1440x0",
        "-1x1080",
        "1440x-5",
        "1440.5x1080",
        "wide",
        "1440x1080x2",
        "1440×1080",
        "x",
    ] {
        assert_eq!(
            screenshot_size(Some(value)),
            SCREENSHOT_INNER_SIZE,
            "{value:?}"
        );
    }
}
