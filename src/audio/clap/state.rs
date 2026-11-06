//! CLAP plugin state stream helpers.

use std::ffi::c_void;
use std::ptr;

use clap_sys::stream::{clap_istream, clap_ostream};

/// The most state bytes accepted from one plugin, so a runaway `save` cannot
/// exhaust memory.
pub(super) const MAX_STATE_BYTES: usize = 8 * 1024 * 1024;

/// Collects what a plugin writes in `clap_plugin_state.save`.
#[derive(Default)]
pub(super) struct StateWriter {
    pub(super) bytes: Vec<u8>,
    pub(super) overflowed: bool,
}

impl StateWriter {
    /// The stream handed to the plugin.  It points at `self`, so `self` must
    /// stay in place while the plugin uses it.
    pub(super) fn stream(&mut self) -> clap_ostream {
        clap_ostream {
            ctx: self as *mut Self as *mut c_void,
            write: Some(state_write),
        }
    }
}

pub(super) unsafe extern "C" fn state_write(
    stream: *const clap_ostream,
    buffer: *const c_void,
    size: u64,
) -> i64 {
    let Some(stream) = (unsafe { stream.as_ref() }) else {
        return -1;
    };
    let Some(writer) = (unsafe { (stream.ctx as *mut StateWriter).as_mut() }) else {
        return -1;
    };
    if size == 0 {
        return 0;
    }
    if buffer.is_null() {
        return -1;
    }
    let size = match usize::try_from(size) {
        Ok(size) if size <= MAX_STATE_BYTES - writer.bytes.len().min(MAX_STATE_BYTES) => size,
        _ => {
            writer.overflowed = true;
            return -1;
        }
    };
    let data = unsafe { std::slice::from_raw_parts(buffer as *const u8, size) };
    writer.bytes.extend_from_slice(data);
    size as i64
}

/// Feeds saved bytes to `clap_plugin_state.load`.
pub(super) struct StateReader<'a> {
    pub(super) data: &'a [u8],
    pub(super) position: usize,
}

impl<'a> StateReader<'a> {
    pub(super) fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    /// The stream handed to the plugin.  It points at `self`, so `self` must
    /// stay in place while the plugin uses it.
    pub(super) fn stream(&mut self) -> clap_istream {
        clap_istream {
            ctx: self as *mut Self as *mut c_void,
            read: Some(state_read),
        }
    }
}

/// Copies up to `size` bytes; returns how many, 0 at the end, -1 on error.
pub(super) unsafe extern "C" fn state_read(
    stream: *const clap_istream,
    buffer: *mut c_void,
    size: u64,
) -> i64 {
    let Some(stream) = (unsafe { stream.as_ref() }) else {
        return -1;
    };
    let Some(reader) = (unsafe { (stream.ctx as *mut StateReader<'_>).as_mut() }) else {
        return -1;
    };
    let remaining = &reader.data[reader.position..];
    let count = usize::try_from(size)
        .unwrap_or(usize::MAX)
        .min(remaining.len())
        .min(i64::MAX as usize);
    if count == 0 {
        return 0;
    }
    if buffer.is_null() {
        return -1;
    }
    unsafe { ptr::copy_nonoverlapping(remaining.as_ptr(), buffer as *mut u8, count) };
    reader.position += count;
    count as i64
}
