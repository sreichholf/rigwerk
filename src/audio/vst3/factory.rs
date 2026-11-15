//! A module's plugin factory: its classes, class-id text, and creating
//! instances.
//!
//! Saved preferences hold class ids as the 32 hex digits `moduleinfo.json`
//! writes, in canonical order.  On Windows a `TUID` is COM-compatible: the
//! first three GUID fields are little-endian, so the text is reordered on
//! the way in (the mapping vst3-host 0.11.1 uses, `internal/utils.rs`).

use std::ffi::c_void;
use std::ptr;

use ::vst3::Steinberg::{
    kResultOk, IPluginFactory, IPluginFactory3, IPluginFactory3Trait, IPluginFactoryTrait,
    PClassInfo, TUID,
};
use ::vst3::{ComPtr, Interface};

use super::c_text;

/// The category of an audio processor class (`kVstAudioEffectClass`).
pub(crate) const AUDIO_MODULE_CLASS: &str = "Audio Module Class";

/// For each `TUID` byte on a COM platform, the canonical byte it holds.
const COM_ORDER: [usize; 16] = [3, 2, 1, 0, 5, 4, 7, 6, 8, 9, 10, 11, 12, 13, 14, 15];

/// One class a factory offers.
#[derive(Clone, Debug)]
pub(crate) struct ClassInfo {
    pub(crate) cid: TUID,
    pub(crate) name: String,
    pub(crate) category: String,
}

/// Every class the factory reports, in its order.  A class whose info
/// cannot be read is left out.
pub(crate) fn classes(factory: &ComPtr<IPluginFactory>) -> Vec<ClassInfo> {
    // SAFETY: a live factory; `countClasses` takes no arguments.
    let count = unsafe { factory.countClasses() }.max(0);
    (0..count)
        .filter_map(|index| {
            // SAFETY: `PClassInfo` is integers and byte arrays, so all
            // zeroes is a valid value.
            let mut info: PClassInfo = unsafe { std::mem::zeroed() };
            // SAFETY: `info` is a writable `PClassInfo` for the call.
            let result = unsafe { factory.getClassInfo(index, &mut info) };
            (result == kResultOk).then(|| ClassInfo {
                cid: info.cid,
                name: c_text(&info.name),
                category: c_text(&info.category),
            })
        })
        .collect()
}

/// The audio processor class to create: the one `class_id` names, or with
/// no id (a bundle without `moduleinfo.json`), the first audio class.
pub(crate) fn audio_class(
    factory: &ComPtr<IPluginFactory>,
    class_id: Option<&str>,
) -> Result<TUID, String> {
    let classes = classes(factory);
    let mut audio = classes
        .iter()
        .filter(|class| class.category == AUDIO_MODULE_CLASS);
    match class_id {
        Some(text) => {
            let cid =
                parse_class_id(text).ok_or_else(|| format!("{text} is not a VST3 class id."))?;
            audio
                .find(|class| class.cid == cid)
                .map(|class| class.cid)
                .ok_or_else(|| format!("The plugin has no audio class {text}."))
        }
        None => audio
            .next()
            .map(|class| class.cid)
            .ok_or_else(|| "The plugin has no audio class.".to_owned()),
    }
}

/// The `TUID` for class-id text in this platform's byte order.
pub(crate) fn parse_class_id(text: &str) -> Option<TUID> {
    parse_class_id_for(text, cfg!(target_os = "windows"))
}

/// The `TUID` for 32 hex digits (no separators), in COM byte order when
/// `com` is set.
pub(super) fn parse_class_id_for(text: &str, com: bool) -> Option<TUID> {
    if text.len() != 32 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut canonical = [0u8; 16];
    for (index, byte) in canonical.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok()?;
    }
    let mut cid: TUID = [0; 16];
    for (index, unit) in cid.iter_mut().enumerate() {
        let source = if com { COM_ORDER[index] } else { index };
        *unit = canonical[source] as std::ffi::c_char;
    }
    Some(cid)
}

/// A new instance of class `cid` as interface `I`, or `None` when the
/// factory refuses.
pub(crate) fn create<I: Interface>(
    factory: &ComPtr<IPluginFactory>,
    cid: &TUID,
) -> Option<ComPtr<I>> {
    let mut object: *mut c_void = ptr::null_mut();
    // SAFETY: `cid` and the IID are 16 bytes each, and `object` is where
    // the factory writes the new instance.
    let result =
        unsafe { factory.createInstance(cid.as_ptr(), I::IID.as_ptr().cast(), &mut object) };
    if result != kResultOk {
        return None;
    }
    // SAFETY: on success the factory hands over one reference to an `I`.
    unsafe { ComPtr::from_raw(object.cast::<I>()) }
}

/// Hands the factory the process-wide host context
/// (`IPluginFactory3::setHostContext`).  A factory without the interface,
/// or one that ignores it (`kNotImplemented` off Linux), is fine.
pub(crate) fn set_host_context(factory: &ComPtr<IPluginFactory>) {
    if let Some(factory) = factory.cast::<IPluginFactory3>() {
        // SAFETY: the context is a static, so it outlives the module that
        // keeps it.
        unsafe { factory.setHostContext(super::host::host_context()) };
    }
}

#[cfg(test)]
mod tests;
