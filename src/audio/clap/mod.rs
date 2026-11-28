//! CLAP effect host.  `instance` is the UI-thread half, through
//! `PluginHandle`; `processor` is the audio-thread half the slot owns.
//! They share no lock: values cross in the transfer queues, and a restart
//! waits until the pause handshake has stopped the processor.

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
