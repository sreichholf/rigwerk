//! A small VST3 and CLAP insert chain for the master bus.
//!
//! Plugins are discovered and loaded through `vst3-host` or the CLAP host in
//! [`super::clap`]; the chain lives in [`super::shared::Shared`] so the playback
//! callback can process it.  The UI only ever holds descriptors, never the
//! plugin objects themselves.

mod chain;
mod load;
mod pref;
mod slot;

#[cfg(test)]
mod tests;

use std::sync::atomic::{AtomicBool, Ordering};

/// The largest latency, in frames, one insert may report: the size the
/// alignment delay lines are built for (about 170 ms at 192 kHz).  A larger
/// report is clamped, so a broken value cannot blow up the alignment.
pub(crate) const MAX_ALIGN: u32 = 1 << 15;

/// Clamps a latency a plugin reported to [`MAX_ALIGN`].  Call it where the
/// value is read, on the main thread: the first clamp is logged.
pub(crate) fn clamp_latency(latency: u32) -> u32 {
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if latency > MAX_ALIGN {
        if !LOGGED.swap(true, Ordering::Relaxed) {
            eprintln!("[plugins] a plugin reports {latency} samples of latency; using {MAX_ALIGN}");
        }
        MAX_ALIGN
    } else {
        latency
    }
}

pub use pref::{PluginFormat, PluginParamView, PluginPref, PluginView};

pub(crate) use chain::{PluginChain, SlotLink};
pub(crate) use load::{
    capture_state, host_format_parameter, host_set_parameter, instance_id, read_plugin_parameters,
    service_clap, take_parameter_changes, take_parameter_values, PluginHandle, Restore,
    Vst3Service, MAX_BLOCK,
};
pub(crate) use slot::drain_deferred_plugins;

#[cfg(test)]
pub(crate) use slot::{deferred_slot_count, tracked_slots_alive};
