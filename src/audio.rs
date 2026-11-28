//! Input and output are separate clocks.  The ring absorbs the drift
//! between them.
//!
//! A callback allocates nothing, takes no blocking lock, and must not let
//! a panic unwind into the host.

mod asio_panel;
mod bridge;
pub(crate) mod cab;
mod calibration;
pub(crate) mod clap;
mod delay;
mod denormal;
mod devices;
mod engine;
mod eq;
mod extra_output;
pub(crate) mod nam;
#[cfg(target_os = "windows")]
pub(crate) use extra_output::is_generic_extra_output;
pub(crate) use extra_output::is_vb_cable_render;
pub(crate) use extra_output::EXTRA_OUTPUT_DRIFT_LIMIT_PPM;
mod gate;
mod input;
mod limiter;
mod loudness;
pub(crate) mod lv2;
mod metronome;
mod mixer;
mod output;
mod output_map;
mod panic_guard;
mod path;
mod pitch;
mod plugins;
pub(crate) use plugins::mark_main_thread;
#[cfg(test)]
pub(crate) use plugins::{set_test_loader, TEST_PATH};
mod record;
mod resampler;
mod ring;
pub(crate) mod rt_guard;
mod scope;
pub(crate) mod serde_util;
mod shared;
mod thread_priority;
mod tuner;
mod types;
pub(crate) mod vst3;

#[cfg(target_os = "windows")]
mod win;

#[cfg(any(target_os = "linux", test))]
mod linux;

#[cfg(target_os = "windows")]
pub use win::wasapi::probe as probe_output_driver;

#[cfg(test)]
mod hardware_tests;

