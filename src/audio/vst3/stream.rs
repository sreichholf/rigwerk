//! The in-memory stream a plugin reads and writes its state through, and
//! the envelope a saved VST3 state is stored in.
//!
//! The envelope is the one vst3-host 0.11.1 wrote (`plugin.rs`) when it
//! hosted RigWerk's plugins, byte for byte, because saved preferences hold
//! it and an older RigWerk reads any other blob as raw component state: 16 bytes `VST3HOST_STATE\0\0`, then
//! little-endian u32 version (1), component length and controller length
//! (`u32::MAX` for no controller state), then the two payloads.

use std::ffi::CStr;
use std::sync::{Mutex, MutexGuard, PoisonError};

use ::vst3::Steinberg::IBStream_::IStreamSeekMode_::{kIBSeekCur, kIBSeekEnd, kIBSeekSet};
use ::vst3::Steinberg::Vst::{
    IAttributeList, IStreamAttributes, IStreamAttributesTrait, PresetAttributes, StateType,
    String128,
};
use ::vst3::Steinberg::{
    kInvalidArgument, kOutOfMemory, kResultFalse, kResultOk, tresult, IBStream, IBStreamTrait,
    ISizeableStream, ISizeableStreamTrait,
};
use ::vst3::{Class, ComWrapper};

use super::message::HostAttributeList;

/// The most a plugin may grow a stream to, or seek to.  The cursor and the
/// write length come from the plugin; this keeps a wild seek from turning
/// the next write into a huge allocation.  Far above any real state.
pub(crate) const MAX_STREAM_BYTES: usize = 64 * 1024 * 1024;

/// A growable byte buffer with a cursor (`IBStream`, `ISizeableStream`),
/// which tells the plugin what kind of state it carries
/// (`IStreamAttributes`).
pub(crate) struct MemoryStream {
    /// The bytes and the cursor.
    data: Mutex<(Vec<u8>, usize)>,
    /// `PresetAttributes::kStateType` and nothing else; no file name.
    attributes: ComWrapper<HostAttributeList>,
}

impl MemoryStream {
    /// A stream holding `bytes`, with the cursor at the start.  Its state
    /// type is project state: every component and controller state RigWerk
    /// saves or restores belongs to its saved session, not to a preset.
    pub(crate) fn new(bytes: Vec<u8>) -> Self {
        let attributes = HostAttributeList::new();
        // SAFETY: both are the bindings' NUL-terminated `'static` literals.
        let (key, project) = unsafe {
            (
                CStr::from_ptr(PresetAttributes::kStateType),
                CStr::from_ptr(StateType::kProject),
            )
        };
        attributes.set_text(key, project);
        Self {
            data: Mutex::new((bytes, 0)),
            attributes: ComWrapper::new(attributes),
        }
    }

    /// Everything in the stream.
    pub(crate) fn bytes(&self) -> Vec<u8> {
        self.data().0.clone()
    }

    fn data(&self) -> MutexGuard<'_, (Vec<u8>, usize)> {
        self.data.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Class for MemoryStream {
    type Interfaces = (IBStream, ISizeableStream, IStreamAttributes);
}

impl IStreamAttributesTrait for MemoryStream {
    /// No file behind a session state.
    unsafe fn getFileName(&self, name: *mut String128) -> tresult {
        // SAFETY: the plugin passes a writable `String128` or null.
        match unsafe { name.as_mut() } {
            Some(name) => {
                name.fill(0);
                kResultFalse
            }
            None => kInvalidArgument,
        }
    }

    unsafe fn getAttributes(&self) -> *mut IAttributeList {
        // Borrowed: the stream keeps the list alive.
        self.attributes
            .as_com_ref::<IAttributeList>()
            .map_or(std::ptr::null_mut(), |list| list.as_ptr())
    }
}

impl IBStreamTrait for MemoryStream {
    unsafe fn read(
        &self,
        buffer: *mut std::ffi::c_void,
        num_bytes: i32,
        num_bytes_read: *mut i32,
    ) -> tresult {
        let Ok(wanted) = usize::try_from(num_bytes) else {
            return kInvalidArgument;
        };
        if buffer.is_null() && wanted > 0 {
            return kInvalidArgument;
        }
        let mut data = self.data();
        let (bytes, cursor) = &mut *data;
        let start = (*cursor).min(bytes.len());
        let count = wanted.min(bytes.len() - start);
        if count > 0 {
            // SAFETY: the plugin's buffer holds `num_bytes` bytes, and
            // `count <= num_bytes`.
            unsafe {
                std::ptr::copy_nonoverlapping(bytes[start..].as_ptr(), buffer.cast::<u8>(), count);
            }
        }
        *cursor = start + count;
        if !num_bytes_read.is_null() {
            // SAFETY: a non-null out pointer from the plugin.
            unsafe { *num_bytes_read = count as i32 };
        }
        kResultOk
    }

