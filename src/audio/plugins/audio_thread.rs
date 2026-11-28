//! Per-thread marker for a host callback, which cannot ask the OS which
//! thread called it.  It never crosses a queue, so a callback reads it
//! without a lock.
//!
//! The UI thread stands in for an audio-class call with no block inside
//! the plugin (a forced pause, teardown): for that call it is the plugin's
//! audio thread.  LV2 does not use this.  Its URID map and worker rings
//! answer every thread the same way, without a lock.

use std::cell::Cell;

thread_local! {
    static ON_AUDIO_THREAD: Cell<bool> = const { Cell::new(false) };
}

/// Scopes nest: dropping one restores the previous mark.
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

pub(crate) fn on_audio_thread() -> bool {
    ON_AUDIO_THREAD.with(Cell::get)
}