pub(crate) use asio_panel::AsioPanelSide;
pub use cab::{
    CabControls, CabNote, CabSettings, CAB_MAX_HIGH_CUT_HZ, CAB_MAX_LEVEL_DB, CAB_MAX_LOW_CUT_HZ,
    CAB_MAX_MS, CAB_MAX_TAPS, CAB_MIN_LEVEL_DB,
};
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
    GateKey, GateSettings, GateSlot, MAX_ATTACK_MS as GATE_MAX_ATTACK_MS,
    MAX_HOLD_MS as GATE_MAX_HOLD_MS, MAX_HYSTERESIS_DB as GATE_MAX_HYSTERESIS_DB,
    MAX_RANGE_DB as GATE_MAX_RANGE_DB, MAX_RELEASE_MS as GATE_MAX_RELEASE_MS,
    MAX_THRESHOLD_DB as GATE_MAX_THRESHOLD_DB, MIN_ATTACK_MS as GATE_MIN_ATTACK_MS,
    MIN_HOLD_MS as GATE_MIN_HOLD_MS, MIN_HYSTERESIS_DB as GATE_MIN_HYSTERESIS_DB,
    MIN_RANGE_DB as GATE_MIN_RANGE_DB, MIN_RELEASE_MS as GATE_MIN_RELEASE_MS,
    MIN_THRESHOLD_DB as GATE_MIN_THRESHOLD_DB,
};
#[cfg(test)]
pub(crate) use limiter::safety_ceiling_linear;
pub(crate) use limiter::{SAFETY_CEILING_DB, SAFETY_GR_DB};
#[cfg(target_os = "linux")]
pub(crate) use linux::alsa::alsa_pcm_is_direct;
#[cfg(target_os = "linux")]
pub(crate) use linux::alsa::{planned_output_period, smallest_output_buffer};
#[cfg(target_os = "linux")]
pub(crate) use linux::hotplug;
#[cfg(target_os = "linux")]
pub(crate) use linux::native::registry as pipewire_registry;
#[cfg(target_os = "linux")]
pub(crate) use linux::native::{is_native_id, node_latency_frames, node_name as linux_node_name};
#[cfg(target_os = "linux")]
pub(crate) use linux::pipewire::{check_output as pipewire_check_output, is_pipewire_device};
pub use loudness::LoudnessReading;
pub(crate) use loudness::SILENCE_LUFS;
pub(crate) use metronome::{clamp_click_level_db, clamp_metronome_bpm, click_level_gain};
pub use metronome::{
    ClickRhythm, CLICK_LEVEL_DEFAULT_DB, CLICK_LEVEL_MAX_DB, CLICK_LEVEL_MIN_DB, CLICK_RHYTHMS,
    METRONOME_BPM_DEFAULT, METRONOME_BPM_MAX, METRONOME_BPM_MIN,
};
#[cfg(test)]
pub(crate) use mixer::OutputMixer;
pub use nam::{
    LegacyNamSettings, NamBlockControls, NamBlockSettings, NamPosition, NamStack, MAX_NAM_BLOCKS,
    NAM_MAX_TRIM_DB, NAM_MIN_TRIM_DB, NAM_SOFT_LIMIT,
};
pub(crate) use output_map::low_bits;
pub use output_map::PATH_TRIM_MAX_MS;
pub use output_map::{
    AlignmentReport, ChannelMap, MapPreset, MixSide, OutputDeviceSettings, PathMapSettings,
    UnusedChannels, DEVICE_LEVEL_MAX_DB, DEVICE_LEVEL_MIN_DB, LOUDNESS_TARGET_DEFAULT_LUFS,
    LOUDNESS_TARGET_MAX_LUFS, LOUDNESS_TARGET_MIN_LUFS, LOUDNESS_TRIM_MAX_DB, MAX_EXTRA_OUTPUTS,
    MAX_OUTPUT_CHANNELS, VIRTUAL_OUTPUT_ID, VIRTUAL_OUTPUT_NAME,
};
pub(crate) use path::path_level_gain;
pub(crate) use path::PathState;
pub use path::{
    sanitize_path_level, sanitize_path_match, sanitize_send, DspTime, LockStats, PathFeed, PathMix,
    PluginLockReport, MAX_PATHS, PATH_LEVEL_MAX_DB, PATH_LEVEL_MIN_DB, PATH_MATCH_MAX_DB,
    SEND_OFF_DB,
};
pub(crate) use pitch::pitch_latency_samples;
pub use pitch::{
    PitchEngine, PitchMode, PitchSettings, PITCH_MAX_CENTS, PITCH_MAX_DELAY_MS,
    PITCH_MAX_SEMITONES, PITCH_MIN_CENTS, PITCH_MIN_DELAY_MS, PITCH_MIN_SEMITONES,
};
#[cfg(test)]
pub(crate) use plugins::vst3_fixture;
pub(crate) use plugins::PluginHandle;
pub(crate) use plugins::MAX_ALIGN;
pub(crate) use plugins::MAX_EFFECTS;
pub use plugins::{PluginFormat, PluginParamView, PluginPref, PluginView};
pub(crate) use record::{RecordPhase, RecordSource, RecordStatus};
#[cfg(test)]
pub(crate) use shared::tests::claim_main_thread;
pub(crate) use shared::CallbackFailure;
pub(crate) use shared::{ExtraOutputLink, ExtraOutputPlan, SCOPE_LEN};
pub(crate) use tuner::{
    cents_to_ratio, note_name, octave, tuning_index, TuneState, TunerAnalysis, TunerChannel,
    TunerFrame, TunerTolerance, Tuning, TuningGroup, DEFAULT_PRESET_ID, TAP_COPY_FRAMES, TUNINGS,
};
#[cfg(test)]
pub(crate) use tuner::{tuning_by_id, TunerReading};
pub use types::{
    BufferMode, DeviceItem, EngineState, Inventory, OutputMode, Snapshot, StartConfig,
    WasapiOutputMode,
};
#[cfg(target_os = "linux")]
pub use types::{QuantumInfo, PIPEWIRE_LOW_QUANTUM};
