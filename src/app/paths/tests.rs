use super::EffectKey;
use crate::app::blank_app;
use crate::audio::{
    ChannelMap, OutputDeviceSettings, PathMapSettings, PathState, PluginPref, MAX_PATHS,
};

#[test]
fn add_duplicate_and_remove_paths_in_the_app_and_the_engine() {
    let mut app = blank_app();
    assert_eq!(app.path_slots(), vec![0]);
    app.path_mut().mix.mono = true;
    app.clock_output.path_map[0].level_db = -3.0;
    app.push_paths_to_engine();

    let added = app.add_path().expect("a free slot");
    assert_eq!(added, 1);
    assert_eq!(app.selected_path, 1);
    assert!(app.path().mix.mute, "added muted");
    assert_eq!(app.clock_output.path_map[1], PathMapSettings::unrouted());
    assert_eq!(app.engine.path_state(1), PathState::Active);

    let copy = app.duplicate_path(0).expect("a free slot");
    assert_eq!(copy, 2);
    let path = app.paths[2].clone().unwrap();
    assert_eq!(path.name, "Main 2");
    assert!(path.mix.mono && path.mix.mute && !path.mix.solo);
    assert_ne!(path.colour, app.paths[0].as_ref().unwrap().colour);
    assert_eq!(app.clock_output.path_map[2].level_db, -3.0);
    assert!(app.engine.shared.path(2).muted());

    // Names stay unique (ignoring case); a path may recase its own.
    assert!(!app.rename_path(2, " main "));
    assert!(!app.rename_path(2, "  "));
    assert!(app.rename_path(0, "MAIN"));
    assert!(app.rename_path(0, "Main"));
    assert_eq!(app.paths[2].as_ref().unwrap().name, "Main 2");

    let infos = app.path_infos();
    assert_eq!(infos.len(), 3);
    assert!(infos[2].selected && !infos[2].audible);
    assert_eq!((infos[0].stages, infos[0].latency_ms), (0, 0.0));

    assert!(app.remove_path(0));
    assert!(app.paths[0].is_none());
    assert_eq!(app.engine.path_state(0), PathState::FadingOut);
    // It fades out where it played.
    assert_eq!(app.clock_output.path_map[0].level_db, -3.0);
    // No bridge runs, so the next service finishes the removal.
    app.engine.service_plugins();
    assert_eq!(app.engine.path_state(0), PathState::Inactive);
    assert!(app.remove_path(1));
    assert!(!app.remove_path(2), "the last path stays");
    assert_eq!(app.path_slots(), vec![2]);
    assert_eq!(app.path().name, "Main 2");
}

#[test]
fn paths_save_in_display_order_with_compacted_device_maps() {
    let mut app = blank_app();
    app.add_path();
    app.add_path();
    let map = ChannelMap {
        left: 0b0100,
        right: 0b1000,
    };
    app.clock_output.path_map[2].map = map;
    app.remove_path(1);
    app.engine.service_plugins();
    // A path added now takes the free slot 1 but is listed last.
    let last = app.add_path().expect("slot 1 is free");
    assert_eq!(last, 1);
    app.clock_output.path_map[1].level_db = -2.0;
    assert_eq!(app.path_slots(), vec![0, 2, 1]);
    assert_eq!(app.path_infos()[2].slot, 1);
    assert_eq!(app.paths[1].as_ref().unwrap().name, "Path 4");
    let (paths, order) = app.saved_paths();
    assert_eq!(order, vec![0, 2, 1]);
    assert_eq!(paths[1].name, "Path 3");
    assert_eq!(app.saved_selected_path(&order), 2);
    let (clock, _) = app.saved_devices(&order);
    assert_eq!(clock.path_map[1].map, map);
    assert_eq!(clock.path_map[2].level_db, -2.0);

    // Installing two of them reuses the live slots in display order;
    // the third fades out.
    app.install_paths(&paths[..2], clock, Vec::new(), 1);
    assert_eq!(app.path_slots(), vec![0, 2]);
    assert_eq!(app.selected_path, 2);
    assert_eq!(app.clock_output.path_map[2].map, map);
    assert_eq!(app.engine.path_state(1), PathState::FadingOut);
    // The fading path keeps its entry until it is silent.
    assert_eq!(app.clock_output.path_map[1].level_db, -2.0);
    assert!(app.path_count() < MAX_PATHS);
}

