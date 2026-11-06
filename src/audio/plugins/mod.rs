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

pub use pref::{PluginFormat, PluginParamView, PluginPref, PluginView};

pub(crate) use chain::PluginChain;
pub(crate) use load::{
    capture_state, instance_id, read_plugin_parameters, service_clap, PluginHandle, Restore,
};
pub(crate) use slot::drain_deferred_plugins;

#[cfg(test)]
pub(crate) use slot::{deferred_slot_count, tracked_slots_alive};
