use super::{
    device_summary_row, map_summary, output_cell, preset_active, selected_device,
    shows_unused_choice, shows_unused_choice_paths, MapEdit, RoutingAction,
};
use crate::app::cards::Card;
use crate::app::{blank_app, Page};
use crate::audio::{
    ChannelMap, EngineState, MapPreset, MixSide, OutputDeviceSettings, UnusedChannels,
};
use crate::ui::pages::mixer::chain::ChainSelection;
use crate::ui::pages::mixer::output_devices::OutputDeviceRow;
use crate::ui::theme::{AMBER, MUTED, RED};
use crate::ui::widgets::{accesskit_frame, node_named};
use eframe::egui::{self, accesskit::Toggled};

fn with_map(left: u8, right: u8, unused: UnusedChannels) -> OutputDeviceSettings {
    OutputDeviceSettings {
        path_map: crate::audio::PathMapSettings::first(ChannelMap { left, right }),
        unused,
        ..OutputDeviceSettings::default()
    }
}

fn text(settings: &OutputDeviceSettings, channels: usize) -> String {
    map_summary(settings, 0, channels).text
}

const SILENT: UnusedChannels = UnusedChannels::Silent;
const COPY: UnusedChannels = UnusedChannels::MonoCopy;

#[test]
fn the_default_map_reads_as_a_pair() {
    let default = OutputDeviceSettings::default();
    assert_eq!(text(&default, 2), "L/R");
    assert_eq!(text(&default, 4), "Out 1/2 · rest mono copy");
    assert_eq!(text(&with_map(0b01, 0b10, SILENT), 4), "Out 1/2");
    assert!(!map_summary(&default, 0, 2).nothing_routed);
}

#[test]
fn pair_presets_read_as_pairs() {
    let on4 = |preset| {
        let map = ChannelMap::default().with_preset(preset, 4);
        text(&with_map(map.left, map.right, SILENT), 4)
    };
    assert_eq!(on4(MapPreset::Stereo34), "Out 3/4");
    assert_eq!(on4(MapPreset::BothPairs), "Out 1/2 · Out 3/4");
    assert_eq!(on4(MapPreset::Swap), "Out 1/2 swapped");
    assert_eq!(
        on4(MapPreset::MonoToAll),
        "Out 1←mono · Out 2←mono · Out 3←mono · Out 4←mono"
    );
    assert_eq!(text(&with_map(0b10, 0b01, COPY), 2), "L/R swapped");
}

#[test]
fn single_channels_name_their_side() {
    assert_eq!(
        text(&with_map(0b001, 0b100, SILENT), 4),
        "Out 1←L · Out 3←R"
    );
    assert_eq!(
        text(&with_map(0b11, 0b10, SILENT), 2),
        "Out 1←L · Out 2←mono"
    );
    // Out 2/3 is not a pair: pairs start on odd outputs.
    assert_eq!(
        text(&with_map(0b010, 0b100, SILENT), 4),
        "Out 2←L · Out 3←R"
    );
}

#[test]
fn an_empty_map_says_what_plays_instead() {
    let empty = map_summary(&with_map(0, 0, SILENT), 0, 2);
    assert_eq!(empty.text, "nothing routed · plays silence");
    assert!(empty.nothing_routed);
    assert!(empty.silent, "the only case that warns");
    let copy = map_summary(&with_map(0, 0, COPY), 0, 4);
    assert_eq!(copy.text, "nothing routed · mono copy on all");
    assert!(copy.nothing_routed);
    assert!(!copy.silent, "mono copy still plays: no warning");
    assert!(!map_summary(&OutputDeviceSettings::default(), 0, 2).silent);
    // Bits past the device don't count.
    assert!(map_summary(&with_map(0b0100, 0b1000, SILENT), 0, 2).nothing_routed);
}

#[test]
fn a_mono_device_reads_mono_whenever_anything_is_routed() {
    assert_eq!(text(&OutputDeviceSettings::default(), 1), "Out 1←mono");
    assert_eq!(text(&with_map(0, 0b1000, SILENT), 1), "Out 1←mono");
    assert!(map_summary(&with_map(0, 0, SILENT), 0, 1).nothing_routed);
}

