//! RigWerk's own LV2 effect host.
//!
//! The host is built on `lv2-sys` for the C types and a Turtle parser of its
//! own, the way `audio/clap` is built on `clap-sys`; there is no `lilv` and
//! no system library.  A loaded plugin is split the way a CLAP or VST3 one
//! is: the UI-thread half (`instance`) owns the handle's lifetime and every
//! non-audio call, and the audio-thread half (`processor`) owns the port
//! buffers and makes the audio calls.  Nothing here imports `app` or `ui`.
//!
//! The scan reads a bundle's Turtle files (`ttl`) and lists the effects they
//! declare (`scan`) without loading a binary.  `ports` turns the same parsed
//! description into the port table, the control ranges and the buffers,
//! `module` keeps one library loaded per binary, the URID map (`urid`) is one
//! process-wide table that answers from the audio thread without a lock,
//! `host` owns the feature array an instance receives, `worker` holds the
//! byte rings a worker request crosses, and `state` turns the plugin's keyed
//! state properties into the blob the preferences store.

mod host;
mod instance;
mod module;
mod ports;
mod processor;
mod scan;
mod state;
mod ttl;
mod urid;
mod worker;

#[cfg(test)]
mod tests;

pub(crate) use instance::{Lv2Instance, Lv2Loaded};
pub(crate) use processor::Lv2Processor;
pub(crate) use scan::{scan_bundle, scan_library, scan_roots, Lv2Candidate};
