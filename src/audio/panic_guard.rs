//! A panic must not unwind into the host's C callback.
//!
//! A plugin's own `extern "C"` entry aborts before this runs.  The panic
//! hook still runs and may allocate, only after the panic.  A caught
//! failure is one atomic code; [`super::shared::Shared::snapshot`] words it.

/// The caller silences the block and marks the run failed.  Nothing is
/// swallowed here.  `AssertUnwindSafe` lets the body touch the callback's
/// `&mut` state.
#[inline]
#[must_use = "a caught panic must silence the block and mark the run failed"]
pub(crate) fn contain(work: impl FnOnce()) -> bool {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)).is_err()
}

#[cfg(test)]
mod tests;
