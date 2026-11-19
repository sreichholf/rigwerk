use super::{
    device_list_scroll_height, device_matches_filter, device_row, filter_group, DEVICE_ROW_H,
    DEVICE_SCROLL_AFTER,
};
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
    assert_eq!(node.value(), Some("PIPEWIRE · 2 ch"));
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

#[test]
fn device_matches_filter_blank_and_whitespace_match_all() {
    assert!(device_matches_filter("Analog In", "PIPEWIRE", ""));
    assert!(device_matches_filter("Analog In", "PIPEWIRE", "   "));
}

#[test]
fn device_matches_filter_is_case_insensitive_substring() {
    assert!(device_matches_filter("Valeton GP-5", "ASIO", "valeton"));
    assert!(device_matches_filter("Valeton GP-5", "ASIO", "GP-5"));
    assert!(!device_matches_filter("Valeton GP-5", "ASIO", "scarlett"));
}

#[test]
fn device_matches_filter_checks_backend() {
    assert!(device_matches_filter("Analog In", "PIPEWIRE", "pipewire"));
    assert!(device_matches_filter("Analog In", "WASAPI", "wasapi"));
    assert!(!device_matches_filter("Analog In", "WASAPI", "asio"));
}

#[test]
fn device_matches_filter_supports_star_wildcards() {
    assert!(device_matches_filter("Valeton GP-5 USB", "ASIO", "val*usb"));
    assert!(device_matches_filter("Valeton GP-5 USB", "ASIO", "gp*5"));
    assert!(device_matches_filter("Valeton GP-5 USB", "ASIO", "*gp-5*"));
    assert!(device_matches_filter(
        "Valeton GP-5 USB",
        "ASIO",
        "valeton*"
    ));
    assert!(device_matches_filter("Valeton GP-5 USB", "ASIO", "*usb"));
    assert!(device_matches_filter("Valeton GP-5 USB", "ASIO", "*"));
    assert!(!device_matches_filter(
        "Valeton GP-5 USB",
        "ASIO",
        "scar*lett"
    ));
    assert!(!device_matches_filter(
        "Valeton GP-5 USB",
        "ASIO",
        "usb*valeton"
    ));
}

#[test]
fn device_matches_filter_trims_the_query() {
    assert!(device_matches_filter("Analog In", "PIPEWIRE", "  analog  "));
    assert!(device_matches_filter("Analog In", "PIPEWIRE", " *in "));
}

#[test]
fn device_list_scroll_height_is_none_until_past_the_cap() {
    assert_eq!(device_list_scroll_height(0, false, false), None);
    assert_eq!(
        device_list_scroll_height(DEVICE_SCROLL_AFTER, true, true),
        None
    );
    assert_eq!(
        device_list_scroll_height(DEVICE_SCROLL_AFTER + 1, false, false),
        Some(DEVICE_ROW_H * DEVICE_SCROLL_AFTER as f32)
    );
}

#[test]
fn device_list_scroll_height_keeps_room_for_group_headings() {
    let rows = DEVICE_ROW_H * DEVICE_SCROLL_AFTER as f32;
    assert_eq!(
        device_list_scroll_height(DEVICE_SCROLL_AFTER + 1, true, false),
        Some(rows + 18.0)
    );
    assert_eq!(
        device_list_scroll_height(DEVICE_SCROLL_AFTER + 1, true, true),
        Some(rows + 18.0 + 18.0 + 4.0)
    );
}

/// The UI pins In use above the scroll even when the filter would hide it.
/// The matcher itself does not special-case the selected device.
#[test]
fn device_matches_filter_does_not_keep_a_hidden_selected_name() {
    assert!(!device_matches_filter("Analog In", "PIPEWIRE", "scarlett"));
}

#[test]
fn filter_group_skips_the_pinned_index_and_applies_search() {
    let devices = vec![
        (0, item("Valeton")),
        (1, item("Scarlett")),
        (2, item("Analog In")),
    ];
    let peers: Vec<_> = devices.iter().map(|(_, device)| device.clone()).collect();
    let shown = filter_group(&devices, &peers, 0, "a");
    let indices: Vec<_> = shown.iter().map(|(index, _)| *index).collect();
    assert_eq!(indices, vec![1, 2]);
}
