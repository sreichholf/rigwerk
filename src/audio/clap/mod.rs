//! A small CLAP effect host.
//!
//! CLAP is a plain C ABI.  The host opens the `.clap` bundle, hands the plugin
//! a `clap_host` with the callbacks and extensions it implements, and then uses
//! the plugin's parameter, audio-port and GUI extensions.
//!
//! A loaded plugin is split in two: `instance` is the main-thread half,
//! reached only through its `PluginHandle` on the UI thread, and
//! `processor` is the audio-thread half the chain's slot owns.  The two
//! never share a lock: values cross in the transfer queues, and a
//! restart or reactivation waits for the pause handshake to stop the
//! processor first.

mod gui;
mod host;
mod instance;
mod module;
mod ports;
mod processor;
mod scan;
mod state;

#[cfg(test)]
pub(crate) mod tests;

pub(crate) use gui::{ParentWindow, ResizeHints, WindowApi};
pub(crate) use host::{GuiVisibility, HostState};
pub(crate) use instance::{ClapInstance, ClapLoaded};
pub(crate) use processor::ClapProcessor;
pub(crate) use scan::{
    scan_bundle, scan_library, scan_library_isolated, scan_roots, write_scan_file, ClapCandidate,
    CHILD_EXIT_ABANDONED, CHILD_EXIT_UNWRITABLE,
};

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
