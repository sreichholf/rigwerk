use std::sync::atomic::Ordering;

use super::{
    capture_window, endpoint_kind, extra_output_node_name, is_alsa_shim, is_native_id,
    node_extra_outputs, node_latency_frames, path_delay_ns, playback_frames, push_capture,
    EndpointKind, CAPTURE_NODE, PLAYBACK_NODE,
};
use crate::audio::output_map::{MAX_EXTRA_OUTPUTS, VIRTUAL_OUTPUT_ID};
use crate::audio::shared::{ExtraOutputLink, ExtraOutputPlan, Shared};

#[test]
fn the_two_stream_bridge_plays_each_pipewire_node_extra_output_to_its_node() {
    let node = |name: &str| ExtraOutputLink {
        device_id: format!("pipewire:{name}"),
        node_name: name.to_owned(),
        ..ExtraOutputLink::default()
    };
    let mut plan = ExtraOutputPlan {
        count: 3,
        ..ExtraOutputPlan::default()
    };
    // A node that is missing (no name yet), RigWerk Out, then a node; a node
    // past the count is not live.
    plan.slots[0] = ExtraOutputLink {
        node_name: String::new(),
        ..node("alsa_output.usb")
    };
    plan.slots[1] = ExtraOutputLink {
        device_id: VIRTUAL_OUTPUT_ID.to_owned(),
        node_name: "RigWerk Out".to_owned(),
        virtual_output: true,
        virtual_output_on: true,
    };
    plan.slots[2] = node("alsa_output.hdmi");
    plan.slots[3] = node("alsa_output.spdif");
    assert_eq!(
        node_extra_outputs(&plan),
        vec![(2, "alsa_output.hdmi".to_owned())]
    );
    // A generic device has no node name: its own cpal stream plays it.
    plan.slots[2] = ExtraOutputLink {
        device_id: "alsa:hw:1,0".to_owned(),
        ..ExtraOutputLink::default()
    };
    assert!(node_extra_outputs(&plan).is_empty());
}

#[test]
fn each_extra_output_slot_gets_its_own_stream_name() {
    let names: Vec<String> = (0..MAX_EXTRA_OUTPUTS).map(extra_output_node_name).collect();
    assert_eq!(names[0], "RigWerk Extra output 1");
    for (index, name) in names.iter().enumerate() {
        assert!(names[index + 1..].iter().all(|other| other != name));
        assert_ne!(name, CAPTURE_NODE);
        assert_ne!(name, PLAYBACK_NODE);
    }
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

/// Packed stereo f32 little-endian, the layout `push_capture` reads when the
/// chunk has no padding.
fn packed_stereo(frames: &[(f32, f32)]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(frames.len() * 8);
    for &(left, right) in frames {
        bytes.extend_from_slice(&left.to_le_bytes());
        bytes.extend_from_slice(&right.to_le_bytes());
    }
    bytes
}

fn peaks(shared: &Shared) -> (f32, f32) {
    (
        f32::from_bits(shared.in_l.load(Ordering::Relaxed)),
        f32::from_bits(shared.in_r.load(Ordering::Relaxed)),
    )
}

/// The tap is taken before the channel enable.  Swapping the two would leave
/// the tuner silent on a muted side while the ring still looked correct.
#[test]
fn a_muted_capture_channel_still_reaches_the_tap() {
    let shared = Shared::new(48_000);
    shared.set_master(false, [false, true], true);
    let frames = [(0.5, 0.25), (-0.75, 0.125)];
    let bytes = packed_stereo(&frames);

    push_capture(&shared, &bytes, 2, 8, frames.len());

    let pushed: Vec<(f32, f32)> = std::iter::from_fn(|| shared.pop_frame()).collect();
    assert_eq!(pushed, vec![(0.0, 0.25), (0.0, 0.125)]);
    let mut tapped = vec![0.0; frames.len() * 2];
    assert_eq!(shared.copy_tap(&mut tapped, frames.len()), frames.len());
    assert_eq!(tapped, vec![0.5, 0.25, -0.75, 0.125]);
    assert_eq!(peaks(&shared), (0.0, 0.25));
}

/// A one-channel chunk repeats its only sample on both sides before the
/// stage, which takes a full pair.  The rule stays in this file.
#[test]
fn a_mono_capture_chunk_copies_its_sample_to_both_sides() {
    let shared = Shared::new(48_000);
    let mono = [0.5f32, -0.25, 0.125];
    let mut bytes = Vec::with_capacity(mono.len() * 4);
    for sample in mono {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }

    push_capture(&shared, &bytes, 1, 4, mono.len());

    let pushed: Vec<(f32, f32)> = std::iter::from_fn(|| shared.pop_frame()).collect();
    assert_eq!(pushed, vec![(0.5, 0.5), (-0.25, -0.25), (0.125, 0.125)]);
    assert_eq!(peaks(&shared), (0.5, 0.5));
}
