use super::{amp_to_db, format_compact};

#[test]
fn amplitude_reads_as_decibels_and_counts_are_compact() {
    let twelve = 10.0f32.powf(-12.0 / 20.0);
    assert!((amp_to_db(twelve) + 12.0).abs() < 0.05);
    assert!((amp_to_db(1.0) - 0.0).abs() < 0.05);
    assert_eq!(amp_to_db(0.0), -60.0);
    assert_eq!(format_compact(999), "999");
    assert_eq!(format_compact(1_000), "1.0k");
    assert_eq!(format_compact(1_000_000), "1.0M");
}
