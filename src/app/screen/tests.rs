use super::{format_khz, format_ms};

#[test]
fn times_and_rates_follow_the_scale() {
    assert_eq!(format_ms(7.6), "7.6 ms");
    assert_eq!(format_ms(1.0), "1.0 ms");
    assert_eq!(format_ms(13.4), "13 ms");
    assert_eq!(format_khz(48_000), "48 kHz");
    assert_eq!(format_khz(44_100), "44.1 kHz");
}
