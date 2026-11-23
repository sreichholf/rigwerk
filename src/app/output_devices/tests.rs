use super::{
    click_channels, default_click_mask, device_channel_count, learned_trim_after, learned_trim_db,
    loudest, matched_trim_db, usable_loudness, OutputBadge, PlaysClick,
};
use crate::app::blank_app;
use crate::app::devices::inventory_fingerprint;
use crate::app::settings::ExtraOutputPref;
use crate::audio::{DeviceItem, EngineState, MAX_OUTPUT_CHANNELS};

#[test]
fn channel_count_prefers_the_stream_then_the_scan() {
    assert_eq!(device_channel_count(6, 2), 6);
    assert_eq!(device_channel_count(0, 4), 4);
    assert_eq!(device_channel_count(0, 0), 2);
    assert_eq!(device_channel_count(1, 0), 1);
    assert_eq!(device_channel_count(32, 2), MAX_OUTPUT_CHANNELS);
    assert_eq!(device_channel_count(0, 16), MAX_OUTPUT_CHANNELS);
}

#[test]
fn click_chips_stop_at_the_native_pipewire_stereo_stream() {
    #[cfg(target_os = "linux")]
    {
        assert_eq!(
            click_channels("pipewire:alsa_output.pci-0000_00_1f.3", 8),
            2
        );
        assert_eq!(click_channels("pipewire:alsa_output.usb", 1), 1);
    }
    // The ALSA `pipewire` PCM is opened through cpal with its real count.
    assert_eq!(click_channels("alsa:pipewire", 6), 6);
    assert_eq!(click_channels("alsa:hw:CARD=Usb", 8), 8);
    assert_eq!(click_channels("asio:Interface", 4), 4);
}

#[test]
fn a_pipewire_labelled_extra_output_keeps_its_open_channel_count() {
    let mut app = blank_app();
    app.inventory.outputs.push(DeviceItem {
        id: "alsa:pipewire".to_owned(),
        name: "PipeWire Sound Server".to_owned(),
        backend: "PIPEWIRE".to_owned(),
        channels: 2,
        demo: false,
        preferred: false,
        asio_clsid: None,
    });
    app.extra_outputs.push(ExtraOutputPref {
        id: "alsa:pipewire".to_owned(),
        backend: "PIPEWIRE".to_owned(),
        name: "PipeWire Sound Server".to_owned(),
        ..ExtraOutputPref::default()
    });
    app.engine.state = EngineState::Running;
    app.snapshot.extra_output_channel_counts[0] = 6;
    let rows = app.output_device_rows();
    let row = &rows[1];
    assert_eq!(row.extra_output_index, Some(0));
    assert_eq!(row.channels, 6);
    assert_eq!(row.click_channels, 6);
}

fn output(id: &str, name: &str) -> DeviceItem {
    DeviceItem {
        id: id.to_owned(),
        name: name.to_owned(),
        backend: "ALSA".to_owned(),
        channels: 2,
        demo: false,
        preferred: false,
        asio_clsid: None,
    }
}

#[test]
fn a_clock_output_the_scan_missed_reads_missing() {
    let mut app = blank_app();
    app.inventory.outputs.push(output("hw:Speaker", "Studio"));
    app.inventory.outputs.push(output("hw:Phones", "Phones"));
    app.output_idx = 0;
    app.inventory_fingerprint = inventory_fingerprint(&app.inventory);
    app.engine.state = EngineState::Running;
    assert!(!app.output_device_rows()[0].missing);

    let mut gone = app.inventory.clone();
    gone.outputs.remove(0);
    app.apply_inventory(gone.clone());
    let rows = app.output_device_rows();
    assert_eq!(rows[0].name, "Studio");
    assert!(rows[0].clock && rows[0].missing);

    // The second miss moves the selection, and no scan missed the fallback.
    app.apply_inventory(gone);
    let rows = app.output_device_rows();
    assert_eq!(rows[0].name, "Phones");
    assert!(!rows[0].missing);
}

