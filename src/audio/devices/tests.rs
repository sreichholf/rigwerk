use super::{dedup_devices, is_low_latency_device};
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
fn pipewire_is_the_low_latency_backend() {
    let pipewire = device("ALSA:pipewire", "ALSA");
    assert!(is_low_latency_device(&pipewire, "PIPEWIRE"));
    assert!(is_low_latency_device(&pipewire, "ALSA"));
}

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
fn direct_alsa_and_jack_are_low_latency() {
    let direct = device("ALSA:hw:CARD=0,DEV=0", "ALSA");
    assert!(is_low_latency_device(&direct, "PIPEWIRE"));
    assert!(is_low_latency_device(&direct, "JACK"));
    assert!(is_low_latency_device(
        &device("JACK:system", "JACK"),
        "PIPEWIRE"
    ));
}