#[test]
fn a_removed_path_fades_where_it_played() {
    use crate::audio::OutputMixer;
    let mut app = blank_app();
    let copy = app.duplicate_path(0).unwrap();
    app.set_path_mute(copy, false);
    // Path 0 on Out 1, the copy on Out 2 only, inverted.
    app.clock_output.path_map[0].map = ChannelMap { left: 1, right: 0 };
    app.clock_output.path_map[copy] = PathMapSettings {
        map: ChannelMap { left: 0, right: 2 },
        invert: true,
        ..PathMapSettings::default()
    };
    app.push_paths_to_engine();
    app.push_devices_to_engine();
    let shared = std::sync::Arc::clone(&app.engine.shared);
    let mut mixer = OutputMixer::new(48_000, 2);
    mixer.prepare(256);
    let run = |mixer: &mut OutputMixer| {
        // Sized up front: the write closure runs inside the allocation guard.
        let mut out = [[0.0f32; 2]; 256];
        mixer.render_direct_devices(&shared, &[0.5; 256], &[0.5; 256], |_, index, mapped| {
            out[index] = [mapped.sample(0), mapped.sample(1)];
        });
        out.to_vec()
    };
    let steady = run(&mut mixer);
    assert_eq!(steady[255], [0.5, -0.5]);
    assert!(app.remove_path(copy));
    app.push_devices_to_engine();
    let mut out = run(&mut mixer);
    out.extend(run(&mut mixer));
    // Out 1 never picks the fading copy up, and Out 2 only fades.
    assert!(out.iter().all(|frame| frame[0] == 0.5), "{:?}", out[100]);
    assert!(out.iter().all(|frame| frame[1] <= 0.0 && frame[1] >= -0.5));
    assert_eq!(out.last().unwrap()[1], 0.0);
    assert_eq!(app.engine.path_state(copy), PathState::Inactive);
}

#[test]
fn names_stay_unique() {
    let mut app = blank_app();
    let copy = app.duplicate_path(0).unwrap();
    assert_eq!(app.paths[copy].as_ref().unwrap().name, "Main 2");
    let copy_of_copy = app.duplicate_path(copy).unwrap();
    assert_eq!(app.paths[copy_of_copy].as_ref().unwrap().name, "Main 3");
    app.rename_path(copy, "Path 4");
    let added = app.add_path().unwrap();
    assert_eq!(app.paths[added].as_ref().unwrap().name, "Path 5");
}

#[test]
fn selecting_another_path_resets_its_effect_selection() {
    use crate::app::chain::ChainSelection;
    let mut app = blank_app();
    app.engine
        .restore_plugins(0, vec![effect("A.vst3"), effect("B.vst3")]);
    let second = app.add_path().unwrap();
    assert!(app.select_path(0));
    app.chain_selected = ChainSelection::Plugin(1);
    app.plugin_effect_at = Some(1);
    assert!(app.select_path(second));
    assert_eq!(app.chain_selected, ChainSelection::Plugins);
    assert_eq!(app.plugin_effect_at, None);
    assert!(!app.select_path(3), "no path there");
    assert_eq!(app.selected_path, second);
}

#[test]
fn preview_meters_sum_every_path() {
    let mut app = blank_app();
    let copy = app.duplicate_path(0).unwrap();
    app.set_path_mute(copy, false);
    app.clock_output.path_map[copy].invert = true;
    app.clock_output.path_map[copy].level_db = -60.0;
    let gains = app.path_gains();
    assert_eq!(gains[0], 1.0);
    let frame = app
        .clock_output
        .settled_sum(app.live_path_mask(), gains, 2, 0.5, -0.25);
    assert_eq!(frame[..2], [0.5, -0.25], "a −∞ dB path adds nothing");
    app.clock_output.path_map[copy].level_db = 0.0;
    let frame = app
        .clock_output
        .settled_sum(app.live_path_mask(), app.path_gains(), 2, 0.5, -0.25);
    assert_eq!(frame[..2], [0.0, 0.0], "the flipped copy cancels");
    app.set_path_solo(copy, true);
    assert_eq!(app.path_gains()[0], 0.0);
}

