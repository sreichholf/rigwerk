//! Device discovery and the two-clock audio bridge.
//!
//! Input and output are never forced onto the same audio host.  An ASIO (or
//! any system) input feeds a ring buffer while a completely separate output
//! drains it; the output converts from the input's real rate when the two
//! differ, and the ring only has to absorb the remaining drift.
//!
//! Where things live:
//! - `types`: the public vocabulary (devices, snapshot, start settings).
//! - `engine`: the handle the UI uses to start, stop and tune the bridge.
//! - `bridge`: the bridge thread that picks a path and keeps it running.
//! - `shared`: the ring buffer, cushion and meters both sides share.
//! - `devices`: device discovery and stream configuration.
//! - `input` / `output`: the cpal capture and playback sides.
//! - `mixer` / `resampler`: turning ring frames into output frames.
//! - `eq`: the built-in 5-band EQs before and after the plugin inserts.
//! - `real_output`: REAL-style WASAPI playback (Windows).
//! - `asio`: ASIO drivers on either or both sides (Windows).

mod bridge;
pub(crate) mod clap;
mod devices;
mod engine;
mod eq;
mod input;
mod mixer;
mod output;
mod plugins;
mod resampler;
mod shared;
mod types;

#[cfg(target_os = "windows")]
mod asio;
#[cfg(target_os = "windows")]
mod real_output;

#[cfg(target_os = "windows")]
pub use real_output::probe as probe_output_driver;

#[cfg(test)]
mod hardware_tests;

#[cfg(target_os = "linux")]
pub(crate) use devices::alsa_pcm_is_direct;
pub(crate) use devices::is_low_latency_device;
pub use devices::scan_devices;
pub use engine::AudioEngine;
pub use eq::{
    BandKind, EqSettings, EqSlot, BANDS as EQ_BANDS, MAX_GAIN_DB as EQ_MAX_GAIN_DB,
    MAX_Q as EQ_MAX_Q, MIN_Q as EQ_MIN_Q,
};
#[cfg(target_os = "linux")]
pub(crate) use output::smallest_output_buffer;
pub(crate) use plugins::PluginHandle;
pub use plugins::{PluginFormat, PluginParamView, PluginPref, PluginView};
pub use types::{CushionMode, DeviceItem, EngineState, Inventory, Snapshot, StartConfig};
