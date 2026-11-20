//! VST3 and CLAP effect chains, one per signal path.
//!
//! Plugins are loaded through RigWerk's own VST3 host in [`super::vst3`] or
//! the CLAP host in [`super::clap`]; each path's chain lives on that path in
//! [`super::shared::Shared`] so the playback callback can process it.  The UI
//! only ever holds descriptors, never the plugin objects themselves.
//!
//! Each loaded plugin is split in two (`handle`): the processor the chain's
//! slot runs on the audio thread, and the [`PluginHandle`] the UI reaches
//! through the published effect links.  The handle keeps the views, the
//! saved values and the state blob behind its own UI-only lock.
//!
//! Each chain is split the same way: the runtime the audio thread owns
//! (`chain`) and the model the UI edits (`model`), which sends structural
//! edits as commands through a queue (`queue`) and gets the slots the
//! runtime let go of back the same way.  No lock is shared between the two
//! sides.

mod chain;
#[cfg(test)]
mod fixtures;
mod handle;
mod link;
mod load;
mod model;
mod pause;
mod pref;
mod queue;
mod slot;
mod transfer;

#[cfg(test)]
mod tests;

use std::sync::atomic::{AtomicBool, Ordering};

/// The most audio ports one effect may declare, shared by the CLAP host and
/// the LV2 scan.  More than this is a scan error for LV2 and a load error for
/// CLAP.
pub(crate) const MAX_AUDIO_PORTS: usize = 32;

/// The largest latency, in frames, one effect may report: the size the
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

#[cfg(test)]
pub(crate) use chain::ChainRuntime;
pub(crate) use chain::{ChainHome, ChainLease, MAX_EFFECTS};
pub(crate) use handle::PluginHandle;
#[cfg(test)]
pub(crate) use handle::{LoadedPlugin, Outcome, Saved};
pub(crate) use link::{
    ClapFailure, ParamIds, ParamOut, PluginShared, ProcessFailure, RESTART_ACTIVATION,
};
#[cfg(test)]
pub(crate) use load::{set_test_loader, TestLoaderGuard, TEST_PATH};
pub(crate) use load::{Restore, MAX_BLOCK};
pub(crate) use model::{ChainModel, ReloadPlan};
#[cfg(test)]
pub(crate) use pause::PAUSE_STALL;
#[cfg(test)]
pub(crate) use slot::SlotControls;
pub(crate) use slot::{drain_deferred_plugins, Slot, SlotLink};
#[cfg(test)]
pub(crate) use slot::{RELOAD_LIMIT, RELOAD_WINDOW_MS};
pub(crate) use transfer::{param_transfer, ParamReceiver, ParamSender, Retry};

#[cfg(test)]
pub(crate) use fixtures::vst3_fixture;
#[cfg(test)]
pub(crate) use slot::{deferred_slot_count, tracked_slots_alive};
