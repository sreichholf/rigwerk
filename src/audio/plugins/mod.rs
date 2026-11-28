//! One effect chain per signal path (VST3, CLAP, LV2).
//!
//! The UI holds descriptors and [`PluginHandle`]s, never a processor.  No
//! lock is shared with the audio thread, which never loads or destroys a
//! plugin.  A slot it may still be inside (`process` never returned) is
//! leaked, not destroyed.

mod audio_thread;
mod chain;
#[cfg(test)]
mod fixtures;
mod handle;
pub(crate) mod host;
mod link;
mod load;
mod model;
mod pause;
mod pref;
mod queue;
mod slot;
mod state;
mod thread;
mod transfer;

#[cfg(test)]
mod tests;

use std::sync::atomic::{AtomicBool, Ordering};

pub(crate) use audio_thread::{on_audio_thread, AudioThreadScope};

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

#[cfg(test)]
pub(crate) use pref::PluginState;
pub use pref::{PluginFormat, PluginParamView, PluginPref, PluginView};

#[cfg(test)]
pub(crate) use chain::ChainRuntime;
pub(crate) use chain::{release_slot, ChainHome, ChainLease, MAX_EFFECTS};
pub(crate) use handle::PluginHandle;
#[cfg(test)]
pub(crate) use handle::{LoadedPlugin, Outcome, Saved};
pub(crate) use link::{
    ClapFailure, Lv2Failure, ParamIds, ParamOut, PluginShared, ProcessFailure, RESTART_ACTIVATION,
};
pub(crate) use load::{merge_synced_params, Capture, Restore, MAX_BLOCK, MAX_STATE_BLOB};
#[cfg(test)]
pub(crate) use load::{set_test_loader, TestLoaderGuard, TEST_PATH};
pub(crate) use model::{ChainModel, ReloadPlan};
pub(crate) use pause::PauseStep;
#[cfg(test)]
pub(crate) use pause::PAUSE_STALL;
#[cfg(test)]
pub(crate) use slot::SlotControls;
pub(crate) use slot::{Slot, SlotLink, SlotPark};
#[cfg(test)]
pub(crate) use slot::{RELOAD_LIMIT, RELOAD_WINDOW_MS};
pub(crate) use state::StateSave;
#[cfg(test)]
pub(crate) use thread::{claim_main_thread_for_test, MainThreadClaim};
pub(crate) use thread::{
    is_main_thread, mark_main_thread, may_destroy_plugins_here, recorded_main_thread,
};
pub(crate) use transfer::{param_transfer, ParamReceiver, ParamSender, Retry};

#[cfg(test)]
pub(crate) use fixtures::{lv2_fixture, vst3_fixture};
#[cfg(test)]
pub(crate) use slot::tracked_slots_alive;
