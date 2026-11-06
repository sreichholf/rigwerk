//! A small CLAP effect host.
//!
//! CLAP is a plain C ABI.  The host opens the `.clap` bundle, hands the plugin
//! a `clap_host` with the callbacks and extensions it implements, and then uses
//! the plugin's parameter, audio-port and GUI extensions.  Everything is
//! serialised through the same mutex the VST3 host already uses, so a plugin
//! instance is only ever called from one thread at a time.

mod host;
mod module;
mod plugin;
mod ports;
mod scan;
mod state;

#[cfg(test)]
pub(crate) mod tests;

#[cfg(test)]
pub(crate) use host::{claim_main_thread_for_test, MainThreadClaim};
pub(crate) use host::{
    is_main_thread, mark_main_thread, may_destroy_plugins_here, HostState, StateSave,
};
pub(crate) use plugin::ClapPlugin;
pub(crate) use ports::AUDIO_PORTS_CHANGED;
pub(crate) use scan::{scan_bundle, scan_library, ClapCandidate};

use std::ffi::{c_char, CStr};

fn cstring(pointer: *const c_char) -> String {
    if pointer.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(pointer) }
            .to_string_lossy()
            .into_owned()
    }
}
