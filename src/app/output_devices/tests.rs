use super::{click_channels, device_channel_count};
use crate::app::blank_app;
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
