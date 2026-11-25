use super::format_db;

#[test]
fn decibels_are_signed_with_a_unit() {
    assert_eq!(format_db(3.0, 1), "+3.0 dB");
    assert_eq!(format_db(-3.0, 1), "−3.0 dB");
    assert_eq!(format_db(0.0, 1), "0.0 dB");
    assert_eq!(format_db(-0.0, 1), "0.0 dB");
    assert_eq!(format_db(3.04, 1), "+3.0 dB");
    assert_eq!(format_db(-0.06, 1), "−0.1 dB");
    assert_eq!(format_db(-0.05, 1), "−0.1 dB");
    assert_eq!(format_db(-0.04, 1), "0.0 dB");
    assert_eq!(format_db(0.04, 1), "0.0 dB");
}

#[test]
fn silence_and_the_floor_keep_the_infinity_glyph() {
    assert_eq!(format_db(f32::NEG_INFINITY, 1), "−∞");
    assert_eq!(format_db(-120.0, 1), "−∞");
    assert_eq!(format_db(-121.0, 1), "−∞");
    assert_eq!(format_db(f32::NAN, 1), "−∞");
    assert_eq!(format_db(f32::INFINITY, 1), "+120.0 dB");
}

#[test]
fn the_precision_is_the_only_thing_a_caller_can_change() {
    assert_eq!(format_db(-24.0, 0), "−24 dB");
    assert_eq!(format_db(6.4, 0), "+6 dB");
    assert_eq!(format_db(0.4, 0), "0 dB");
    assert_eq!(format_db(-30.5, 0), "−31 dB");
    assert_eq!(format_db(0.0, 0), "0 dB");
    assert_eq!(format_db(1.25, 2), "+1.25 dB");
    assert_eq!(format_db(2.25, 1), "+2.3 dB");
}
