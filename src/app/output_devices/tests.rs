use super::{click_channels, device_channel_count, loudest};
use crate::app::blank_app;
use crate::app::devices::inventory_fingerprint;
use crate::app::settings::FollowerPref;
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
fn a_pipewire_labelled_follower_keeps_its_open_channel_count() {
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
    app.followers.push(FollowerPref {
        id: "alsa:pipewire".to_owned(),
        backend: "PIPEWIRE".to_owned(),
        name: "PipeWire Sound Server".to_owned(),
        ..FollowerPref::default()
    });
    app.engine.state = EngineState::Running;
    app.snapshot.follower_channel_counts[0] = 6;
    let rows = app.output_device_rows();
    let row = &rows[1];
    assert_eq!(row.follower_index, Some(0));
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
fn a_clock_device_the_scan_missed_reads_missing() {
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
fn a_clock_device_back_in_the_scan_is_not_missing() {
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
fn a_clock_device_chosen_after_a_miss_is_not_missing() {
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
fn a_preview_clock_device_is_never_missing() {
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
fn device_levels_follow_the_rows_and_their_meters() {
    let mut app = blank_app();
    let mut out = FollowerPref::virtual_out();
    out.settings.mute = true;
    app.followers.push(out);
    app.meter_out_channels[1] = 0.5;
    app.peak_out_channels[1] = 0.6;
    app.peak_out_channels[4] = 1.0;
    app.meter_follower_channels[0][0] = 0.25;
    app.peak_follower_channels[0][0] = 0.3;
    let levels = app.output_device_levels();
    assert_eq!(levels.len(), 2);
    let clock = &levels[0];
    assert_eq!((clock.meter, clock.peak, clock.mute), (0.5, 0.6, false));
    let virtual_out = &levels[1];
    assert_eq!(virtual_out.name, FollowerPref::virtual_out().name);
    assert_eq!(
        (virtual_out.meter, virtual_out.peak, virtual_out.mute),
        (0.25, 0.3, true)
    );
}

#[test]
fn a_follower_reading_needs_a_running_bridge_and_a_stream() {
    let mut app = blank_app();
    app.snapshot.output_rate = 48_000;
    app.snapshot.follower_rates[1] = 44_100;
    app.snapshot.follower_buffer_frames[1] = 256;
    app.snapshot.follower_ring_fill[1] = 0.5;
    app.snapshot.follower_target_frames[1] = 240;
    assert_eq!(app.follower_reading(1), None, "stopped");
    app.engine.state = EngineState::Running;
    let reading = app.follower_reading(1).expect("a stream");
    assert_eq!(reading.rate, 44_100);
    assert_eq!(reading.buffer_frames, 256);
    assert_eq!(reading.fill, 0.5);
    assert!((reading.cushion_ms - 5.0).abs() < 1.0e-4, "{reading:?}");
    assert_eq!(app.follower_reading(0), None, "no stream of its own");
    assert_eq!(app.follower_reading(crate::audio::MAX_FOLLOWERS), None);
}