#[test]
fn preset_buttons_show_the_current_map() {
    let default = ChannelMap::default();
    assert!(preset_active(default, MapPreset::Stereo12, 2));
    assert!(!preset_active(default, MapPreset::Swap, 2));
    assert!(!preset_active(default, MapPreset::MonoToAll, 2));
    // One channel: any routed map plays (L + R) / 2.
    assert!(preset_active(default, MapPreset::MonoToAll, 1));
    assert!(preset_active(
        ChannelMap {
            left: 0,
            right: 0b100
        },
        MapPreset::MonoToAll,
        1
    ));
    let empty = ChannelMap { left: 0, right: 0 };
    assert!(!preset_active(empty, MapPreset::MonoToAll, 1));
    assert!(preset_active(empty, MapPreset::Clear, 1));
}

#[test]
fn the_unused_choice_shows_when_it_matters() {
    let default = OutputDeviceSettings::default();
    assert!(!shows_unused_choice(&default, 0, 2));
    assert!(shows_unused_choice(&default, 0, 4));
    assert!(shows_unused_choice(&with_map(0b01, 0, SILENT), 0, 2));
    assert!(!shows_unused_choice(&default, 0, 1));
    assert!(shows_unused_choice(&with_map(0, 0, SILENT), 0, 1));
}

#[test]
fn the_selection_falls_back_to_the_clock_device() {
    assert_eq!(selected_device(0, 1), 0);
    assert_eq!(
        selected_device(3, 1),
        0,
        "out of range falls back to the clock device"
    );
    assert_eq!(selected_device(1, 2), 1);
}

#[test]
fn open_in_mixer_selects_the_device_there() {
    let mut app = blank_app();
    app.page = Page::Routing;
    app.chain_selected = ChainSelection::PreEq;
    app.apply_routing_action(RoutingAction::Select(1));
    assert!(app.page == Page::Routing);
    assert_eq!(app.routing_device, 1);
    assert_eq!(
        app.chain_selected,
        ChainSelection::PreEq,
        "selecting here leaves the Mixer alone"
    );

    app.apply_routing_action(RoutingAction::OpenInMixer(0));
    assert!(app.page == Page::Mixer);
    assert_eq!(app.chain_selected, ChainSelection::OutputDevice(0));
}

#[test]
fn an_output_cell_exposes_its_name_and_value() {
    let tree = accesskit_frame(|ui| {
        assert!(!output_cell(ui, true, MixSide::L, 2));
        assert!(!output_cell(ui, false, MixSide::R, 0));
    });
    let on = node_named(&tree, "Mix L to Out 3");
    assert_eq!(on.role(), egui::Role::CheckBox);
    assert_eq!(on.toggled(), Some(Toggled::True));
    assert_eq!(on.value(), Some("routed"));
    let off = node_named(&tree, "Mix R to Out 1");
    assert_eq!(off.toggled(), Some(Toggled::False));
    assert_eq!(off.value(), Some("off"));
}

#[test]
fn a_device_row_reads_its_map() {
    let row = OutputDeviceRow {
        virtual_out: false,
        follower_index: None,
        missing: false,
        publish: false,
        name: "Scarlett 4i4".to_owned(),
        backend: "PipeWire".to_owned(),
        channels: 4,
        clock: true,
        settings: with_map(0b0101, 0b1010, SILENT),
    };
    let tree = accesskit_frame(|ui| {
        assert!(!device_summary_row(
            ui,
            &row,
            0,
            true,
            EngineState::Running,
            "in routing outputs"
        ));
    });
    let node = node_named(&tree, "Scarlett 4i4 in routing outputs");
    assert_eq!(node.role(), egui::Role::Button);
    assert_eq!(node.toggled(), Some(Toggled::True));
    assert_eq!(node.value(), Some("Out 1/2 · Out 3/4"));
}

#[test]
fn the_page_labels_presets_and_cells() {
    let mut app = blank_app();
    let tree = accesskit_frame(|ui| app.routing_page(ui));
    for name in [
        "Stereo preset",
        "Mono to all preset",
        "Swap L/R preset",
        "Clear preset",
        "Mix L to Out 1",
        "Mix R to Out 2",
        "Route input 1 to Mix L",
    ] {
        let _ = node_named(&tree, name);
    }
    let open = node_named(&tree, "Open in Mixer");
    assert_eq!(open.role(), egui::Role::Button);
}

