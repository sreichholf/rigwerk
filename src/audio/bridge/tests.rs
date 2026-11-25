use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use super::{select, Backend, Shared, StartConfig};

/// A start with no ASIO driver and ordinary system device ids.
fn plain() -> StartConfig {
    StartConfig {
        asio_clsid: None,
        input_id: Some("wasapi:input".into()),
        output_id: "wasapi:output".into(),
        output_asio_clsid: None,
        sample_rate: 48_000,
        buffer_size: 128,
        output_rate: None,
        output_buffer: 128,
        asio_wrapper: false,
        real_style_output: false,
        exclusive_output: false,
        pipewire_low_latency: false,
    }
}

/// Two system devices keep the cpal bridge, whatever the output settings are.
/// Every target can select every arm, so this runs on each of them.
#[test]
fn system_devices_fall_back_to_cpal() {
    let mut config = plain();
    config.output_rate = Some(44_100);
    config.real_style_output = true;
    config.exclusive_output = true;
    assert_eq!(select(&config), Backend::Cpal);
}

#[test]
fn an_output_asio_driver_picks_the_output_bridge() {
    let mut config = plain();
    config.output_asio_clsid = Some(1);
    assert_eq!(select(&config), Backend::AsioOutput(1));
}

#[test]
fn an_input_asio_driver_picks_the_input_bridge() {
    let mut config = plain();
    config.asio_clsid = Some(2);
    // The output id does not matter when ASIO input owns the clock.
    config.output_id = "pipewire:test_sink".into();
    assert_eq!(select(&config), Backend::AsioInput(2));
}

/// A native input id does not outrank the ASIO clock on the same side.
#[test]
fn an_input_asio_driver_outranks_a_native_input() {
    let mut config = plain();
    config.asio_clsid = Some(5);
    config.input_id = Some("pipewire:test_source".into());
    assert_eq!(select(&config), Backend::AsioInput(5));
}

/// A native id on the far side does not outrank ASIO output either.
#[test]
fn an_output_asio_driver_outranks_a_native_input() {
    let mut config = plain();
    config.output_asio_clsid = Some(4);
    config.input_id = Some("pipewire:test_sink".into());
    assert_eq!(select(&config), Backend::AsioOutput(4));
}

/// Both sides on ASIO: the output variant still wins, and carries its own
/// clsid.  `run_asio_output_bridge` turns the equal pair into a duplex run.
#[test]
fn an_output_asio_driver_outranks_an_input_one() {
    let mut config = plain();
    config.output_asio_clsid = Some(3);
    config.asio_clsid = Some(3);
    assert_eq!(select(&config), Backend::AsioOutput(3));
}

#[test]
fn a_native_input_id_picks_the_native_client() {
    let mut config = plain();
    config.input_id = Some("pipewire:test_sink".into());
    assert_eq!(select(&config), Backend::LinuxNative);
}

#[test]
fn a_native_output_id_picks_the_native_client() {
    let mut config = plain();
    config.output_id = "pipewire:test_source".into();
    assert_eq!(select(&config), Backend::LinuxNative);
}

/// The bare ALSA plugin id is not a native node; it stays with cpal.
#[test]
fn the_alsa_pipewire_shim_is_not_native() {
    let mut config = plain();
    config.output_id = "pipewire".into();
    config.input_id = Some("alsa:pipewire".into());
    assert_eq!(select(&config), Backend::Cpal);
}

/// The id form holds on every target: a prefix with no node name names
/// nothing, and a node called "pipewire" is the bare plugin id.
#[test]
fn a_pipewire_prefix_without_a_node_is_not_native() {
    let mut config = plain();
    config.output_id = "pipewire:".into();
    assert_eq!(select(&config), Backend::Cpal);

    config.output_id = "pipewire:pipewire".into();
    assert_eq!(select(&config), Backend::Cpal);
}

/// The ASIO arms this platform does not build report that instead of opening
/// anything: an arm that silently fell back would bridge on the wrong host.
#[cfg(not(target_os = "windows"))]
#[test]
fn the_asio_arms_are_not_here() {
    let shared = Arc::new(Shared::new(48_000));
    let stop = Arc::new(AtomicBool::new(false));
    for backend in [Backend::AsioOutput(1), Backend::AsioInput(2)] {
        let error = backend
            .run(&plain(), Arc::clone(&shared), Arc::clone(&stop))
            .unwrap_err();
        assert!(error.contains("not available on this platform"), "{error}");
    }
}

#[cfg(not(target_os = "linux"))]
#[test]
fn the_native_arm_is_not_here() {
    let shared = Arc::new(Shared::new(48_000));
    let stop = Arc::new(AtomicBool::new(false));
    let error = Backend::LinuxNative
        .run(&plain(), shared, stop)
        .unwrap_err();
    assert!(error.contains("not available on this platform"), "{error}");
}
