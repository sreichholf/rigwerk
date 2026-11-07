use super::device_row;
use crate::audio::DeviceItem;
use crate::ui::theme::TEAL;
use crate::ui::widgets::{accesskit_frame, node_named, run_frame, test_input};
use eframe::egui::{self, accesskit::Toggled};

fn item(name: &str) -> DeviceItem {
    DeviceItem {
        id: name.to_owned(),
        name: name.to_owned(),
        backend: "PIPEWIRE".to_owned(),
        channels: 2,
        demo: false,
        preferred: false,
        asio_clsid: None,
    }
}

#[test]
fn a_device_row_exposes_its_name_and_whether_it_is_in_use() {
    let tree = accesskit_frame(|ui| {
        assert!(!device_row(ui, &item("Analog In"), true, TEAL, &[]));
    });
    let node = node_named(&tree, "Analog In");
    assert_eq!(node.role(), egui::Role::RadioButton);
    assert_eq!(node.value(), Some("PIPEWIRE · 2 CH"));
    assert_eq!(node.toggled(), Some(Toggled::True));
}

#[test]
fn enter_on_a_focused_device_row_still_counts_as_a_pick() {
    let ctx = egui::Context::default();
    let device = item("Analog In");
    let _ = run_frame(&ctx, test_input(None), |ui| {
        assert!(!device_row(ui, &device, false, TEAL, &[]));
    });
    let _ = run_frame(&ctx, test_input(Some(egui::Key::Tab)), |ui| {
        assert!(!device_row(ui, &device, false, TEAL, &[]));
    });
    let mut picked = false;
    let _ = run_frame(&ctx, test_input(Some(egui::Key::Enter)), |ui| {
        picked = device_row(ui, &device, false, TEAL, &[]);
    });
    assert!(picked);
}