fn has_node(tree: &egui::accesskit::TreeUpdate, name: &str) -> bool {
    tree.nodes
        .iter()
        .any(|(_, node)| node.label() == Some(name))
}

#[test]
fn one_path_keeps_the_page_plain() {
    let mut app = blank_app();
    let tree = accesskit_frame(|ui| app.routing_page(ui));
    let single = node_named(&tree, "Single path card");
    assert_eq!(single.toggled(), Some(Toggled::True));
    for card in ["Blend clean card", "Dual-amp card", "Dry L / Wet R card"] {
        assert!(!node_named(&tree, card).is_disabled(), "{card}");
    }
    for name in [
        "Mute Main",
        "Open Main in Mixer",
        "Duplicate Main",
        "Add path",
    ] {
        let _ = node_named(&tree, name);
    }
    for name in ["Remove Main", "Main L to Out 1", "Presets for Main"] {
        assert!(!has_node(&tree, name), "{name} only with several paths");
    }
}

#[test]
fn several_paths_get_a_row_pair_each_and_their_own_controls() {
    let mut app = blank_app();
    assert!(app.apply_card(Card::DryWet));
    let tree = accesskit_frame(|ui| app.routing_page(ui));
    assert_eq!(
        node_named(&tree, "Dry L / Wet R card").toggled(),
        Some(Toggled::True)
    );
    for name in [
        "Route input 1 to Main L",
        "Route input 1 to Path 2 R",
        "Path 2 R to Out 2",
        "Remove Path 2",
        "Solo Path 2",
    ] {
        let _ = node_named(&tree, name);
    }
    let on = node_named(&tree, "Main L to Out 1");
    assert_eq!(on.toggled(), Some(Toggled::True));
    let off = node_named(&tree, "Main R to Out 2");
    assert_eq!(off.toggled(), Some(Toggled::False));
    assert!(!has_node(&tree, "Mix L to Out 1"));
}

#[test]
fn path_actions_apply_after_drawing() {
    let mut app = blank_app();
    app.apply_routing_action(RoutingAction::AddPath);
    assert_eq!(app.path_count(), 2);
    let second = app.path_slots()[1];
    app.apply_routing_action(RoutingAction::PathMute(second, false));
    app.apply_routing_action(RoutingAction::PathSolo(second, true));
    app.apply_routing_action(RoutingAction::PathLevel(second, -3.0));
    let info = app.path_infos()[1].clone();
    assert!(!info.mute && info.solo);
    assert_eq!(info.level_db, -3.0);
    app.apply_routing_action(RoutingAction::DuplicatePath(second));
    assert_eq!(app.path_count(), 3);

    app.page = Page::Routing;
    app.chain_selected = ChainSelection::OutputDevice(0);
    app.apply_routing_action(RoutingAction::OpenPathInMixer(0));
    assert!(app.page == Page::Mixer);
    assert_eq!(app.selected_path, 0);
    assert_eq!(app.chain_selected, ChainSelection::PreEq);

    app.apply_routing_action(RoutingAction::PresetPath(second));
    assert_eq!(app.routing_preset_path, Some(second));
    assert_eq!(
        app.selected_path, 0,
        "presets leave the shared selection alone"
    );
    app.apply_routing_action(RoutingAction::RemovePath(second));
    assert_eq!(app.path_count(), 2);
    app.apply_routing_action(RoutingAction::ApplyCard(Card::Single));
    assert_eq!(app.current_card(), Some(Card::Single));
}