#[cfg(target_os = "linux")]
#[test]
fn a_new_extra_output_copies_the_clock_outputs_paths() {
    let mut app = blank_app();
    // Extra outputs need a PipeWire clock output.
    app.inventory.outputs.push(crate::audio::DeviceItem {
        id: "pipewire:alsa_output.usb".to_owned(),
        name: "Interface".to_owned(),
        backend: "PipeWire".to_owned(),
        channels: 2,
        demo: false,
        preferred: false,
        asio_clsid: None,
    });
    app.output_idx = 0;
    let second = app.add_path().unwrap();
    app.add_extra_output_device(crate::audio::VIRTUAL_OUTPUT_ID);
    let extra_output = app.extra_outputs.last().expect("added");
    assert_eq!(extra_output.settings.path_map, app.clock_output.path_map);
    assert_eq!(
        extra_output.settings.path_map[second],
        PathMapSettings::unrouted(),
        "the empty path stays off the new device"
    );
}

fn effect(path: &str) -> PluginPref {
    PluginPref {
        path: path.to_owned(),
        name: path.to_owned(),
        ..PluginPref::default()
    }
}

#[test]
fn a_session_saves_and_applies_every_path() {
    let mut app = blank_app();
    app.session_name = "Two paths".to_owned();
    app.engine.restore_plugins(0, vec![effect("A.vst3")]);
    let copy = app.duplicate_path(0).expect("a free slot");
    assert_eq!(app.engine.plugin_prefs(copy)[0].path, "A.vst3");
    app.engine.restore_plugins(copy, vec![effect("B.vst3")]);
    app.set_path_level(copy, -5.0);
    app.set_path_mute(copy, false);
    app.clock_output.path_map[copy].invert = true;
    app.save_session();
    let saved = app.sessions[0].clone();
    assert_eq!(saved.paths.len(), 2);
    assert_eq!(saved.paths[1].name, "Main 2");
    assert_eq!(saved.paths[1].plugins[0].path, "B.vst3");
    assert_eq!(saved.paths[1].level_db, -5.0);
    assert!(saved.clock_output.path_map[1].invert);

    // Back to one path, then apply: both paths come back, live.
    assert!(app.remove_path(copy));
    app.engine.service_plugins();
    app.clock_output = OutputDeviceSettings::default();
    app.apply_session(0);
    assert_eq!(app.path_slots(), vec![0, 1]);
    assert_eq!(app.engine.path_state(1), PathState::Active);
    assert_eq!(app.engine.plugin_prefs(0)[0].path, "A.vst3");
    assert_eq!(app.engine.plugin_prefs(1)[0].path, "B.vst3");
    assert_eq!(app.engine.shared.path(1).level_db(), -5.0);
    assert!(!app.engine.shared.path(1).muted());
    assert!(app.clock_output.path_map[1].invert);
    assert_eq!(app.engine.shared.clock_output.path_gain(1), -1.0);

    // A session with one path removes the second, with a fade.
    app.sessions.push(crate::app::settings::Session {
        name: "One".to_owned(),
        ..crate::app::settings::Session::default()
    });
    app.apply_session(1);
    assert_eq!(app.path_slots(), vec![0]);
    assert_eq!(app.engine.path_state(1), PathState::FadingOut);
    assert!(app.engine.plugin_prefs(0).is_empty());
}

#[test]
fn plugin_editors_are_keyed_by_path_and_effect() {
    let mut app = blank_app();
    app.engine
        .restore_plugins(0, vec![effect("A.vst3"), effect("B.vst3")]);
    let second = app.add_path().expect("a free slot");
    app.engine.restore_plugins(second, vec![effect("C.vst3")]);
    app.plugin_editor = Some(EffectKey { path: 0, effect: 1 });
    // The selected path is the new one: its removal leaves path 0's
    // editor alone.
    app.remove_plugin(0);
    assert_eq!(app.plugin_editor, Some(EffectKey { path: 0, effect: 1 }));
    assert!(app.engine.plugin_views(second).is_empty());
    assert_eq!(app.engine.plugin_views(0).len(), 2);
    // On path 0, the editor follows its effect as it slides up.
    assert!(app.select_path(0));
    app.remove_plugin(0);
    assert_eq!(app.plugin_editor, Some(EffectKey { path: 0, effect: 0 }));
    // Removing the path closes its editor.
    assert!(app.remove_path(0));
    assert_eq!(app.plugin_editor, None);
    assert_eq!(app.selected_path, second);
}

