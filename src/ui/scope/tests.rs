use super::{
    amp_to_db, level_display_height, scope_badge, BRIDGE_LATENCY_LABEL, LEVEL_PREVIEW_LABEL,
    PATH_LATENCY_LABEL,
};

#[test]
fn level_preview_height_matches_the_peak_bar_scale() {
    assert!((level_display_height(1.0) - 1.0).abs() < 1.0e-5);
    let twelve = 10.0f32.powf(-12.0 / 20.0);
    assert!((amp_to_db(twelve) + 12.0).abs() < 0.05);
    assert!((level_display_height(twelve) - 0.75).abs() < 0.02);
    assert!(level_display_height(twelve) > twelve + 0.4);
    assert_eq!(level_display_height(0.0), 0.0);
}

#[test]
fn the_scope_latency_label_is_not_a_round_trip() {
    let label = BRIDGE_LATENCY_LABEL.to_ascii_uppercase();
    assert!(
        !label.contains("ROUND TRIP"),
        "{BRIDGE_LATENCY_LABEL} still claims a round trip"
    );
    assert_eq!(BRIDGE_LATENCY_LABEL, "US");
    assert_eq!(PATH_LATENCY_LABEL, "US · AFTER");
}

#[test]
fn the_scope_trace_is_labeled_as_a_scope() {
    assert_eq!(LEVEL_PREVIEW_LABEL, "SCOPE");
    assert_eq!(scope_badge(true), LEVEL_PREVIEW_LABEL);
    let active = scope_badge(true).to_ascii_uppercase();
    assert!(
        !active.contains("LIVE"),
        "active scope still claims a live signal"
    );
    assert!(!active.contains("FLOWING"));
    assert!(!active.contains("PREVIEW"));
    assert_eq!(scope_badge(false), "IDLE");
    assert!(!scope_badge(false).to_ascii_uppercase().contains("PAUSED"));
}
