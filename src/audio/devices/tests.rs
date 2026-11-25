use super::{dedup_devices, is_low_latency_device, low_latency_decision};
use crate::audio::DeviceItem;

fn device(id: &str, backend: &str) -> DeviceItem {
    named(id, "test", backend)
}

fn named(id: &str, name: &str, backend: &str) -> DeviceItem {
    DeviceItem {
        id: id.to_owned(),
        name: name.to_owned(),
        backend: backend.to_owned(),
        channels: 2,
        demo: false,
        preferred: false,
        asio_clsid: None,
    }
}

#[test]
fn same_name_on_different_backends_both_remain() {
    let mut devices = vec![
        named("wasapi:{spk}", "Speakers", "WASAPI"),
        // Same endpoint listed twice; backend case must not make a second row.
        named("wasapi:{spk}", "Speakers", "wasapi"),
        named("asio-out:abc", "Speakers", "ASIO"),
        named("ALSA:hw:CARD=USB,DEV=0", "USB Audio", "ALSA"),
        named("ALSA:plughw:CARD=USB,DEV=0", "USB Audio", "ALSA"),
    ];
    dedup_devices(&mut devices);
    let keys: Vec<_> = devices
        .iter()
        .map(|device| (device.id.as_str(), device.backend.as_str()))
        .collect();
    assert_eq!(
        keys,
        [
            ("wasapi:{spk}", "WASAPI"),
            ("asio-out:abc", "ASIO"),
            ("ALSA:hw:CARD=USB,DEV=0", "ALSA"),
            ("ALSA:plughw:CARD=USB,DEV=0", "ALSA"),
        ]
    );
    // Each surviving id is its own row, so both "Speakers" can be chosen.
    assert_eq!(
        devices
            .iter()
            .position(|device| device.id == "wasapi:{spk}"),
        Some(0)
    );
    assert_eq!(
        devices
            .iter()
            .position(|device| device.id == "asio-out:abc"),
        Some(1)
    );
}

#[test]
fn the_low_latency_decision_is_asio_or_the_platform_group() {
    let mut asio = device("asio:abc", "ASIO");
    asio.asio_clsid = Some(1);
    let system = device("wasapi:{spk}", "WASAPI");
    let direct = device("ALSA:hw:CARD=0,DEV=0", "ALSA");
    // An ASIO driver belongs there even when the platform group does not claim
    // its backend.
    assert!(low_latency_decision(&asio, false));
    assert!(low_latency_decision(&asio, true));
    // A backend the platform group claims does too, without an ASIO driver.
    assert!(low_latency_decision(&direct, true));
    assert!(low_latency_decision(&system, true));
    // Neither input: a plain system device stays on the system side.
    assert!(!low_latency_decision(&direct, false));
    assert!(!low_latency_decision(&system, false));
}

#[test]
fn an_asio_driver_is_low_latency_on_every_target() {
    let mut asio = device("asio:abc", "ASIO");
    asio.asio_clsid = Some(1);
    assert!(is_low_latency_device(&asio, "ASIO"));
    assert!(is_low_latency_device(&asio, "PIPEWIRE"));
}

#[test]
fn a_plain_system_device_is_not_low_latency_on_every_target() {
    let system = device("wasapi:{spk}", "WASAPI");
    assert!(!is_low_latency_device(&system, "ASIO"));
    assert!(!is_low_latency_device(&system, "PIPEWIRE"));
}

#[cfg(target_os = "linux")]
#[test]
fn pipewire_is_the_low_latency_backend() {
    let pipewire = device("ALSA:pipewire", "ALSA");
    assert!(is_low_latency_device(&pipewire, "PIPEWIRE"));
    assert!(is_low_latency_device(&pipewire, "ALSA"));
}

#[cfg(target_os = "linux")]
#[test]
fn virtual_alsa_pcms_other_than_pipewire_stay_on_the_system_side() {
    for id in ["ALSA:pulse", "ALSA:sysdefault:CARD=0"] {
        assert!(
            !is_low_latency_device(&device(id, "ALSA"), "PIPEWIRE"),
            "{id}"
        );
        assert!(
            !is_low_latency_device(&device(id, "ALSA"), "ALSA"),
            "{id} with an ALSA label"
        );
    }
}

/// A row with the low-latency backend but no ASIO driver behind it is the one
/// input the two bodies answer differently.  The Linux body ignores the label
/// and answers `false` here, so this fails if a `test` arm ever puts that body
/// back into a Windows build.
#[cfg(not(target_os = "linux"))]
#[test]
fn the_off_linux_body_claims_the_low_latency_backend_by_label() {
    let by_label = device("asio-out:abc", "ASIO");
    assert!(is_low_latency_device(&by_label, "ASIO"));
    let system = device("wasapi:{spk}", "WASAPI");
    assert!(!is_low_latency_device(&system, "ASIO"));
}

