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
//! - `linux/sink_monitor`: the clock sink's monitor peak under Low latency.
//! - `linux/alsa`: direct ALSA hardware PCMs and the output period.
//!
//! The rest of the audio side is backend-neutral:
//! - `types`: the public vocabulary (devices, snapshot, start settings).
//! - `engine`: the handle the UI uses to start, stop and tune the bridge.
//! - `bridge`: the bridge thread that picks a path and keeps it running.
//! - `shared`: the ring buffer, cushion, meters and tuner tap both sides share.
//! - `path`: one signal path's controls (routing, EQs, gate, inserts).
//! - `devices`: device discovery and stream configuration.
//! - `input` / `output`: the cpal capture and playback sides.
//! - `mixer` / `resampler`: turning ring frames into output frames.
//! - `calibration`: the click, the capture buffer and the correlator behind
//!   the measured loopback (the estimate stays in `app::budget`).
//! - `follower`: generic multi-output (each follower mixed and mapped on the
//!   clock callback → own ring + cpal stream).
//! - `ring`: lock-free multi-lane SPSC ring for each generic follower.
//! - `eq`: the built-in 5-band EQs before and after the plugin inserts.
//! - `gate`: the built-in noise gate (PRE or POST inserts).
//! - `tuner`: YIN pitch detection and the preset-guided per-string analysis.
//! - `output_map`: per-device level, mute, safety ceiling and channel map.
//! - `limiter`: the per-device sample-peak ceiling after the device level.
//! - `delay`: the delay lines and taps behind the latency alignment.
//! - `denormal`: flush-to-zero for the audio callbacks.
//! - `thread_priority`: real-time scheduling for cpal's Linux threads.

mod bridge;
mod calibration;
pub(crate) mod clap;
mod delay;
mod denormal;
mod devices;
mod engine;
mod eq;
mod follower;
#[cfg(target_os = "windows")]
pub(crate) use follower::is_generic_follower;
pub(crate) use follower::is_vb_cable_render;
pub(crate) use follower::FOLLOWER_DRIFT_LIMIT_PPM;
mod gate;
mod input;
mod limiter;
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
mod tuner;
mod types;

#[cfg(target_os = "windows")]
mod win;

#[cfg(any(target_os = "linux", test))]
mod linux;

#[cfg(target_os = "windows")]
pub use win::wasapi::probe as probe_output_driver;

#[cfg(test)]
mod hardware_tests;

pub(crate) use calibration::{
    analyze_channels, click_waveform, round_trip_ms, CalibrationFailure, CalibrationState,
    MAX_ROUND_TRIP_MS,
};
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
#[cfg(test)]
pub(crate) use mixer::OutputMixer;
pub use output_map::{
    AlignmentReport, ChannelMap, MapPreset, MixSide, OutputDeviceSettings, PathMapSettings,
    UnusedChannels, DEVICE_LEVEL_MAX_DB, DEVICE_LEVEL_MIN_DB, MAX_FOLLOWERS, MAX_OUTPUT_CHANNELS,
    VIRTUAL_OUT_ID, VIRTUAL_OUT_NAME,
};
#[cfg(test)]
pub(crate) use shared::tests::claim_main_thread;
// Ranges for the path level, the per-device path level and the trim.
#[cfg(test)]
pub(crate) use limiter::safety_ceiling_linear;
pub(crate) use limiter::{SAFETY_CEILING_DB, SAFETY_GR_DB};
pub use output_map::PATH_TRIM_MAX_MS;
pub(crate) use path::path_level_gain;
pub(crate) use path::PathState;
pub use path::{
    sanitize_path_level, DspTime, LockStats, PathMix, PluginLockReport, MAX_PATHS,
    PATH_LEVEL_MAX_DB, PATH_LEVEL_MIN_DB,
};
pub(crate) use plugins::service_clap;
pub(crate) use plugins::MAX_ALIGN;
pub(crate) use plugins::{instance_id as plugin_instance_id, PluginHandle};
pub use plugins::{PluginFormat, PluginParamView, PluginPref, PluginView};
pub(crate) use shared::{FollowerLink, FollowerPlan, SCOPE_LEN};
pub(crate) use tuner::{
    cents_to_ratio, note_name, octave, tuning_index, StringReading, TuneState, TunerAnalysis,
    TunerChannel, TunerFrame, TunerTolerance, Tuning, TuningGroup, CENTER_CENTS, DEFAULT_PRESET_ID,
    PRESETS, TAP_COPY_FRAMES,
};
#[cfg(test)]
pub(crate) use tuner::{tuning_by_id, TunerReading};
#[cfg(target_os = "linux")]
pub use types::QuantumInfo;
pub use types::{
    CushionMode, DeviceItem, EngineState, Inventory, Snapshot, StartConfig, WasapiOutputMode,
    PIPEWIRE_LOW_QUANTUM,
};
