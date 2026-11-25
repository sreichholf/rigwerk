//! The outcome of saving a plugin's state, shared by CLAP and LV2.
//!
//! Each host captures its own format's blob.  They share these three
//! answers, and `host.rs`'s `finish_capture` maps them onto the handle's own
//! capture result.

/// The outcome of a host's `save_state`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StateSave {
    Saved(Vec<u8>),
    /// No state extension, or the save failed.
    Unavailable,
    /// Not now (values still queued, or not on the main thread); try again
    /// at the next save.
    Deferred,
}
