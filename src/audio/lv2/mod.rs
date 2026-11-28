//! LV2 effect host on `lv2-sys` and its own Turtle parser.  No lilv.
//!
//! The scan lists a bundle without loading its binary.  The URID map is one
//! process-wide table and answers from the audio thread without a lock.
//! `work` runs on one process-wide thread once an instance is registered; a
//! restore's requests run on the UI thread before that, and what is left at
//! teardown after it.

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
