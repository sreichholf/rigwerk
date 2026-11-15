//! The host's `IMessage` and `IAttributeList`, which a plugin allocates
//! through `IHostApplication::createInstance` to talk between its
//! component and its controller.  Results follow the SDK's
//! `hostclasses.cpp`.
//!
//! Used on the UI thread in practice; the mutexes are only there so the
//! objects are `Sync`, and no plugin code runs while one is held.

use std::collections::BTreeMap;
use std::ffi::{c_char, c_void, CStr, CString};
use std::ptr;
use std::sync::{Mutex, MutexGuard, PoisonError};

use ::vst3::Steinberg::Vst::{IAttributeList, IAttributeListTrait, IMessage, IMessageTrait, TChar};
use ::vst3::Steinberg::{kInvalidArgument, kResultFalse, kResultTrue, tresult, FIDString};
use ::vst3::{Class, ComWrapper};

/// One stored attribute.
#[derive(Clone, Debug, PartialEq)]
enum AttrValue {
    Int(i64),
    Float(f64),
    /// UTF-16, without the terminating NUL.
    String(Vec<u16>),
    Binary(Vec<u8>),
}

/// A typed key/value list (`IAttributeList`).
#[derive(Default)]
pub(crate) struct HostAttributeList {
    values: Mutex<BTreeMap<CString, AttrValue>>,
}

impl HostAttributeList {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Stores `text` as a string attribute under `id`.
    pub(crate) fn set_text(&self, id: &CStr, text: &CStr) {
        let units = text.to_string_lossy().encode_utf16().collect();
        self.values()
            .insert(id.to_owned(), AttrValue::String(units));
    }

    fn values(&self) -> MutexGuard<'_, BTreeMap<CString, AttrValue>> {
        self.values.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Stores `value` under `id`.
    ///
    /// # Safety
    /// A non-null `id` must be a NUL-terminated string.
    unsafe fn set(&self, id: *const c_char, value: AttrValue) -> tresult {
        if id.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: the caller's contract.
        let key = unsafe { CStr::from_ptr(id) }.to_owned();
        self.values().insert(key, value);
        kResultTrue
    }

    /// The value under `id`, if any.
    ///
    /// # Safety
    /// `id` must be a NUL-terminated string (null is checked by callers).
    unsafe fn get(&self, id: *const c_char) -> Option<AttrValue> {
        // SAFETY: the caller's contract.
        let key = unsafe { CStr::from_ptr(id) };
        self.values().get(key).cloned()
    }
}

impl Class for HostAttributeList {
    type Interfaces = (IAttributeList,);
}

impl IAttributeListTrait for HostAttributeList {
    unsafe fn setInt(&self, id: *const c_char, value: i64) -> tresult {
        // SAFETY: the plugin passes an attribute id or null.
        unsafe { self.set(id, AttrValue::Int(value)) }
    }

    unsafe fn getInt(&self, id: *const c_char, value: *mut i64) -> tresult {
        if id.is_null() || value.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: a non-null id and out pointer from the plugin.
        match unsafe { self.get(id) } {
            Some(AttrValue::Int(stored)) => {
                // SAFETY: `value` was checked non-null above.
                unsafe { *value = stored };
                kResultTrue
            }
            _ => kResultFalse,
        }
    }

    unsafe fn setFloat(&self, id: *const c_char, value: f64) -> tresult {
        // SAFETY: the plugin passes an attribute id or null.
        unsafe { self.set(id, AttrValue::Float(value)) }
    }

    unsafe fn getFloat(&self, id: *const c_char, value: *mut f64) -> tresult {
        if id.is_null() || value.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: a non-null id and out pointer from the plugin.
        match unsafe { self.get(id) } {
            Some(AttrValue::Float(stored)) => {
                // SAFETY: `value` was checked non-null above.
                unsafe { *value = stored };
                kResultTrue
            }
            _ => kResultFalse,
        }
    }