#[test]
fn a_clock_output_back_in_the_scan_is_not_missing() {
    let mut app = blank_app();
    app.inventory.outputs.push(output("hw:Speaker", "Studio"));
    app.output_idx = 0;
    app.inventory_fingerprint = inventory_fingerprint(&app.inventory);
    let present = app.inventory.clone();
    let mut gone = present.clone();
    gone.outputs.clear();
    app.apply_inventory(gone);
    assert!(app.output_device_rows()[0].missing);
    app.apply_inventory(present);
    assert!(!app.output_device_rows()[0].missing);
}

#[test]
fn a_clock_output_chosen_after_a_miss_is_not_missing() {
    let mut app = blank_app();
    app.inventory.outputs.push(output("hw:Speaker", "Studio"));
    app.inventory.outputs.push(output("hw:Phones", "Phones"));
    app.output_idx = 0;
    app.inventory_fingerprint = inventory_fingerprint(&app.inventory);
    let mut gone = app.inventory.clone();
    gone.outputs.remove(0);
    app.apply_inventory(gone);
    assert!(app.output_device_rows()[0].missing);

    app.output_idx = app
        .inventory
        .outputs
        .iter()
        .position(|device| device.id == "hw:Phones")
        .unwrap();
    app.accept_output_device();
    let rows = app.output_device_rows();
    assert_eq!(rows[0].name, "Phones");
    assert!(!rows[0].missing);
}

#[test]
fn a_preview_clock_output_is_never_missing() {
    let app = blank_app();
    assert!(app.selected_output().demo);
    assert!(!app.output_device_rows()[0].missing);
}

#[test]
fn the_loudest_channel_stops_at_the_device_count() {
    assert_eq!(loudest(&[0.1, 0.4, 0.9], 2), 0.4);
    assert_eq!(loudest(&[0.1, 0.4], 8), 0.4);
    assert_eq!(loudest(&[], 2), 0.0);
}

#[test]
fn a_extra_output_reading_needs_a_running_bridge_and_a_stream() {
    let mut app = blank_app();
    app.snapshot.output_rate = 48_000;
    app.snapshot.extra_output_rates[1] = 44_100;
    app.snapshot.extra_output_buffer_frames[1] = 256;
    app.snapshot.extra_output_ring_fill[1] = 0.5;
    app.snapshot.extra_output_target_frames[1] = 240;
    assert_eq!(app.extra_output_reading(1), None, "stopped");
    app.engine.state = EngineState::Running;
    let reading = app.extra_output_reading(1).expect("a stream");
    assert_eq!(reading.rate, 44_100);
    assert_eq!(reading.buffer_frames, 256);
    assert_eq!(reading.fill, 0.5);
    assert!(
        (reading.bridge_buffer_ms - 5.0).abs() < 1.0e-4,
        "{reading:?}"
    );
    assert_eq!(app.extra_output_reading(0), None, "no stream of its own");
    assert_eq!(
        app.extra_output_reading(crate::audio::MAX_EXTRA_OUTPUTS),
        None
    );
}

/// The blank app with a four-channel extra output (device 1).
fn with_quad_output() -> crate::app::RigWerkApp {
    let mut app = blank_app();
    app.inventory.outputs.push(output("alsa:hw:Main", "Main"));
    app.output_idx = 0;
    let mut quad = output("alsa:hw:Quad", "Quad");
    quad.channels = 4;
    app.inventory.outputs.push(quad);
    app.extra_outputs.push(ExtraOutputPref {
        id: "alsa:hw:Quad".to_owned(),
        backend: "ALSA".to_owned(),
        name: "Quad".to_owned(),
        ..ExtraOutputPref::default()
    });
    app
}

