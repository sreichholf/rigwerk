use super::{output_summary, path_count_label};
use crate::app::blank_app;
use crate::app::cards::Card;

#[test]
fn the_strip_counts_paths_and_shows_the_alignment() {
    assert_eq!(path_count_label("Main", 3), "Main · 3 paths");
    assert_eq!(output_summary("+0.0 dB", 2.0), "+0.0 dB · +2.0 ms");
    let mut app = blank_app();
    assert_eq!(app.added_alignment_ms(), 0.0);
    assert!(app.apply_card(Card::Blend));
    let second = app.path_slots()[1];
    app.engine.screenshot_path_latency(second, 96);
    assert!((app.added_alignment_ms() - 2.0).abs() < 1e-3);
    app.set_device_align(0, false);
    assert_eq!(app.added_alignment_ms(), 0.0, "nothing waits");
    app.engine.screenshot_path_latency(second, 0);
}
