//! Device discovery and the two-clock audio bridge.
//!
//! Input and output are never forced onto the same audio host.  An ASIO (or
//! any system) input feeds a ring buffer while a completely separate output
//! drains it; the output converts from the input's real rate when the two
//! differ, and the ring only has to absorb the remaining drift.
//!
//! Backends live in their own platform folders:
//! - `win/asio`: ASIO drivers on either or both sides (Windows).
//! - `win/wasapi`: REAL-style WASAPI playback (Windows).
//! - `linux/native`: PipeWire sources, sinks, and RigWerk's own nodes.
//! - `linux/pipewire`: the desktop-wide quantum switch.
//! - `linux/alsa`: direct ALSA hardware PCMs and the output period.
//!
//! The rest of the audio side is backend-neutral:
//! - `types`: the public vocabulary (devices, snapshot, start settings).
//! - `engine`: the handle the UI uses to start, stop and tune the bridge.
//! - `bridge`: the bridge thread that picks a path and keeps it running.
//! - `shared`: the ring buffer, cushion and meters both sides share.
//! - `path`: one signal path's controls (routing, EQs, gate, inserts).
//! - `devices`: device discovery and stream configuration.
//! - `input` / `output`: the cpal capture and playback sides.
//! - `mixer` / `resampler`: turning ring frames into output frames.
//! - `follower`: generic multi-output (finished mix → own ring + cpal stream).
//! - `ring`: lock-free stereo SPSC used by the clock bridge and followers.
//! - `eq`: the built-in 5-band EQs before and after the plugin inserts.
//! - `gate`: the built-in noise gate (PRE or POST inserts).
//! - `output_map`: per-device level, mute and channel map.
//! - `denormal`: flush-to-zero for the audio callbacks.
//! - `thread_priority`: real-time scheduling for cpal's Linux threads.

mod bridge;
pub(crate) mod clap;
mod denormal;
mod devices;
mod engine;
mod eq;
mod follower;
#[cfg(target_os = "windows")]
pub(crate) use follower::is_generic_follower;
pub(crate) use follower::is_vb_cable_render;
mod gate;
mod input;
mod mixer;
mod output;
mod output_map;
mod path;
mod plugins;
mod resampler;
mod ring;
mod scope;
mod shared;
mod thread_priority;
mod types;

#[cfg(target_os = "windows")]
mod win;

#[cfg(any(target_os = "linux", test))]
mod linux;

#[cfg(target_os = "windows")]
pub use win::wasapi::probe as probe_output_driver;

#[cfg(test)]
mod hardware_tests;

pub(crate) use devices::device_choice_label;
pub(crate) use devices::is_low_latency_device;
pub(crate) use devices::placeholder_inventory;
pub use devices::scan_devices;
pub(crate) use devices::scan_devices_from;
pub use engine::AudioEngine;
pub use eq::{
    BandKind, EqBand, EqSettings, EqSlot, BANDS as EQ_BANDS, MAX_GAIN_DB as EQ_MAX_GAIN_DB,
    MAX_Q as EQ_MAX_Q, MIN_Q as EQ_MIN_Q,
};
pub use gate::{
    GateSettings, GateSlot, MAX_ATTACK_MS as GATE_MAX_ATTACK_MS, MAX_HOLD_MS as GATE_MAX_HOLD_MS,
    MAX_HYSTERESIS_DB as GATE_MAX_HYSTERESIS_DB, MAX_RANGE_DB as GATE_MAX_RANGE_DB,
    MAX_RELEASE_MS as GATE_MAX_RELEASE_MS, MAX_THRESHOLD_DB as GATE_MAX_THRESHOLD_DB,
    MIN_ATTACK_MS as GATE_MIN_ATTACK_MS, MIN_HOLD_MS as GATE_MIN_HOLD_MS,
    MIN_HYSTERESIS_DB as GATE_MIN_HYSTERESIS_DB, MIN_RANGE_DB as GATE_MIN_RANGE_DB,
    MIN_RELEASE_MS as GATE_MIN_RELEASE_MS, MIN_THRESHOLD_DB as GATE_MIN_THRESHOLD_DB,
};
#[cfg(target_os = "linux")]
pub(crate) use linux::alsa::alsa_pcm_is_direct;
#[cfg(target_os = "linux")]
pub(crate) use linux::alsa::{planned_output_period, smallest_output_buffer};
#[cfg(target_os = "linux")]
pub(crate) use linux::hotplug;
#[cfg(target_os = "linux")]
pub(crate) use linux::native::{is_native_id, node_latency_frames, node_name as linux_node_name};
#[cfg(target_os = "linux")]
pub(crate) use linux::pipewire::{check_output as pipewire_check_output, is_pipewire_device};
pub use output_map::{
    sanitize_path_level, ChannelMap, MapPreset, MixSide, OutputDeviceSettings, PathMapSettings,
    UnusedChannels, DEVICE_LEVEL_MAX_DB, DEVICE_LEVEL_MIN_DB, MAX_FOLLOWERS, MAX_OUTPUT_CHANNELS,
    VIRTUAL_OUT_ID, VIRTUAL_OUT_NAME,
};
// Ranges for the path level, the per-device path level and the trim, for
// the path pages (U1/U2).
#[cfg(test)]
pub(crate) use mixer::OutputMixer;
pub(crate) use output_map::path_level_gain;
#[allow(unused_imports)]
pub use output_map::{PATH_LEVEL_MAX_DB, PATH_LEVEL_MIN_DB, PATH_TRIM_MAX_MS};
pub(crate) use path::PathState;
pub use path::{PathMix, MAX_PATHS};
#[cfg(target_os = "windows")]
pub(crate) use plugins::service_clap;
pub(crate) use plugins::{instance_id as plugin_instance_id, PluginHandle};
pub use plugins::{PluginFormat, PluginParamView, PluginPref, PluginView};
pub(crate) use shared::{FollowerLink, FollowerPlan, SCOPE_LEN};
#[cfg(target_os = "linux")]
pub use types::QuantumInfo;
pub use types::{
    CushionMode, DeviceItem, EngineState, Inventory, Snapshot, StartConfig, WasapiOutputMode,
    PIPEWIRE_LOW_QUANTUM,
};
