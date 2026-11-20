use super::*;

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
