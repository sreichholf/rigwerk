use super::{preset_row, PresetAction};
use crate::ui::widgets::{accesskit_frame, node_named, run_frame, test_input};
use eframe::egui::{self, accesskit::Toggled};

#[test]
fn a_preset_row_exposes_its_name_and_delete_control() {
    let tree = accesskit_frame(|ui| {
        assert_eq!(
            preset_row(
                ui,
                "Clean",
                "dry vocal",
                "mic -> phones",
                "48 kHz",
                true,
                false
            ),
            PresetAction::None
        );
    });
    let row = node_named(&tree, "Clean");
    assert_eq!(row.role(), egui::Role::Button);
    assert_eq!(row.value(), Some("dry vocal"));
    assert_eq!(row.toggled(), Some(Toggled::True));
    let delete = node_named(&tree, "Delete Clean");
    assert_eq!(delete.role(), egui::Role::Button);
    assert!(delete.toggled().is_none());
}

#[test]
fn enter_on_a_focused_preset_row_loads_it() {
    let ctx = egui::Context::default();
    let show = |ui: &mut egui::Ui| {
        preset_row(
            ui,
            "Clean",
            "dry vocal",
            "mic -> phones",
            "48 kHz",
            false,
            false,
        )
    };
    let _ = run_frame(&ctx, test_input(None), |ui| {
        assert_eq!(show(ui), PresetAction::None);
    });
    let _ = run_frame(&ctx, test_input(Some(egui::Key::Tab)), |ui| {
        assert_eq!(show(ui), PresetAction::None);
    });
    let mut action = PresetAction::None;
    let _ = run_frame(&ctx, test_input(Some(egui::Key::Enter)), |ui| {
        action = show(ui);
    });
    assert_eq!(action, PresetAction::Load);
}
