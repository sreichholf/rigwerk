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
//! - `win/output`: which clock output Windows opens and in what order, and its
//!   "device in use" answer (Windows).
//! - `linux/native`: PipeWire sources, sinks, and RigWerk's own nodes.
//! - `linux/pipewire`: the desktop-wide quantum switch.
//! - `linux/sink_monitor`: the clock sink's monitor peak under Low latency.
//! - `linux/alsa`: direct ALSA hardware PCMs and the output period.  The PCM
//!   id grammar is `devices`'s; this module re-exports it.
//!
//! The rest of the audio side is backend-neutral:
//! - `types`: the public vocabulary (devices, snapshot, start settings).
//! - `engine`: the handle the UI uses to start, stop and tune the bridge.
//! - `asio_panel`: the UI's request to open an ASIO control panel.  The
//!   bridge thread that owns the driver takes it; the call is Windows-only.
//! - `bridge`: the bridge thread that picks a path and keeps it running.
//! - `shared`: the ring buffer, bridge buffer, meters and tuner tap both sides share.
//! - `path`: one signal path's controls (routing, EQs, gate, effects).
//! - `plugins`: the VST3, CLAP and LV2 chain on each path.  The slot owns each
//!   plugin's processor; the UI reaches the rest through its handle and
//!   holds views.  The audio side owns each chain's runtime and the UI
//!   edits its model; edits cross as commands, never under a shared lock.
//! - `vst3`: RigWerk's own VST3 host on the `vst3` bindings (module,
//!   factory, host context, run loop, streams, the controller and
//!   processor halves, the editor view) and the bundle scan, which reads
//!   `moduleinfo.json` and never loads a binary.
//! - `clap`: the CLAP host (the main-thread instance, the audio-thread
//!   processor, the editor).
//! - `lv2`: RigWerk's own LV2 effect host on the `lv2-sys` bindings (the
//!   UI-thread instance, the audio-thread processor) and the bundle scan,
//!   which reads the Turtle metadata and never loads a binary.
//! - `scope`: decimated peak-hold lanes.  Drawing them is `ui/scope`.
//! - `devices`: device discovery and stream configuration.
//! - `input` / `output`: the cpal capture and playback sides.
//! - `mixer` / `resampler`: turning ring frames into output frames.
//! - `calibration`: the click, the capture buffer and the correlator behind
//!   the measured loopback (the estimate stays in `app::budget`).
//! - `metronome`: the click generator.  One voice, mixed onto device
//!   channels after the safety ceiling.
//! - `extra_output`: generic multi-output (each extra output mixed and mapped on the
//!   clock callback → own ring + cpal stream).
//! - `ring`: lock-free multi-lane SPSC ring for each generic extra output.
//! - `record`: one armed endpoint into a WAV file.  The callback only
//!   pushes; a writer thread owns the disk.
//! - `eq`: the built-in 5-band EQs before and after the plugin effects.
//! - `gate`: the built-in noise gate (PRE or POST effects).
//! - `nam`: the built-in Neural Amp Modeler blocks (before or after the
//!   effects, up to sixteen of them).  The callback never builds or drops a
//!   model, never loads one, and keeps each block's runtime state with the
//!   block's id.
//! - `cab`: the built-in cabinet impulse response, after the effects and
//!   after the NAM blocks.  The first 128 taps are direct; the rest is
//!   partitioned overlap-save.  The callback never reads a file, resamples,
//!   or drops a kernel.
//! - `tuner`: YIN pitch detection and the preset-guided per-string analysis.
//! - `output_map`: per-device level, mute, safety ceiling and channel map.
//! - `limiter`: the per-device sample-peak ceiling after the device level.
//! - `loudness`: ITU-R BS.1770-4 momentary, short-term and integrated metres
//!   and the slow automatic level that uses them, one per output device.
//! - `delay`: the delay lines and taps behind the latency alignment.
//! - `denormal`: flush-to-zero for the audio callbacks.
//! - `rt_guard`: the debug allocation guard; every callback's sample body
//!   runs inside one, and ASIO's silence path is covered through `dispatch`.
//! - `thread_priority`: real-time scheduling for cpal's Linux threads.

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
mod path;
mod plugins;
pub(crate) use plugins::mark_main_thread;
#[cfg(test)]
pub(crate) use plugins::{set_test_loader, TEST_PATH};
mod record;
mod resampler;
mod ring;
pub(crate) mod rt_guard;
mod scope;
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
#[cfg(test)]
pub(crate) use mixer::OutputMixer;
pub use nam::{
    LegacyNamSettings, NamBlockControls, NamBlockSettings, NamPosition, NamStack, MAX_NAM_BLOCKS,
    NAM_MAX_TRIM_DB, NAM_MIN_TRIM_DB, NAM_SOFT_LIMIT,
};
pub(crate) use output_map::low_bits;
pub use output_map::{
    AlignmentReport, ChannelMap, MapPreset, MixSide, OutputDeviceSettings, PathMapSettings,
    UnusedChannels, DEVICE_LEVEL_MAX_DB, DEVICE_LEVEL_MIN_DB, LOUDNESS_TARGET_DEFAULT_LUFS,
    LOUDNESS_TARGET_MAX_LUFS, LOUDNESS_TARGET_MIN_LUFS, LOUDNESS_TRIM_MAX_DB, MAX_EXTRA_OUTPUTS,
    MAX_OUTPUT_CHANNELS, VIRTUAL_OUTPUT_ID, VIRTUAL_OUTPUT_NAME,
};
#[cfg(test)]
pub(crate) use shared::tests::claim_main_thread;
// Ranges for the path level, the per-device path level and the trim.
#[cfg(test)]
pub(crate) use limiter::safety_ceiling_linear;
pub(crate) use limiter::{SAFETY_CEILING_DB, SAFETY_GR_DB};
pub use loudness::LoudnessReading;
pub(crate) use loudness::SILENCE_LUFS;
pub(crate) use metronome::{clamp_click_level_db, clamp_metronome_bpm, click_level_gain};
pub use metronome::{
    ClickRhythm, CLICK_LEVEL_DEFAULT_DB, CLICK_LEVEL_MAX_DB, CLICK_LEVEL_MIN_DB, CLICK_RHYTHMS,
    METRONOME_BPM_DEFAULT, METRONOME_BPM_MAX, METRONOME_BPM_MIN,
};
pub use output_map::PATH_TRIM_MAX_MS;
pub(crate) use path::path_level_gain;
pub(crate) use path::PathState;
pub use path::{
    sanitize_path_level, sanitize_path_match, sanitize_send, DspTime, LockStats, PathFeed, PathMix,
    PluginLockReport, MAX_PATHS, PATH_LEVEL_MAX_DB, PATH_LEVEL_MIN_DB, PATH_MATCH_MAX_DB,
    SEND_OFF_DB,
};
#[cfg(test)]
pub(crate) use plugins::vst3_fixture;
pub(crate) use plugins::PluginHandle;
pub(crate) use plugins::MAX_ALIGN;
pub(crate) use plugins::MAX_EFFECTS;
pub use plugins::{PluginFormat, PluginParamView, PluginPref, PluginView};
pub(crate) use record::{RecordPhase, RecordSource, RecordStatus};
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
