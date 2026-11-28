//! Process-wide plugin main thread.  Recorded once, from the UI thread,
//! before any plugin loads; a later call does not move it.
//!
//! A host thread-check reads the id.  Destroying a plugin is allowed on
//! that thread, or before any thread is recorded.

use std::sync::RwLock;

/// A host copies this when a plugin is created.  The audio callback uses
/// that copy and does not lock this.
static MAIN_THREAD: RwLock<Option<std::thread::ThreadId>> = RwLock::new(None);

pub(crate) fn recorded_main_thread() -> Option<std::thread::ThreadId> {
    match MAIN_THREAD.read() {
        Ok(slot) => *slot,
        Err(poison) => *poison.into_inner(),
    }
}

/// Call from the UI thread before any plugin loads.  A later call does
/// not move it.
pub(crate) fn mark_main_thread() {
    let mut slot = MAIN_THREAD
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if slot.is_none() {
        *slot = Some(std::thread::current().id());
    }
}

pub(crate) fn is_main_thread() -> bool {
    recorded_main_thread().is_some_and(|id| id == std::thread::current().id())
}

/// The recorded UI thread may destroy plugins.  Before any thread is
/// recorded the caller may too, so startup and tests do not leak.  Any
/// other thread defers the drop to the UI thread.
pub(crate) fn may_destroy_plugins_here() -> bool {
    match recorded_main_thread() {
        Some(id) => id == std::thread::current().id(),
        None => true,
    }
}

/// Tests point the recorded main thread here so a case can run on its own
/// thread without stealing the id from a later test.
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
