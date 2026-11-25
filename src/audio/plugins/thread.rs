//! The recorded plugin main thread, and where a plugin may be destroyed.
//!
//! Both are process-wide and belong to no plugin format: a host's
//! thread-check extension asks the first, and a chain that lets go of a slot
//! on another thread asks the second before it defers the destroy.  The
//! claim is made once, from the UI thread, before any plugin loads; nothing
//! moves it.

use std::sync::RwLock;

/// The UI thread, recorded by [`mark_main_thread`] before any plugin loads.
/// A host copies the id when a plugin is created.  The audio callback uses
/// that copy; it does not lock this.
static MAIN_THREAD: RwLock<Option<std::thread::ThreadId>> = RwLock::new(None);

/// The recorded UI thread, if one has been recorded yet.
pub(crate) fn recorded_main_thread() -> Option<std::thread::ThreadId> {
    match MAIN_THREAD.read() {
        Ok(slot) => *slot,
        Err(poison) => *poison.into_inner(),
    }
}

/// Records the calling thread as the plugin main thread.  Call it from the UI
/// thread before any plugin is loaded.  A later call does not move it.
pub(crate) fn mark_main_thread() {
    let mut slot = MAIN_THREAD
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if slot.is_none() {
        *slot = Some(std::thread::current().id());
    }
}

/// Whether this thread is the recorded UI/plugin main thread.
/// False until [`mark_main_thread`] has run.
pub(crate) fn is_main_thread() -> bool {
    recorded_main_thread().is_some_and(|id| id == std::thread::current().id())
}

/// Whether plugin instances may be destroyed on this thread.
/// The recorded UI thread may.  Before any thread is recorded, the caller
/// may too, so startup and tests do not leak.  Any other thread must hand
/// the instances to the deferred queue and let the UI thread drop them.
pub(crate) fn may_destroy_plugins_here() -> bool {
    match recorded_main_thread() {
        Some(id) => id == std::thread::current().id(),
        None => true,
    }
}

/// Points the recorded main thread at the caller for the guard's lifetime.
/// Production records the UI thread once; tests use this so a case can run
/// on its own thread without stealing that id from a later test.
#[cfg(test)]
pub(crate) struct MainThreadClaim {
    previous: Option<std::thread::ThreadId>,
}

#[cfg(test)]
pub(crate) fn claim_main_thread_for_test() -> MainThreadClaim {
    let mut slot = MAIN_THREAD
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let previous = *slot;
    *slot = Some(std::thread::current().id());
    MainThreadClaim { previous }
}

#[cfg(test)]
impl Drop for MainThreadClaim {
    fn drop(&mut self) {
        let mut slot = MAIN_THREAD
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *slot = self.previous;
    }
}
