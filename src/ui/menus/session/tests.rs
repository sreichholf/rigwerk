use super::{
    buffers_only_ms, session_buffer_text, session_detail, session_devices, session_facts,
    session_latency_text, session_list_scroll_height, session_list_shows_filter,
    session_matches_filter, session_rate_text, session_row, SessionAction, SESSION_ROW_H,
};
use crate::ui::widgets::{accesskit_frame, node_named, run_frame, test_input};
use eframe::egui::{self, accesskit::Toggled};

#[test]
fn a_session_row_exposes_its_name_and_delete_control() {
    let facts = "48 kHz · 64 smp · ≈ 2.7 ms";
    let tree = accesskit_frame(|ui| {
        assert_eq!(
            session_row(ui, "Clean", "Mic → Phones", facts, true, false, false),
            SessionAction::None
        );
    });
    let row = node_named(&tree, "Clean");
    assert_eq!(row.role(), egui::Role::Button);
    assert_eq!(
        row.value(),
        Some("Mic → Phones\n48 kHz · 64 smp · ≈ 2.7 ms")
    );
    assert_eq!(row.toggled(), Some(Toggled::True));
    let delete = node_named(&tree, "Delete");
    assert_eq!(delete.role(), egui::Role::Button);
    assert!(delete.toggled().is_none());
}

#[test]
fn confirming_delete_reads_delete_session() {
    let tree = accesskit_frame(|ui| {
        let _ = session_row(
            ui,
            "Clean",
            "Mic → Phones",
            "48 → 44.1 kHz · 64 / 128 smp · ≈ 14 ms",
            false,
            true,
            false,
        );
    });
    let delete = node_named(&tree, "Delete session");
    assert_eq!(delete.role(), egui::Role::Button);
    assert!(delete.toggled().is_none());
}

#[test]
fn load_confirm_is_part_of_the_row_text() {
    let detail = session_detail("Mic → Phones", true);
    let facts = "48 kHz · 64 smp · ≈ 2.7 ms";
    let tree = accesskit_frame(|ui| {
        let _ = session_row(ui, "Clean", &detail, facts, false, false, true);
    });
    let row = node_named(&tree, "Clean");
    assert_eq!(
        row.value(),
        Some("Mic → Phones\nLoad and lose changes\n48 kHz · 64 smp · ≈ 2.7 ms")
    );
}

#[test]
fn enter_on_a_focused_session_row_loads_it() {
    let ctx = egui::Context::default();
    let show = |ui: &mut egui::Ui| {
        session_row(
            ui,
            "Clean",
            "Mic → Phones",
            "48 kHz · 64 smp · ≈ 2.7 ms",
            false,
            false,
            false,
        )
    };
    let _ = run_frame(&ctx, test_input(None), |ui| {
        assert_eq!(show(ui), SessionAction::None);
    });
    let _ = run_frame(&ctx, test_input(Some(egui::Key::Tab)), |ui| {
        assert_eq!(show(ui), SessionAction::None);
    });
    let mut action = SessionAction::None;
    let _ = run_frame(&ctx, test_input(Some(egui::Key::Enter)), |ui| {
        action = show(ui);
    });
    assert_eq!(action, SessionAction::Load);
}

#[test]
fn rate_buffer_and_latency_text() {
    assert_eq!(session_devices("Mic", "Phones"), "Mic → Phones");
    assert_eq!(session_devices("Mic", ""), "Mic");
    assert_eq!(session_devices("", ""), "");
    assert_eq!(session_rate_text(48_000, 0), "48 kHz");
    assert_eq!(session_rate_text(48_000, 48_000), "48 kHz");
    assert_eq!(session_rate_text(44_100, 44_100), "44.1 kHz");
    assert_eq!(session_rate_text(48_000, 44_100), "48 → 44.1 kHz");
    assert_eq!(session_buffer_text(64, 0), "64 smp");
    assert_eq!(session_buffer_text(64, 64), "64 smp");
    assert_eq!(session_buffer_text(64, 128), "64 / 128 smp");
    assert_eq!(session_latency_text(7.6), "≈ 7.6 ms");
    assert_eq!(session_latency_text(10.0), "≈ 10 ms");
    assert_eq!(session_latency_text(13.2), "≈ 13 ms");

    let same = buffers_only_ms(48_000, 64, 0, 0, 10.0);
    let two_buffers = 2.0 * 64.0 / 48_000.0 * 1000.0;
    assert!((same - two_buffers).abs() < 1e-3);
    assert_eq!(session_latency_text(same), "≈ 2.7 ms");

    let followed = buffers_only_ms(48_000, 64, 48_000, 64, 40.0);
    assert!((followed - two_buffers).abs() < 1e-3);

    let sized = buffers_only_ms(48_000, 64, 0, 128, 10.0);
    let sized_expected = (64.0 + 128.0) / 48_000.0 * 1000.0;
    assert!((sized - sized_expected).abs() < 1e-3);

    let split = buffers_only_ms(48_000, 64, 44_100, 128, 10.0);
    let buffers = 64.0 / 48_000.0 * 1000.0 + 128.0 / 44_100.0 * 1000.0;
    assert!((split - buffers - 10.0).abs() < 1e-3);
    assert_eq!(
        session_facts(48_000, 44_100, 64, 128, split),
        "48 → 44.1 kHz · 64 / 128 smp · ≈ 14 ms"
    );

    let followed_rate = buffers_only_ms(48_000, 64, 44_100, 0, 6.0);
    let followed_expected = 64.0 / 48_000.0 * 1000.0 + 64.0 / 44_100.0 * 1000.0 + 6.0;
    assert!((followed_rate - followed_expected).abs() < 1e-3);
    assert_eq!(session_latency_text(followed_rate), "≈ 8.8 ms");
}

#[test]
fn the_list_filters_from_eight_and_scrolls_past_twelve() {
    assert!(!session_list_shows_filter(7));
    assert!(session_list_shows_filter(8));
    assert!(session_matches_filter("Room Sound", "room"));
    assert!(session_matches_filter("Room Sound", "  SOUND "));
    assert!(!session_matches_filter("Room", "vocal"));
    assert!(session_matches_filter("Room", "   "));
    assert_eq!(session_list_scroll_height(0), None);
    assert_eq!(session_list_scroll_height(12), None);
    assert_eq!(session_list_scroll_height(13), Some(SESSION_ROW_H * 12.0));
}