#[test]
fn one_usb_card_does_not_repeat_under_every_alsa_plugin() {
    let card = "GP-5, USB Audio";
    let mut devices = vec![
        named("alsa:hw:CARD=GP5,DEV=0", card, "ALSA"),
        named("alsa:plughw:CARD=GP5,DEV=0", card, "ALSA"),
        named("alsa:sysdefault:CARD=GP5", card, "ALSA"),
        named("alsa:front:CARD=GP5", card, "ALSA"),
        named("alsa:usbstream:CARD=GP5", card, "ALSA"),
    ];
    super::hide_redundant_alsa_aliases(&mut devices);
    let ids: Vec<_> = devices.iter().map(|device| device.id.as_str()).collect();
    assert_eq!(
        ids,
        ["alsa:hw:CARD=GP5,DEV=0", "alsa:plughw:CARD=GP5,DEV=0",]
    );
    assert_eq!(
        super::device_choice_label(&devices[0], &devices),
        "GP-5, USB Audio · hw"
    );
    assert_eq!(
        super::device_choice_label(&devices[1], &devices),
        "GP-5, USB Audio · plughw"
    );
}

#[test]
fn two_pipewire_nodes_with_one_description_show_the_node_name() {
    let devices = vec![
        named("pipewire:alsa_input.usb-a", "USB Audio", "PIPEWIRE"),
        named("pipewire:alsa_input.usb-b", "USB Audio", "PIPEWIRE"),
    ];
    assert_eq!(
        super::device_choice_label(&devices[0], &devices),
        "USB Audio · alsa_input.usb-a"
    );
    // A native PipeWire node is low latency on Linux; off Linux the label is
    // what the body matches, so this row stays on the system side.
    #[cfg(target_os = "linux")]
    assert!(is_low_latency_device(&devices[0], "ALSA"));
}

#[test]
fn a_card_that_only_has_sysdefault_is_kept() {
    let mut devices = vec![named("alsa:sysdefault:CARD=GP5", "GP-5, USB Audio", "ALSA")];
    super::hide_redundant_alsa_aliases(&mut devices);
    assert_eq!(devices.len(), 1);
    assert_eq!(
        super::device_choice_label(&devices[0], &devices),
        "GP-5, USB Audio"
    );
}

#[test]
fn the_alsa_id_grammar_is_shared_and_pins_its_edges() {
    // The host prefix is matched case-insensitively and the PCM is trimmed.
    assert_eq!(
        super::alsa_pcm_name("ALSA:hw:CARD=0,DEV=0"),
        "hw:CARD=0,DEV=0"
    );
    assert_eq!(
        super::alsa_pcm_name("alsa: plughw:CARD=0,DEV=0 "),
        "plughw:CARD=0,DEV=0"
    );
    // Anything without the ALSA host prefix is passed through, trimmed.
    assert_eq!(super::alsa_pcm_name("wasapi:{spk}"), "wasapi:{spk}");
    assert_eq!(super::alsa_pcm_name(" hw:0 "), "hw:0");
    // Direct means the PCM starts with hw:/plughw:, not that it contains one.
    assert!(super::alsa_pcm_is_direct("plughw:CARD=X"));
    assert!(!super::alsa_pcm_is_direct("ALSA:dmix:hw:0"));
    assert!(!super::alsa_pcm_is_direct("ALSA:default"));
    // A bare host spelling is not a PCM plugin, so it has no tag.
    assert_eq!(super::device_pcm_tag("ALSA"), None);
}

#[test]
fn the_native_id_grammar_is_shared_and_pins_its_edges() {
    assert_eq!(
        super::node_name("pipewire:alsa_input.usb-Valeton-GP5"),
        Some("alsa_input.usb-Valeton-GP5")
    );
    assert!(super::is_native_id("pipewire:alsa_output.pci-0000_00_1f.3"));
    // The ALSA plugin id and a bare prefix are not nodes.
    assert!(!super::is_native_id("alsa:pipewire"));
    assert!(!super::is_native_id("pipewire"));
    assert!(!super::is_native_id("pipewire:"));
    assert!(!super::is_native_id("pipewire:pipewire"));
}

#[cfg(target_os = "linux")]
#[test]
fn direct_alsa_and_jack_are_low_latency() {
    let direct = device("ALSA:hw:CARD=0,DEV=0", "ALSA");
    assert!(is_low_latency_device(&direct, "PIPEWIRE"));
    assert!(is_low_latency_device(&direct, "JACK"));
    assert!(is_low_latency_device(
        &device("JACK:system", "JACK"),
        "PIPEWIRE"
    ));
}
