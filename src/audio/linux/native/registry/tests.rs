use std::collections::HashMap;

use super::{props_endpoint, EndpointKind};

fn kind(props: &[(&'static str, &'static str)]) -> Option<EndpointKind> {
    let props: HashMap<&str, &'static str> = props.iter().copied().collect();
    props_endpoint(|key| props.get(key).copied())
}

#[test]
fn sources_and_sinks_wake_a_scan() {
    assert_eq!(
        kind(&[
            ("node.name", "alsa_input.usb-GP5"),
            ("media.class", "Audio/Source")
        ]),
        Some(EndpointKind::Source)
    );
    assert_eq!(
        kind(&[("node.name", "test_sink"), ("media.class", "Audio/Sink")]),
        Some(EndpointKind::Sink)
    );
    assert_eq!(
        kind(&[
            ("node.name", "easyeffects_sink"),
            ("media.class", "Audio/Sink/Virtual")
        ]),
        Some(EndpointKind::Sink)
    );
}

#[test]
fn our_own_nodes_streams_and_monitors_do_not() {
    assert_eq!(
        kind(&[
            ("node.name", "RigWerk Capture"),
            ("media.class", "Stream/Input/Audio")
        ]),
        None
    );
    assert_eq!(
        kind(&[("node.name", "rigwerk_sink"), ("media.class", "Audio/Sink")]),
        None
    );
    assert_eq!(
        kind(&[
            ("node.name", "firefox"),
            ("media.class", "Stream/Output/Audio")
        ]),
        None
    );
    assert_eq!(
        kind(&[
            ("node.name", "alsa_input.usb-GP5"),
            ("media.class", "Audio/Source"),
            ("stream.monitor", "true")
        ]),
        None
    );
    assert_eq!(
        kind(&[
            ("node.name", "test_sink.monitor"),
            ("media.class", "Audio/Source")
        ]),
        None
    );
}

#[test]
fn a_node_without_a_name_or_class_does_not() {
    assert_eq!(kind(&[("media.class", "Audio/Sink")]), None);
    assert_eq!(
        kind(&[("node.name", ""), ("media.class", "Audio/Sink")]),
        None
    );
    assert_eq!(kind(&[("node.name", "midi_bridge")]), None);
}