#[test]
fn apply_click_pair_routes_music_to_one_and_two_and_the_click_to_three_and_four() {
    use crate::audio::{DeviceItem, MapPreset, UnusedChannels};

    let mut app = blank_app();
    app.inventory.outputs.push(DeviceItem {
        id: "hw:Out".to_owned(),
        name: "Out".to_owned(),
        backend: "ALSA".to_owned(),
        channels: 4,
        demo: false,
        preferred: true,
        asio_clsid: None,
    });
    app.clock_output.level_db = -3.0;
    app.clock_output.mute = true;
    app.clock_output.align = false;
    app.clock_output.click_mask = 0;
    app.clock_output.path_map[0].level_db = -6.0;
    app.clock_output.path_map[0].invert = true;
    app.clock_output.path_map[0].trim_ms = 1.25;
    app.clock_output.path_map[0].map = ChannelMap {
        left: 0b1000_0001,
        right: 0b1000_0010,
    };
    app.clock_output.path_map[1] = PathMapSettings {
        level_db: -2.0,
        invert: true,
        trim_ms: 0.5,
        map: ChannelMap { left: 0, right: 0 },
    };
    let wide = app.clock_output.path_map[0].map;
    app.apply_click_pair(0);

    let routed = app.clock_output.path_map[0];
    assert_eq!(
        routed.map,
        wide.with_preset(MapPreset::Stereo12, 4),
        "bits past the device width stay"
    );
    assert_ne!(routed.map.left & 0b1000_0000, 0);
    assert_eq!(routed.level_db, -6.0);
    assert!(routed.invert);
    assert_eq!(routed.trim_ms, 1.25);
    let empty = app.clock_output.path_map[1];
    assert!(empty.map.is_empty(), "an unrouted path stays unrouted");
    assert_eq!(empty.level_db, -2.0);
    assert!(empty.invert);
    assert_eq!(empty.trim_ms, 0.5);
    assert_eq!(app.clock_output.unused, UnusedChannels::Silent);
    assert_eq!(app.clock_output.click_mask, 0b1100);
    // A bit past this device's width is kept when the pair is applied again.
    app.clock_output.click_mask = 0b1000_0000;
    app.apply_click_pair(0);
    assert_eq!(app.clock_output.click_mask, 0b1000_1100);
    assert!(app.clock_output.path_map[1].map.is_empty());
    assert_eq!(app.clock_output.level_db, -3.0);
    assert!(app.clock_output.mute);
    assert!(!app.clock_output.align);
}

#[test]
fn set_click_channel_moves_one_bit_of_one_device() {
    let mut app = blank_app();
    app.extra_outputs
        .push(crate::app::settings::ExtraOutputPref::default());
    app.clock_output.click_mask = 0b1000_0001;
    app.extra_outputs[0].settings.click_mask = 0;
    let routing = app.clock_output.path_map;

    app.set_click_channel(0, 2, true);
    assert_eq!(app.clock_output.click_mask, 0b1000_0101);
    app.set_click_channel(0, 0, false);
    assert_eq!(app.clock_output.click_mask, 0b1000_0100);
    app.set_click_channel(0, 2, true);
    assert_eq!(
        app.clock_output.click_mask, 0b1000_0100,
        "setting twice is a no-op"
    );
    assert_eq!(app.clock_output.path_map, routing, "routing is unchanged");
    assert_eq!(app.extra_outputs[0].settings.click_mask, 0);

    app.set_click_channel(1, 3, true);
    assert_eq!(app.extra_outputs[0].settings.click_mask, 0b1000);
    assert_eq!(app.clock_output.click_mask, 0b1000_0100);

    // Past the mask, or no such device: nothing moves.
    app.set_click_channel(0, crate::audio::MAX_OUTPUT_CHANNELS, true);
    app.set_click_channel(2, 0, true);
    assert_eq!(app.clock_output.click_mask, 0b1000_0100);
    assert_eq!(app.extra_outputs[0].settings.click_mask, 0b1000);
}

#[test]
fn the_gate_reduction_reads_the_snapshot_only_while_the_bridge_runs() {
    let mut app = crate::app::blank_app();
    app.snapshot.gate_reduction_db[1] = 24.0;
    assert_eq!(app.gate_reduction_db(1), 0.0);
    app.engine.state = crate::audio::EngineState::Running;
    assert_eq!(app.gate_reduction_db(1), 24.0);
    assert_eq!(app.gate_reduction_db(0), 0.0);
    assert_eq!(app.gate_reduction_db(crate::audio::MAX_PATHS), 0.0);
}

