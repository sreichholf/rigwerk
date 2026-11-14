use super::{
    capture_window, endpoint_kind, follower_node_name, is_alsa_shim, is_native_id, node_followers,
    node_latency_frames, node_name, path_delay_ns, playback_frames, EndpointKind, CAPTURE_NODE,
    PLAYBACK_NODE,
};
use crate::audio::output_map::{MAX_FOLLOWERS, VIRTUAL_OUT_ID};
use crate::audio::shared::{FollowerLink, FollowerPlan};

#[test]
fn the_two_stream_bridge_plays_each_pipewire_node_follower_to_its_node() {
    let node = |name: &str| FollowerLink {
        device_id: format!("pipewire:{name}"),
        node_name: name.to_owned(),
        ..FollowerLink::default()
    };
    let mut plan = FollowerPlan {
        count: 3,
        ..FollowerPlan::default()
    };
    // A node that is missing (no name yet), RigWerk Out, then a node; a node
    // past the count is not live.
    plan.slots[0] = FollowerLink {
        node_name: String::new(),
        ..node("alsa_output.usb")
    };
    plan.slots[1] = FollowerLink {
        device_id: VIRTUAL_OUT_ID.to_owned(),
        node_name: "RigWerk Out".to_owned(),
        virtual_out: true,
        publish: true,
    };
    plan.slots[2] = node("alsa_output.hdmi");
    plan.slots[3] = node("alsa_output.spdif");
    assert_eq!(
        node_followers(&plan),
        vec![(2, "alsa_output.hdmi".to_owned())]
    );
    // A generic device has no node name: its own cpal stream plays it.
    plan.slots[2] = FollowerLink {
        device_id: "alsa:hw:1,0".to_owned(),
        ..FollowerLink::default()
    };
    assert!(node_followers(&plan).is_empty());
}

#[test]
fn each_follower_slot_gets_its_own_stream_name() {
    let names: Vec<String> = (0..MAX_FOLLOWERS).map(follower_node_name).collect();
    assert_eq!(names[0], "RigWerk Follower 1");
    for (index, name) in names.iter().enumerate() {
        assert!(names[index + 1..].iter().all(|other| other != name));
        assert_ne!(name, CAPTURE_NODE);
        assert_ne!(name, PLAYBACK_NODE);
    }
}

#[test]
fn native_ids_are_node_names_and_the_alsa_plugin_is_not() {
    assert_eq!(
        node_name("pipewire:alsa_input.usb-Valeton-GP5"),
        Some("alsa_input.usb-Valeton-GP5")
    );
    assert!(is_native_id("pipewire:alsa_output.pci-0000_00_1f.3"));
    assert!(!is_native_id("alsa:pipewire"));
    assert!(!is_native_id("pipewire"));
    assert!(!is_native_id("pipewire:"));
    assert!(!is_native_id("pipewire:pipewire"));
}

#[test]
fn the_alsa_shim_is_the_pipewire_plugin() {
    assert!(is_alsa_shim("alsa:pipewire"));
    assert!(is_alsa_shim("pipewire"));
    assert!(!is_alsa_shim("pipewire:alsa_output.pci-0000_00_1f.3"));
    assert!(!is_alsa_shim("alsa:hw:CARD=GP5,DEV=0"));
    assert!(!is_alsa_shim("alsa:pulse"));
}

#[test]
fn endpoints_skip_monitors_and_our_own_nodes() {
    assert_eq!(
        endpoint_kind("alsa_input.usb-GP5", "Audio/Source", false),
        Some(EndpointKind::Source)
    );
    assert_eq!(
        endpoint_kind("alsa_output.pci-x", "Audio/Sink", false),
        Some(EndpointKind::Sink)
    );
    assert_eq!(
        endpoint_kind("easyeffects_source", "Audio/Source/Virtual", false),
        Some(EndpointKind::Source)
    );
    assert_eq!(
        endpoint_kind("alsa_output.pci-x.monitor", "Audio/Source", true),
        None
    );
    assert_eq!(
        endpoint_kind("alsa_output.pci-x.monitor", "Audio/Source", false),
        None
    );
    assert_eq!(
        endpoint_kind("RigWerk Capture", "Audio/Source", false),
        None
    );
    assert_eq!(endpoint_kind("", "Audio/Sink", false), None);
    assert_eq!(endpoint_kind("some-app", "Stream/Output", false), None);
}

#[test]
fn node_latency_follows_the_low_latency_switch() {
    assert_eq!(node_latency_frames(true, 1024), 64);
    assert_eq!(node_latency_frames(false, 256), 256);
    assert_eq!(node_latency_frames(false, 8), 32);
    assert_eq!(node_latency_frames(false, 90_000), 8192);
}

#[test]
fn playback_fills_the_requested_quantum_not_the_mapped_maxsize() {
    // Stereo f32: 8 bytes/frame. A typical map is 8192 frames; the cycle asks for 64.
    assert_eq!(playback_frames(8192 * 8, 8, 64), 64);
    assert_eq!(playback_frames(8192 * 8, 8, 0), 8192);
    assert_eq!(playback_frames(128 * 8, 8, 1024), 128);
    assert_eq!(playback_frames(64, 8, 16), 8);
    assert_eq!(playback_frames(0, 8, 64), 0);
}

#[test]
fn capture_reads_from_the_chunk_offset_with_its_stride() {
    // Packed stereo f32, no offset: every byte of `size` is frames.
    let window = capture_window(8192, 0, 64 * 8, 0, 2);
    assert_eq!((window.start, window.stride, window.frames), (0, 8, 64));
    // An offset moves the start, and the size is bounded by what is
    // mapped after it.
    let window = capture_window(1024, 1000, 512, 8, 2);
    assert_eq!((window.start, window.frames), (1000, 3));
    // The offset is taken modulo the mapped size.
    assert_eq!(capture_window(1024, 1024 + 16, 64, 8, 2).start, 16);
    // A wider stride than the samples (padding) is honoured; the last
    // frame does not need trailing padding.
    let window = capture_window(4096, 0, 16 * 3 + 8, 16, 2);
    assert_eq!((window.stride, window.frames), (16, 4));
    // A stride too small for the channels falls back to packed frames.
    assert_eq!(capture_window(4096, 0, 64, 4, 2).stride, 8);
    assert_eq!(capture_window(4096, 0, 64, -8, 2).stride, 8);
    // Nothing mapped, or less than one frame, yields no frames.
    assert_eq!(capture_window(0, 0, 64, 8, 2).frames, 0);
    assert_eq!(capture_window(4096, 0, 7, 8, 2).frames, 0);
}

#[test]
fn path_delay_converts_graph_ticks_to_nanoseconds() {
    // 147.520833 ms at 48 kHz ≈ the AKG A2DP sink report.
    assert_eq!(path_delay_ns(7081, 1, 48_000), Some(147_520_833));
    assert_eq!(path_delay_ns(0, 1, 48_000), Some(0));
    assert_eq!(path_delay_ns(-100, 1, 48_000), Some(0));
    assert_eq!(path_delay_ns(64, 1, 0), None);
}

#[test]
fn listing_nodes_returns_whether_or_not_a_server_is_running() {
    match super::list_nodes() {
        Ok(nodes) => {
            assert!(nodes
                .sources
                .iter()
                .chain(nodes.sinks.iter())
                .all(|device| is_native_id(&device.id)));
        }
        Err(error) => assert!(!error.is_empty()),
    }
}