#[test]
fn a_plays_dot_fills_an_empty_map_and_clears_a_plain_one() {
    use crate::audio::ChannelMap;
    let mut app = with_quad_output();
    app.extra_outputs[0].settings.path_map[0].map = ChannelMap { left: 0, right: 0 };
    assert!(!app.path_reaches_device(0, 1));
    assert_eq!(app.click_plays(1, 0), PlaysClick::Applied);
    assert_eq!(
        app.extra_outputs[0].settings.path_map[0].map,
        ChannelMap::default()
    );
    assert!(app.path_reaches_device(0, 1));
    assert_eq!(app.click_plays(1, 0), PlaysClick::Cleared);
    assert!(!app.path_reaches_device(0, 1));
    assert_eq!(
        app.click_plays(1, 3),
        PlaysClick::Nothing,
        "no path in slot 3"
    );
}

#[test]
fn a_plays_dot_never_clears_a_custom_map() {
    use crate::app::screen::Sheet;
    use crate::audio::ChannelMap;
    let mut app = with_quad_output();
    let custom = ChannelMap {
        left: 0b0101,
        right: 0b1010,
    };
    app.extra_outputs[0].settings.path_map[0].map = custom;
    assert_eq!(app.click_plays(1, 0), PlaysClick::OpenSheet);
    assert_eq!(app.extra_outputs[0].settings.path_map[0].map, custom);
    app.press_plays(1, 0);
    assert_eq!(app.chrome.sheet, Some(Sheet::Output { output: 1 }));
    assert_eq!(app.extra_outputs[0].settings.path_map[0].map, custom);
}

#[test]
fn a_plays_dot_ignores_bits_past_the_device() {
    use crate::audio::ChannelMap;
    let mut app = blank_app();
    // Out 1/2, plus 3/4 kept from a wider device the clock output was.
    app.clock_output.path_map[0].map = ChannelMap {
        left: 0b0101,
        right: 0b1010,
    };
    assert_eq!(app.click_plays(0, 0), PlaysClick::Cleared);
    assert_eq!(
        app.clock_output.path_map[0].map,
        ChannelMap {
            left: 0b0100,
            right: 0b1000
        },
        "the hidden bits stay"
    );
}

#[test]
fn the_metronome_toggle_moves_only_the_click_channels() {
    let mut app = with_quad_output();
    let map = app.extra_outputs[0].settings.path_map;
    assert!(!app.metronome_on(1));
    app.toggle_metronome(1);
    assert_eq!(
        app.extra_outputs[0].settings.click_mask, 0b1100,
        "3 and 4 on four channels"
    );
    assert!(app.metronome_on(1));
    app.toggle_metronome(0);
    assert_eq!(app.clock_output.click_mask, 0b0011, "1 and 2 on two");
    // Off remembers the channels, on restores them where they fit.
    app.extra_outputs[0].settings.click_mask = 0b0110;
    app.toggle_metronome(1);
    assert_eq!(app.extra_outputs[0].settings.click_mask, 0);
    assert_eq!(app.last_click_mask, 0b0110);
    app.toggle_metronome(1);
    assert_eq!(app.extra_outputs[0].settings.click_mask, 0b0110);
    app.toggle_metronome(0);
    app.toggle_metronome(0);
    assert_eq!(
        app.clock_output.click_mask, 0b0011,
        "what fits a stereo output"
    );
    assert_eq!(app.extra_outputs[0].settings.path_map, map, "routing stays");
}

#[test]
fn the_metronome_falls_back_by_channel_count() {
    assert_eq!(default_click_mask(8), 0b1100);
    assert_eq!(default_click_mask(4), 0b1100);
    assert_eq!(default_click_mask(2), 0b0011);
    assert_eq!(default_click_mask(1), 0b0001);
    // A native PipeWire output takes two click channels at most.
    #[cfg(target_os = "linux")]
    assert_eq!(
        default_click_mask(click_channels("pipewire:sink", 8)),
        0b0011
    );
}

#[test]
fn strips_badge_the_clock_and_the_extra_outputs() {
    let mut app = with_quad_output();
    assert_eq!(app.output_badge(0), OutputBadge::Clock);
    assert_eq!(app.output_badge(1), OutputBadge::Extra);
    app.engine.state = EngineState::Running;
    app.snapshot.extra_output_missing[0] = true;
    assert_eq!(app.output_badge(1), OutputBadge::Missing);
    assert!(OutputBadge::Missing.warns());
    app.inventory
        .outputs
        .retain(|device| device.id != "alsa:hw:Quad");
    app.snapshot.extra_output_missing[0] = false;
    assert_eq!(app.output_badge(1), OutputBadge::Missing, "not listed");
}