    unsafe fn write(
        &self,
        buffer: *mut std::ffi::c_void,
        num_bytes: i32,
        num_bytes_written: *mut i32,
    ) -> tresult {
        let Ok(count) = usize::try_from(num_bytes) else {
            return kInvalidArgument;
        };
        if buffer.is_null() && count > 0 {
            return kInvalidArgument;
        }
        let mut data = self.data();
        let (bytes, cursor) = &mut *data;
        let end = cursor.saturating_add(count);
        // Over the cap nothing is written; an empty write copies nothing.
        let written = if end > MAX_STREAM_BYTES || count == 0 {
            0
        } else {
            if end > bytes.len() {
                bytes.resize(end, 0);
            }
            // SAFETY: the plugin's buffer holds `num_bytes` bytes, and the
            // stream was grown to hold them at the cursor.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    buffer.cast::<u8>(),
                    bytes[*cursor..end].as_mut_ptr(),
                    count,
                );
            }
            *cursor = end;
            count
        };
        if !num_bytes_written.is_null() {
            // SAFETY: a non-null out pointer from the plugin.
            unsafe { *num_bytes_written = written as i32 };
        }
        if written < count {
            kOutOfMemory
        } else {
            kResultOk
        }
    }

    unsafe fn seek(&self, pos: i64, mode: i32, result: *mut i64) -> tresult {
        let mut data = self.data();
        let (bytes, cursor) = &mut *data;
        // The enum is `int` on Windows and `unsigned` elsewhere.
        let mode = i64::from(mode);
        let base = if mode == i64::from(kIBSeekSet) {
            0
        } else if mode == i64::from(kIBSeekCur) {
            *cursor as i64
        } else if mode == i64::from(kIBSeekEnd) {
            bytes.len() as i64
        } else {
            return kInvalidArgument;
        };
        // Before the start lands on the start, as vst3-host 0.11.1 did.
        let Some(target) = base.checked_add(pos).map(|target| target.max(0)) else {
            return kInvalidArgument;
        };
        if target > MAX_STREAM_BYTES as i64 {
            return kInvalidArgument;
        }
        *cursor = target as usize;
        if !result.is_null() {
            // SAFETY: a non-null out pointer from the plugin.
            unsafe { *result = target };
        }
        kResultOk
    }

    unsafe fn tell(&self, pos: *mut i64) -> tresult {
        if pos.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: a non-null out pointer from the plugin.
        unsafe { *pos = self.data().1 as i64 };
        kResultOk
    }
}

impl ISizeableStreamTrait for MemoryStream {
    unsafe fn getStreamSize(&self, size: *mut i64) -> tresult {
        if size.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: a non-null out pointer from the plugin.
        unsafe { *size = self.data().0.len() as i64 };
        kResultOk
    }

    unsafe fn setStreamSize(&self, size: i64) -> tresult {
        match usize::try_from(size) {
            Ok(size) if size <= MAX_STREAM_BYTES => {
                self.data().0.resize(size, 0);
                kResultOk
            }
            _ => kInvalidArgument,
        }
    }
}

/// A plugin's two state streams.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StateBlob {
    pub(crate) component: Vec<u8>,
    /// `None` when the controller wrote no state (or is the component).
    pub(crate) controller: Option<Vec<u8>>,
}

const STATE_MAGIC: &[u8; 16] = b"VST3HOST_STATE\0\0";
const STATE_VERSION: u32 = 1;
const STATE_HEADER: usize = 16 + 4 + 4 + 4;
/// The controller length that means "no controller state".
const NO_CONTROLLER: u32 = u32::MAX;

/// The envelope for `blob`.  Both payloads together may use what one
/// stream may hold.
pub(crate) fn encode_state(blob: &StateBlob) -> Result<Vec<u8>, String> {
    let controller = blob.controller.as_deref();
    let payload = blob.component.len() + controller.map_or(0, <[u8]>::len);
    if payload > MAX_STREAM_BYTES {
        return Err(format!(
            "The plugin state is too large ({payload} bytes, at most {MAX_STREAM_BYTES})."
        ));
    }
    // Each length fits: the payload does.
    let controller_len = controller.map_or(NO_CONTROLLER, |state| state.len() as u32);
    let mut out = Vec::with_capacity(STATE_HEADER + payload);
    out.extend_from_slice(STATE_MAGIC);
    out.extend_from_slice(&STATE_VERSION.to_le_bytes());
    out.extend_from_slice(&(blob.component.len() as u32).to_le_bytes());
    out.extend_from_slice(&controller_len.to_le_bytes());
    out.extend_from_slice(&blob.component);
    if let Some(controller) = controller {
        out.extend_from_slice(controller);
    }
    Ok(out)
}

/// The two states in an envelope.  Bytes without the magic are a blob
/// saved before the envelope: raw component state.
pub(crate) fn decode_state(bytes: &[u8]) -> Result<StateBlob, String> {
    if !bytes.starts_with(STATE_MAGIC) {
        if bytes.len() > MAX_STREAM_BYTES {
            return Err(format!(
                "The plugin state is too large ({} bytes, at most {MAX_STREAM_BYTES}).",
                bytes.len()
            ));
        }
        return Ok(StateBlob {
            component: bytes.to_vec(),
            controller: None,
        });
    }
    if bytes.len() < STATE_HEADER {
        return Err("The plugin state header is cut short.".to_owned());
    }
    let field = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"));
    let version = field(16);
    if version != STATE_VERSION {
        return Err(format!("The plugin state has unknown version {version}."));
    }
    let component_len = field(20) as usize;
    let controller_len = Some(field(24))
        .filter(|&len| len != NO_CONTROLLER)
        .map(|len| len as usize);
    let expected = STATE_HEADER
        .checked_add(component_len)
        .and_then(|len| len.checked_add(controller_len.unwrap_or(0)));
    if expected != Some(bytes.len()) || bytes.len() > STATE_HEADER + MAX_STREAM_BYTES {
        return Err(format!(
            "The plugin state does not match its header ({} bytes).",
            bytes.len()
        ));
    }
    let (component, controller) = bytes[STATE_HEADER..].split_at(component_len);
    Ok(StateBlob {
        component: component.to_vec(),
        controller: controller_len.map(|_| controller.to_vec()),
    })
}

#[cfg(test)]
mod tests;