#[test]
fn device_rows_name_their_paths_and_warn() {
    let mut app = blank_app();
    assert!(app.apply_card(Card::DryWet));
    let infos = app.path_infos();
    let line = |app: &crate::app::RigWerkApp| {
        let rows = app.output_device_rows();
        app.device_paths_line(0, &rows[0], &infos)
    };
    // Aligned (the default): no warning.
    let aligned = line(&app);
    assert_eq!(aligned.text, "2 paths · 0.0 ms");
    assert_eq!(aligned.color, MUTED);
    assert_eq!(aligned.dots, vec![infos[0].colour, infos[1].colour]);

    // Unaligned, and the second path trimmed: they arrive apart.
    app.set_device_align(0, false);
    app.clock_device.path_map[infos[1].slot].trim_ms = 1.0;
    let shift = line(&app);
    assert_eq!(
        shift.text, "2 paths · 1.0 ms",
        "the trimmed path arrives last"
    );
    assert_eq!(shift.color, AMBER);

    // Both on L/R: they add up.
    app.clock_device.path_map[infos[0].slot].map = ChannelMap::default();
    app.clock_device.path_map[infos[1].slot].map = ChannelMap::default();
    let sum = line(&app);
    assert_eq!(sum.text, "unaligned sum · comb filter");
    assert_eq!(sum.color, RED);
    // Aligned again: the trim still pulls the sum apart ...
    app.set_device_align(0, true);
    assert_eq!(line(&app).color, RED);
    // ... and without it the same sum lines up.
    app.clock_device.path_map[infos[1].slot].trim_ms = 0.0;
    assert_ne!(line(&app).color, RED);

    // Only the second path plays here.
    app.clock_device.path_map[infos[0].slot].map = ChannelMap { left: 0, right: 0 };
    let one = line(&app);
    assert_eq!(one.text, "Path 2 · L/R");
    assert_eq!(one.dots, vec![infos[1].colour]);
}

#[test]
fn the_unused_choice_counts_every_path() {
    let mut settings = with_map(0b01, 0, SILENT);
    settings.path_map[1] = crate::audio::PathMapSettings::unrouted();
    assert!(shows_unused_choice_paths(&settings, &[0, 1], 2));
    settings.path_map[1].map = ChannelMap {
        left: 0,
        right: 0b10,
    };
    assert!(!shows_unused_choice_paths(&settings, &[0, 1], 2));
    assert!(shows_unused_choice_paths(&settings, &[0, 1], 4));
}

#[test]
fn sum_edits_write_their_own_slot_sanitized() {
    let mut app = blank_app();
    assert!(app.apply_card(Card::DryWet));
    let second = app.path_slots()[1];
    let before = app.clock_device.path_map[0];
    app.apply_map_edit(
        0,
        MapEdit::Level {
            slot: second,
            level_db: 40.0,
            commit: true,
        },
    );
    assert_eq!(app.clock_device.path_map[second].level_db, 6.0, "clamped");
    app.apply_map_edit(
        0,
        MapEdit::Invert {
            slot: second,
            invert: true,
        },
    );
    assert!(app.clock_device.path_map[second].invert);
    assert_eq!(app.clock_device.path_map[0], before, "other slot untouched");
    assert!(app
        .log
        .iter()
        .any(|entry| entry.text.contains("Path 2 level on")));
    assert!(app
        .log
        .iter()
        .any(|entry| entry.text.contains("Path 2 polarity on")));
}

#[test]
fn a_drag_logs_only_when_it_ends() {
    let mut app = blank_app();
    assert!(app.apply_card(Card::DryWet));
    let logged = app.log.len();
    for level_db in [-1.0, -2.0, -3.0] {
        app.apply_map_edit(
            0,
            MapEdit::Level {
                slot: 0,
                level_db,
                commit: false,
            },
        );
    }
    assert_eq!(app.log.len(), logged);
    assert_eq!(app.clock_device.path_map[0].level_db, -3.0);
}

#[test]
fn a_matrix_edit_makes_the_routing_custom() {
    let mut app = blank_app();
    assert!(app.apply_card(Card::DryWet));
    assert_eq!(app.current_card(), Some(Card::DryWet));
    app.apply_map_edit(
        0,
        MapEdit::Cell {
            slot: 0,
            side: MixSide::R,
            channel: 1,
            on: true,
        },
    );
    assert_eq!(app.current_card(), None);
    assert_eq!(app.routing_last_card, Some(Card::DryWet));
    assert_eq!(
        super::super::paths::custom_label(app.routing_last_card),
        "Custom · edited from Dry L / Wet R"
    );
}
