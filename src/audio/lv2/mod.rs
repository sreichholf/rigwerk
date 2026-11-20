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
//! declare (`scan`) without loading a binary.  Loading, the URID map, the
//! host features, the worker and the state belong in modules of their own.

mod scan;
mod ttl;

#[cfg(test)]
mod tests;

pub(crate) use scan::{scan_bundle, scan_library, scan_roots, Lv2Candidate};