#[test]
fn the_source_button_names_the_inputs_and_the_mode() {
    let mut app = blank_app();
    assert_eq!(app.source_label(0), "In 1 + In 2 · stereo");
    app.set_path_source(0, 1, 1, false);
    app.set_path_source(0, 0, 1, true);
    app.set_path_mono(0, true);
    assert_eq!(app.source_label(0), "In 1 · mono");
    app.path_mut().mix.output_enabled = [true, false];
    assert_eq!(app.source_label(0), "In 1 · mono · L only");
    app.path_mut().mix.output_enabled = [false, true];
    assert_eq!(app.source_label(0), "In 1 · mono · R only");
    app.set_path_source(0, 0, 0, false);
    app.set_path_source(0, 0, 1, false);
    app.set_path_source(0, 1, 0, true);
    app.path_mut().mix.output_enabled = [true, true];
    assert_eq!(app.source_label(0), "In 2 · mono");
    app.set_path_source(0, 2, 0, true);
    assert_eq!(
        app.paths[0].as_ref().unwrap().mix.routes[1],
        [true, false, false, false]
    );
    assert_eq!(app.source_label(3), "");
}

#[test]
fn removing_a_path_takes_two_clicks() {
    let mut app = blank_app();
    let second = app.add_path().expect("a free slot");
    assert!(!app.request_remove_path(second), "the first click arms it");
    assert!(app.paths[second].is_some());
    assert!(
        !app.request_remove_path(0),
        "another path disarms the first"
    );
    assert!(!app.request_remove_path(second));
    assert!(app.request_remove_path(second));
    assert!(app.paths[second].is_none());
    assert_eq!(app.chrome.remove_confirm, None);
}

#[test]
fn the_dsp_marker_goes_to_the_heaviest_path() {
    use super::heaviest_path;
    assert_eq!(heaviest_path(&[(0, 10), (2, 30), (1, 30)]), Some(2));
    assert_eq!(heaviest_path(&[(0, 0)]), None);
    assert_eq!(heaviest_path(&[]), None);
    let app = blank_app();
    assert_eq!(app.dsp_marker_path(), None, "no alarm");
}

#[test]
fn a_path_colour_is_stored_and_the_level_and_solo_are_independent() {
    let mut app = blank_app();
    let copy = app.duplicate_path(0).unwrap();
    // A duplicate starts muted: unmute it to test level and solo on their own.
    app.set_path_mute(copy, false);
    app.set_path_colour(copy, 3);
    assert_eq!(app.path_infos()[1].colour_index, 3);
    app.set_path_level(copy, -6.0);
    app.set_path_solo(copy, true);
    let info = app
        .path_infos()
        .into_iter()
        .find(|info| info.slot == copy)
        .unwrap();
    assert_eq!(info.level_db, -6.0);
    assert!(info.solo);
    assert!(!info.mute);
    assert!(info.audible);
    app.set_path_mute(copy, true);
    let info = app
        .path_infos()
        .into_iter()
        .find(|info| info.slot == copy)
        .unwrap();
    assert!(!info.audible);
}

#[test]
fn a_duplicated_path_loads_its_own_nam_model() {
    let mut app = blank_app();
    let source = app.add_path().expect("a free slot");
    let index = app.add_nam_block(source).expect("a block");
    app.set_nam_model(source, index, "/nonexistent/cab.nam".to_owned());
    let copy = app.duplicate_path(source).expect("a free slot");
    // Both slots went live once: their generations match, so the source's
    // stamp would match the copy too.
    assert_eq!(
        app.engine.path_generation(source),
        app.engine.path_generation(copy)
    );
    let path = app.paths[copy].as_ref().unwrap();
    assert_eq!(path.nam[0].model, "/nonexistent/cab.nam");
    assert_eq!(path.nam[0].install, crate::app::nam::NamInstall::default());
    app.sync_nam_models();
    let path = app.paths[copy].as_ref().unwrap();
    assert_eq!(
        path.nam[0].install.path, "/nonexistent/cab.nam",
        "loaded again"
    );
}

