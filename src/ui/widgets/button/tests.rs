use super::{OutlineButton, CONTROL_H};
use crate::ui::theme;
use crate::ui::widgets::{accesskit_frame, node_named, run_frame, test_input, TextField};
use eframe::egui;

#[test]
fn a_field_and_buttons_on_one_row_share_height_and_centre() {
    // The app's own style: its button padding is what made rows uneven.
    let ctx = egui::Context::default();
    theme::configure_style(&ctx);
    let mut rects = Vec::new();
    let mut text = "E:\\Guitar".to_owned();
    let _ = run_frame(&ctx, test_input(None), |ui| {
        ui.horizontal(|ui| {
            rects.push(
                TextField::new(&mut text, "Folder")
                    .width(300.0)
                    .show(ui)
                    .rect,
            );
            rects.push(OutlineButton::new("Paste").show(ui).rect);
            rects.push(OutlineButton::new("Browse").show(ui).rect);
        });
    });
    for rect in &rects {
        assert_eq!(rect.height(), CONTROL_H, "{rect:?}");
        assert_eq!(rect.center().y, rects[0].center().y, "{rects:?}");
    }
}

#[test]
fn a_glyph_button_is_named_and_shows_its_label_as_the_value() {
    let tree = accesskit_frame(|ui| {
        OutlineButton::new("×").name("Close").show(ui);
        OutlineButton::new("Copy").enabled(false).show(ui);
    });
    let close = node_named(&tree, "Close");
    assert_eq!(close.role(), egui::Role::Button);
    assert_eq!(close.value(), Some("×"));
    let copy = node_named(&tree, "Copy");
    assert!(copy.is_disabled());
    assert_eq!(copy.value(), None);
}

#[test]
fn a_full_width_button_shortens_its_label() {
    let ctx = egui::Context::default();
    let mut width = 0.0;
    let _ = run_frame(&ctx, test_input(None), |ui| {
        ui.allocate_ui(egui::vec2(80.0, 40.0), |ui| {
            let response = OutlineButton::new("In 1 + In 2 · stereo, a long source")
                .full_width()
                .show(ui);
            width = response.rect.width();
        });
    });
    assert!(width <= 80.0, "stays inside its column: {width}");
}
