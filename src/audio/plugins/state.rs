#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StateSave {
    Saved(Vec<u8>),
    /// No state extension, or the save failed.
    Unavailable,
    /// Values still queued, or not on the main thread.  Try the next save.
    Deferred,
}