#[test]
fn the_buffer_line_reads_what_the_device_opened() {
    let mut app = with_quad_output();
    app.low_latency_output = false;
    app.wasapi_output_mode = crate::audio::WasapiOutputMode::Shared;
    let chosen = if cfg!(target_os = "windows") {
        "Shared"
    } else {
        "Standard"
    };
    assert_eq!(app.output_buffer_line(0), chosen);
    app.engine.state = EngineState::Running;
    app.snapshot.opened_output_mode = Some(crate::audio::OutputMode::LowLatency);
    app.snapshot.output_rate = 48_000;
    app.snapshot.output_step_frames = 128;
    assert_eq!(app.output_buffer_line(0), "Low latency · 2.7 ms");
    assert_eq!(app.output_buffer_line(1), "auto");
    app.snapshot.extra_output_rates[0] = 48_000;
    app.snapshot.extra_output_target_frames[0] = 254;
    assert_eq!(app.output_buffer_line(1), "auto 5.3 ms");
}

#[test]
fn output_level_and_mute_stay_in_range() {
    let mut app = with_quad_output();
    app.set_output_level(1, 40.0);
    assert_eq!(
        app.extra_outputs[0].settings.level_db,
        crate::audio::DEVICE_LEVEL_MAX_DB
    );
    app.set_output_level(0, f32::NAN);
    assert_eq!(app.clock_output.level_db, 0.0);
    app.set_output_mute(1, true);
    assert!(app.extra_outputs[0].settings.mute);
}

#[test]
fn output_loudness_target_and_learned_trim_stay_in_range() {
    let mut app = with_quad_output();
    app.clear_output_learn(1);
    assert_eq!(app.extra_outputs[0].settings.loudness.learned_db, 0.0);
    app.set_output_loudness_target(0, 40.0);
    assert_eq!(
        app.clock_output.loudness.target_lufs,
        crate::audio::LOUDNESS_TARGET_MAX_LUFS
    );
    app.set_output_loudness_target(0, f32::NAN);
    assert_eq!(
        app.clock_output.loudness.target_lufs,
        crate::audio::LOUDNESS_TARGET_DEFAULT_LUFS
    );
    app.set_output_loudness_target(0, -100.0);
    assert_eq!(
        app.clock_output.loudness.target_lufs,
        crate::audio::LOUDNESS_TARGET_MIN_LUFS
    );
}

#[test]
fn a_learn_adds_the_difference_and_refuses_a_short_window() {
    assert_eq!(learned_trim_db(0.0, -14.0, -23.0), 9.0);
    // Learning again after the trim has corrected the reading changes nothing.
    assert_eq!(learned_trim_db(9.0, -14.0, -14.0), 9.0);
    assert_eq!(learned_trim_db(0.0, -14.0, -50.0), 12.0);
    assert_eq!(learned_trim_after(0.0, -14.0, -23.0, 26), None);
    assert_eq!(learned_trim_after(0.0, -14.0, -23.0, 27), Some(9.0));
    assert_eq!(
        learned_trim_after(0.0, -14.0, crate::audio::SILENCE_LUFS, 80),
        None
    );
    assert!(usable_loudness(-14.0, -33.0));
    assert!(!usable_loudness(-14.0, -35.0));
    assert_eq!(learned_trim_after(0.0, -14.0, -35.0, 80), None);
}

#[test]
fn a_match_adds_the_difference_to_the_current_trim() {
    assert_eq!(matched_trim_db(0.0, -14.0, -23.0), 9.0);
    assert_eq!(matched_trim_db(3.0, -14.0, -20.0), 9.0);
    assert_eq!(matched_trim_db(0.0, -14.0, -50.0), 24.0);
    assert_eq!(matched_trim_db(0.0, -14.0, 10.0), -24.0);
}

