//! RigWerk's own LV2 effect host.
//!
//! The host is built on `lv2-sys` for the C types and a Turtle parser of its
//! own, the way `audio/clap` is built on `clap-sys`; there is no `lilv` and
//! no system library.  A loaded plugin is split the way a CLAP or VST3 one
//! is: the UI-thread half owns the handle's lifetime and every non-audio
//! call, and the audio-thread half owns the port buffers and makes the audio
//! calls.  Nothing here imports `app` or `ui`.
//!
//! The scan reads a bundle's Turtle files (`ttl`) and lists the effects they
//! declare (`scan`) without loading a binary.  The URID map (`urid`) is one
//! process-wide table that answers from the audio thread without a lock,
//! `host` owns the feature array an instance receives, and `worker` holds
//! the byte rings a worker request crosses.  State, the ports and the two
//! halves of a loaded instance belong in modules of their own.

mod host;
mod scan;
mod ttl;
mod urid;
mod worker;

#[cfg(test)]
mod tests;

#[allow(unused_imports)] // The loader uses these in the next commits.
pub(crate) use host::{HostFeatures, SUPPORTED};
#[allow(unused_imports)] // State code and the UI service use it later.
pub(crate) use urid::{global, seed};
#[allow(unused_imports)] // Wired to the worker thread in a later commit.
pub(crate) use worker::{WorkerRings, RING_BYTES};

pub(crate) use scan::{scan_bundle, scan_library, scan_roots, Lv2Candidate};
