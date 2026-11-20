//! The audio-thread marker shared by every plugin host.
//!
//! A host callback cannot ask the operating system which thread called it,
//! so the host marks the thread itself.  The CLAP host marks the thread for
//! `start_processing`, `process`, `stop_processing` and an active
//! `params.flush`, so its thread checks can tell the audio thread from the
//! main thread.  The marker is per thread and never crosses a queue, so a
//! callback answers from it without a lock.  The UI thread also stands in as
//! the audio thread for an audio-class call made with no block inside the
//! plugin (a forced pause, teardown), because for that call it is the
//! plugin's audio thread.
//!
//! The LV2 host does not use it: its URID map answers every thread the same
//! way, without a lock, and its worker rings never lock either.

use std::cell::Cell;

thread_local! {
    /// True while the current thread makes the plugin's audio-thread calls.
    static ON_AUDIO_THREAD: Cell<bool> = const { Cell::new(false) };
}

/// Marks the current thread as the plugin's audio thread for as long as it
/// lives.  Scopes nest: dropping one restores what was there before.
pub(crate) struct AudioThreadScope {
    previous: bool,
}

impl AudioThreadScope {
    pub(crate) fn enter() -> Self {
        Self {
            previous: ON_AUDIO_THREAD.with(|flag| flag.replace(true)),
        }
    }
}

impl Drop for AudioThreadScope {
    fn drop(&mut self) {
        ON_AUDIO_THREAD.with(|flag| flag.set(self.previous));
    }
}

/// Whether the current thread is inside an [`AudioThreadScope`].
pub(crate) fn on_audio_thread() -> bool {
    ON_AUDIO_THREAD.with(Cell::get)
}