#[test]
fn a_map_summary_reads_pairs_sides_and_unreached_channels() {
    use super::map_summary;
    use crate::audio::{ChannelMap, UnusedChannels};

    let mut app = with_quad_output();
    let settings = &mut app.extra_outputs[0].settings;
    assert_eq!(map_summary(settings, 0, 4).text, "Out 1/2 · rest mono copy");

    settings.path_map[0].map = ChannelMap {
        left: 0b0101,
        right: 0b1010,
    };
    assert_eq!(map_summary(settings, 0, 4).text, "Out 1/2 · Out 3/4");

    settings.path_map[0].map = ChannelMap {
        left: 0b0010,
        right: 0b0001,
    };
    assert_eq!(
        map_summary(settings, 0, 4).text,
        "Out 1/2 swapped · rest mono copy"
    );

    settings.path_map[0].map = ChannelMap {
        left: 0b0001,
        right: 0b0100,
    };
    assert_eq!(
        map_summary(settings, 0, 4).text,
        "Out 1 ← L · Out 3 ← R · rest mono copy"
    );

    settings.path_map[0].map = ChannelMap {
        left: 0b0001,
        right: 0b0001,
    };
    assert_eq!(map_summary(settings, 0, 1).text, "Out 1 ← mono");

    settings.path_map[0].map = ChannelMap { left: 0, right: 0 };
    settings.unused = UnusedChannels::MonoCopy;
    let empty = map_summary(settings, 0, 4);
    assert_eq!(empty.text, "nothing routed · mono copy on all");
    assert!(empty.nothing_routed && !empty.silent);

    settings.unused = UnusedChannels::Silent;
    let silent = map_summary(settings, 0, 4);
    assert_eq!(silent.text, "nothing routed · plays silence");
    assert!(silent.nothing_routed && silent.silent);

    // A bit past this device's width reaches nothing.
    settings.path_map[0].map = ChannelMap {
        left: 1 << 7,
        right: 0,
    };
    assert!(map_summary(settings, 0, 4).nothing_routed);
}

#[test]
fn a_map_edit_writes_one_slot_and_logs_what_committed() {
    use super::MapEdit;
    use crate::app::layouts::{custom_label, PathLayout};

    let mut app = with_quad_output();
    assert!(app.apply_layout(PathLayout::Single));
    assert_eq!(app.current_layout(), Some(PathLayout::Single));

    app.apply_map_edit(
        1,
        MapEdit::Level {
            slot: 0,
            level_db: 40.0,
            commit: true,
        },
    );
    assert_eq!(
        app.extra_outputs[0].settings.path_map[0].level_db,
        crate::audio::PATH_LEVEL_MAX_DB
    );
    assert!(app
        .log
        .iter()
        .any(|line| line.text.contains("Main level on Quad")));

    app.apply_map_edit(
        1,
        MapEdit::Invert {
            slot: 0,
            invert: true,
        },
    );
    assert!(app.extra_outputs[0].settings.path_map[0].invert);
    assert!(app
        .log
        .iter()
        .any(|line| line.text.contains("polarity on Quad")));

    let logged = app.log.len();
    app.apply_map_edit(
        1,
        MapEdit::Trim {
            slot: 0,
            trim_ms: 9.0,
            commit: false,
        },
    );
    assert_eq!(
        app.extra_outputs[0].settings.path_map[0].trim_ms,
        crate::audio::PATH_TRIM_MAX_MS
    );
    assert_eq!(app.log.len(), logged, "a drag logs only when it ends");

    app.apply_map_edit(
        1,
        MapEdit::Cell {
            slot: 0,
            side: crate::audio::MixSide::L,
            channel: 3,
            on: true,
        },
    );
    assert!(app.extra_outputs[0].settings.path_map[0]
        .map
        .routes(crate::audio::MixSide::L, 3));
    assert_eq!(app.current_layout(), None);
    assert_eq!(
        custom_label(app.routing_last_layout),
        "Custom · edited from Single path"
    );
}