#[test]
fn moving_one_nam_block_after_the_effects_moves_only_that_block() {
    use crate::audio::NamPosition;
    let mut app = blank_app();
    let slot = app.add_path().expect("a free slot");
    let pedal = app.add_nam_block(slot).expect("a block");
    let amp = app.add_nam_block(slot).expect("a block");
    let path = app.paths[slot].as_ref().unwrap();
    let (pedal_id, amp_id) = (path.nam[pedal].controls.id, path.nam[amp].controls.id);
    let mut moved = path.nam[pedal].controls;
    moved.position = NamPosition::Post;
    moved.input_db = 1.5;
    app.set_nam_controls(slot, pedal, moved);

    let path = app.paths[slot].as_ref().unwrap();
    let position = |id: u32| {
        path.nam
            .iter()
            .find(|block| block.controls.id == id)
            .map(|block| block.controls.position)
    };
    assert_eq!(
        position(amp_id),
        Some(NamPosition::Pre),
        "the other block stays"
    );
    assert_eq!(position(pedal_id), Some(NamPosition::Post));
    let ids: Vec<u32> = path.nam.iter().map(|block| block.controls.id).collect();
    assert_eq!(ids, [amp_id, pedal_id], "Pre group first, then Post");
    assert_eq!(path.nam[1].controls.input_db, 1.5, "the edit is kept");
}

#[test]
fn a_preset_into_a_live_slot_never_reuses_a_block_id() {
    use crate::app::settings::PathPref;
    use crate::audio::NamBlockSettings;
    let mut app = blank_app();
    let live = app.path_slots()[0];
    app.add_nam_block(live).expect("a block");
    app.add_nam_block(live).expect("a block");
    // A path that is fading out still holds its blocks in the engine.
    let fading = app.add_path().expect("a free slot");
    app.add_nam_block(fading).expect("a block");
    assert!(app.remove_path(fading));
    assert_eq!(app.engine.path_state(fading), PathState::FadingOut);

    let pref = PathPref {
        nam_blocks: vec![NamBlockSettings::default(); 2],
        ..PathPref::default()
    };
    let prefs = vec![pref; MAX_PATHS];
    app.install_paths(&prefs, OutputDeviceSettings::default(), Vec::new(), 0);

    let ids = |app: &crate::app::RigWerkApp, slot: usize| -> Vec<u32> {
        app.paths[slot]
            .as_ref()
            .unwrap()
            .nam
            .iter()
            .map(|block| block.controls.id)
            .collect()
    };
    let published = |app: &crate::app::RigWerkApp, slot: usize| -> Vec<u32> {
        app.engine
            .path_nam(slot)
            .blocks()
            .iter()
            .map(|block| block.id)
            .collect()
    };
    // The live slot numbers on from its own next id, and the old ids leave
    // the published stack, so the engine fades them out.
    assert_eq!(ids(&app, live), vec![2, 3]);
    assert_eq!(published(&app, live), vec![2, 3]);
    assert_eq!(app.paths[live].as_ref().unwrap().next_nam_id, 4);
    // The fading slot numbers past what the engine still published.
    assert_eq!(ids(&app, fading), vec![1, 2]);
    assert_eq!(published(&app, fading), vec![1, 2]);
    // A slot that held nothing starts at 0.
    let fresh = app
        .path_slots()
        .into_iter()
        .find(|&slot| slot != live && slot != fading)
        .expect("a fresh slot");
    assert_eq!(ids(&app, fresh), vec![0, 1]);
}

#[test]
fn an_interleaved_save_loads_in_stack_order() {
    use crate::app::settings::PathPref;
    use crate::audio::{NamBlockControls, NamBlockSettings, NamPosition};
    let block = |position: NamPosition, model: &str| NamBlockSettings {
        model: model.to_owned(),
        controls: NamBlockControls {
            position,
            ..NamBlockControls::default()
        },
    };
    let pref = PathPref {
        nam_blocks: vec![
            block(NamPosition::Post, "a"),
            block(NamPosition::Pre, "b"),
            block(NamPosition::Post, "c"),
            block(NamPosition::Pre, "d"),
        ],
        ..PathPref::default()
    };
    let path = super::SignalPath::from_pref(&pref, 10);
    let models: Vec<&str> = path.nam.iter().map(|block| block.model.as_str()).collect();
    assert_eq!(models, ["b", "d", "a", "c"], "Pre group first, then Post");
    let stack: Vec<u32> = path.nam_stack().blocks().iter().map(|b| b.id).collect();
    assert_eq!(stack, path.nam_ids(), "a row's index names the same block");
    assert_eq!(path.next_nam_id, 14);
}
