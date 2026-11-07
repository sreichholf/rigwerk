use super::{
    input_channel_caption, input_channel_role, input_matrix_label, patch_count_label,
    routing_columns_stack, routing_top_row_height, MIX_COLUMNS,
};
use crate::ui::widgets::{accesskit_frame, ellipsize, node_named};
use eframe::egui::accesskit::Toggled;

fn input_row_caption(backend: &str, name: &str) -> String {
    ellipsize(&input_channel_caption(backend, name), 22)
}

#[test]
fn input_rows_name_the_selected_device() {
    let pipewire = input_channel_caption("PIPEWIRE", "Analog In");
    assert_eq!(pipewire, "PIPEWIRE · Analog In");
    assert!(!pipewire.to_ascii_uppercase().contains("ASIO"));
    assert!(!pipewire.to_ascii_lowercase().contains("guitar"));
    assert!(!pipewire.to_ascii_lowercase().contains("spare"));

    let mic = input_channel_caption("wasapi", "Microphone");
    assert_eq!(mic, "WASAPI · Microphone");
    assert!(!mic.contains("ASIO"));

    let asio = input_channel_caption("ASIO", "Focusrite USB");
    assert_eq!(asio, "ASIO · Focusrite USB");
    assert!(!asio.to_ascii_lowercase().contains("guitar"));
    assert!(!asio.to_ascii_lowercase().contains("spare"));
}

#[test]
fn a_long_device_name_stays_inside_the_row() {
    let shown = input_row_caption("PIPEWIRE", "Analog Input That Is Very Long");
    assert!(shown.starts_with("PIPEWIRE · "));
    assert!(shown.chars().count() <= 22);
    assert!(shown.contains('…'));
    assert!(!shown.to_ascii_uppercase().contains("ASIO"));
    assert!(!shown.to_ascii_lowercase().contains("guitar"));
}

#[test]
fn layer_one_feeds_mix_l_and_mix_r() {
    assert_eq!(input_matrix_label(2), "2 IN -> MIX L/R");
    assert_eq!(MIX_COLUMNS, ["MIX L", "MIX R"]);
    assert_eq!(input_channel_role(0), "Left");
    assert_eq!(input_channel_role(1), "Right");
}

#[test]
fn a_route_cell_exposes_its_name_and_enabled_value() {
    let tree = accesskit_frame(|ui| {
        assert!(!super::route_cell(ui, true, 0, 1));
        assert!(!super::route_cell(ui, false, 1, 0));
    });
    let node = node_named(&tree, "Route input 1 to Mix R");
    assert_eq!(node.role(), eframe::egui::Role::CheckBox);
    assert_eq!(node.value(), Some("enabled"));
    assert_eq!(node.toggled(), Some(Toggled::True));
    let off = node_named(&tree, "Route input 2 to Mix L");
    assert_eq!(off.value(), Some("muted"));
    assert_eq!(off.toggled(), Some(Toggled::False));
}

#[test]
fn the_patch_header_counts_devices_and_playing_channels() {
    assert_eq!(patch_count_label(1, 2), "1 DEVICE · 2 CHANNELS PLAYING");
    assert_eq!(patch_count_label(3, 1), "3 DEVICES · 1 CHANNEL PLAYING");
    assert_eq!(patch_count_label(1, 0), "1 DEVICE · 0 CHANNELS PLAYING");
}

#[test]
fn routing_columns_stack_below_1100_px() {
    assert!(routing_columns_stack(764.0));
    assert!(routing_columns_stack(1099.0));
    assert!(!routing_columns_stack(1100.0));
    assert!(!routing_columns_stack(1400.0));
}

#[test]
fn side_by_side_routing_cards_share_one_height() {
    let two = routing_top_row_height(2);
    let four = routing_top_row_height(4);
    assert!(two >= 300.0);
    assert!(four > two);
}
