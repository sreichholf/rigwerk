//! Panic containment for the real-time entry points.
//!
//! A panic in a cpal stream closure or a PipeWire listener unwinds into the
//! host's callback, which is undefined behaviour or an abort.  Every backend
//! that the process can carry a panic out of runs its body through
//! [`contain`]:
//!
//! - the cpal capture, clock-output and generic extra-output closures;
//! - every PipeWire listener body (`process`, `param_changed`,
//!   `state_changed`) on the native streams, the extra-output streams and the
//!   sink monitor;
//! - the duplex filter's `process` and `state_changed`, and the RigWerk Out
//!   virtual source's `process`;
//! - WASAPI's REAL-style and exclusive render threads;
//! - ASIO, which catches its own in `win/asio/callback`.
//!
//! What is not covered, and why:
//!
//! - A plugin panicking inside its own `extern "C"` entry: Rust aborts on an
//!   unwind across that boundary, before the panic can reach [`contain`].
//!   Containment covers RigWerk's own sample path and anything the plugin
//!   unwinds through Rust frames.
//! - The bridge and UI threads, where a thread panic ends that thread only
//!   and the engine already reports it.
//!
//! `catch_unwind` does not suppress the panic hook: the default hook still
//! runs on the audio thread and may allocate or log.  That happens only after
//! the panic, never on the happy path, which is one `catch_unwind` call and
//! allocates nothing.  On the bridge backends a caught failure is published as
//! one atomic code on `Shared`, never as a formatted message; `Shared::snapshot`
//! words it for the screens.

/// Runs `work` and reports whether it panicked.  The caller is the one that
/// silences its block and marks the run failed: nothing is swallowed here.
/// Safe for the audio thread; `AssertUnwindSafe` is what lets the body touch
/// the callback's `&mut` state.
#[inline]
#[must_use = "a caught panic must silence the block and mark the run failed"]
pub(crate) fn contain(work: impl FnOnce()) -> bool {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)).is_err()
}

#[cfg(test)]
mod tests;