    unsafe fn setString(&self, id: *const c_char, string: *const TChar) -> tresult {
        if string.is_null() {
            return kInvalidArgument;
        }
        let mut units = Vec::new();
        // SAFETY: the plugin passes a NUL-terminated UTF-16 string.
        unsafe {
            let mut at = string;
            while *at != 0 {
                units.push(*at);
                at = at.add(1);
            }
        }
        // SAFETY: the plugin passes an attribute id or null.
        unsafe { self.set(id, AttrValue::String(units)) }
    }

    unsafe fn getString(
        &self,
        id: *const c_char,
        string: *mut TChar,
        size_in_bytes: u32,
    ) -> tresult {
        if id.is_null() || string.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: a non-null id from the plugin.
        let Some(AttrValue::String(stored)) = (unsafe { self.get(id) }) else {
            return kResultFalse;
        };
        // Room for the text and its NUL, cut to fit (always terminated).
        let capacity = size_in_bytes as usize / std::mem::size_of::<TChar>();
        let Some(room) = capacity.checked_sub(1) else {
            return kResultFalse;
        };
        let count = stored.len().min(room);
        // SAFETY: `string` holds `capacity` units, and `count < capacity`.
        unsafe {
            ptr::copy_nonoverlapping(stored.as_ptr(), string, count);
            *string.add(count) = 0;
        }
        kResultTrue
    }

    unsafe fn setBinary(
        &self,
        id: *const c_char,
        data: *const c_void,
        size_in_bytes: u32,
    ) -> tresult {
        let bytes = if size_in_bytes == 0 {
            Vec::new()
        } else if data.is_null() {
            return kInvalidArgument;
        } else {
            // SAFETY: the plugin passes `size_in_bytes` readable bytes.
            unsafe { std::slice::from_raw_parts(data.cast::<u8>(), size_in_bytes as usize) }
                .to_vec()
        };
        // SAFETY: the plugin passes an attribute id or null.
        unsafe { self.set(id, AttrValue::Binary(bytes)) }
    }

    unsafe fn getBinary(
        &self,
        id: *const c_char,
        data: *mut *const c_void,
        size_in_bytes: *mut u32,
    ) -> tresult {
        if id.is_null() || data.is_null() || size_in_bytes.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: a non-null id from the plugin.
        let key = unsafe { CStr::from_ptr(id) };
        // The pointer is into the stored bytes, which stay put until the
        // attribute is set again (the SDK hands out the same).
        let found = match self.values().get(key) {
            Some(AttrValue::Binary(stored)) => Some((stored.as_ptr(), stored.len())),
            _ => None,
        };
        // SAFETY: both out pointers were checked above.
        unsafe {
            match found {
                Some((bytes, len)) => {
                    *data = bytes.cast();
                    *size_in_bytes = len as u32;
                    kResultTrue
                }
                None => {
                    *size_in_bytes = 0;
                    kResultFalse
                }
            }
        }
    }
}

/// A message: an id and its attributes (`IMessage`).
pub(crate) struct HostMessage {
    id: Mutex<Option<CString>>,
    attributes: ComWrapper<HostAttributeList>,
}

impl HostMessage {
    pub(crate) fn new() -> Self {
        Self {
            id: Mutex::new(None),
            attributes: ComWrapper::new(HostAttributeList::new()),
        }
    }

    fn id(&self) -> MutexGuard<'_, Option<CString>> {
        self.id.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Class for HostMessage {
    type Interfaces = (IMessage,);
}

impl IMessageTrait for HostMessage {
    unsafe fn getMessageID(&self) -> FIDString {
        // Valid until the id is set again, as in the SDK.
        self.id().as_ref().map_or(ptr::null(), |id| id.as_ptr())
    }

    unsafe fn setMessageID(&self, id: FIDString) {
        // SAFETY: the plugin passes a NUL-terminated id or null.
        let id = (!id.is_null()).then(|| unsafe { CStr::from_ptr(id) }.to_owned());
        *self.id() = id;
    }

    unsafe fn getAttributes(&self) -> *mut IAttributeList {
        // Borrowed: the message keeps the list alive (`hostclasses.cpp`).
        self.attributes
            .as_com_ref::<IAttributeList>()
            .map_or(ptr::null_mut(), |list| list.as_ptr())
    }
}

#[cfg(test)]
mod tests;
