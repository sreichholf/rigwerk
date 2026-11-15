//! VST3 and CLAP insert chains, one per signal path.
//!
//! Plugins are discovered and loaded through `vst3-host` or the CLAP host in
//! [`super::clap`]; each path's chain lives on that path in
//! [`super::shared::Shared`] so the playback callback can process it.  The UI
//! only ever holds descriptors, never the plugin objects themselves.
//!
//! Each loaded plugin is split in two (`handle`): the processor the chain's
//! slot runs on the audio thread, and the [`PluginHandle`] the UI reaches
//! through the published insert links.  The handle keeps the views, the
//! saved values and the state blob behind its own UI-only lock, so the
//! chain lock is only taken for structural edits.

mod chain;
#[cfg(test)]
mod fixtures;
mod handle;
mod link;
mod load;
mod pause;
mod pref;
mod slot;
mod transfer;

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

pub(crate) use chain::PluginChain;
pub(crate) use handle::PluginHandle;
#[cfg(test)]
pub(crate) use handle::{LoadedPlugin, Outcome, Saved};
pub(crate) use link::{
    ClapFailure, ParamIds, ParamOut, PluginShared, ProcessFailure, RESTART_ACTIVATION,
};
pub(crate) use load::{Restore, MAX_BLOCK};
#[cfg(test)]
pub(crate) use pause::PAUSE_STALL;
#[cfg(test)]
pub(crate) use slot::SlotControls;
pub(crate) use slot::{drain_deferred_plugins, SlotLink};
pub(crate) use transfer::{param_transfer, ParamReceiver, ParamSender};

#[cfg(test)]
pub(crate) use fixtures::vst3_fixture;
#[cfg(test)]
pub(crate) use slot::{deferred_slot_count, tracked_slots_alive};
