use super::{offset_to_show, rows_below, ROW_PITCH};

#[test]
fn a_row_in_view_keeps_the_scroll_where_it_is() {
    assert_eq!(offset_to_show(64.0, 92.0, 32.0, 96.0), 32.0);
    assert_eq!(offset_to_show(0.0, 28.0, 0.0, 96.0), 0.0);
}

#[test]
fn a_row_above_or_below_the_view_scrolls_just_into_it() {
    // Above: its top goes to the top edge.
    assert_eq!(offset_to_show(32.0, 60.0, 96.0, 96.0), 32.0);
    // Below: its bottom goes to the bottom edge.
    let top = 7.0 * ROW_PITCH;
    let bottom = top + 28.0;
    assert_eq!(offset_to_show(top, bottom, 0.0, 96.0), bottom - 96.0);
    // Never past the start.
    assert_eq!(offset_to_show(-4.0, 24.0, 10.0, 96.0), 0.0);
}

#[test]
fn the_fade_counts_the_rows_below_the_view() {
    let content = 10.0 * ROW_PITCH;
    assert_eq!(rows_below(content, 0.0, 3.0 * ROW_PITCH), 7);
    assert_eq!(
        rows_below(content, 0.0, 2.5 * ROW_PITCH),
        8,
        "a cut row counts"
    );
    assert_eq!(rows_below(content, 7.0 * ROW_PITCH, 3.0 * ROW_PITCH), 0);
    assert_eq!(rows_below(content, 0.0, content), 0);
}
