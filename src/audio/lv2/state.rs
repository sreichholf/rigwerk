//! LV2 state, saved as the opaque blob `PluginState` already keeps.
//!
//! LV2 keeps state as a set of keyed properties, each a value with a type URI
//! and flags, not as bytes.  The preferences keep bytes (base64 in the
//! `PluginState`), so this module turns one into the other.  The framing is
//! little-endian:
//!
//! ```text
//! magic   "RWL2ST01"   8 bytes
//! version u32          1
//! count   u32          number of records that follow
//! record  key length u32, key URI bytes
//!         type length u32, type URI bytes
//!         flags u32
//!         value length u32, value bytes
//! ```
//!
//! Only the framing is ours: the value bytes are the plugin's own, in the
//! type it named, and are copied back unchanged except a top-level
//! `atom:URID`, which is stored as the URI and mapped again on restore.  The
//! blob stores URIs, never the process's URIDs, so it outlives the run that
//! wrote it; at restore every URI is mapped again.  A blob is capped at
//! [`MAX_STATE_BLOB`], the size the preferences keep.  A value whose body
//! contains URIDs (an `atom:Object`, a sequence, anything nested) stays
//! opaque bytes.
//!
//! Only values flagged `LV2_STATE_IS_POD` are saved, deliberately: anything
//! else is a pointer or a handle that would not survive the save.  A value
//! flagged `LV2_STATE_IS_NATIVE` is refused even when it also claims POD,
//! because it is only valid in the process that wrote it.  Control port
//! values are not part of the state; they are the saved parameters, which
//! `Restore::params` applies on top.
//!
//! A zero-byte value is kept as an empty record, although `state.h` asks
//! for at least one byte: real plugins store their default empty strings and
//! paths that way (`eg-params` does), and refusing them fails the whole
//! save.
//!
//! A plugin that lists `state:loadDefaultState` gets its `state:state` from
//! the bundle restored at load when no saved blob was restored.  The Turtle
//! values become atoms the way lilv (through sratom) makes them: a `file:`
//! IRI is an `atom:Path` (absolute, which the identity `state:mapPath`
//! hands back unchanged), another IRI an `atom:URID`, `xsd:int`/`integer`
//! an `atom:Int`, `xsd:long` an `atom:Long`, `xsd:float`/`decimal` an
//! `atom:Float`, `xsd:double` an `atom:Double`, `xsd:boolean` an
//! `atom:Bool`, and a plain literal an `atom:String`.  Any other value (a
//! blank node, another datatype, a language tag) is left out with one log
//! line; the plugin sees that key as absent.
//!
//! `save` and `restore` call the plugin's `LV2_State_Interface` with the
//! instance's feature array, so a plugin that looks `state:mapPath` or
//! `state:freePath` up in it finds them.  `restore` is an instantiation-class
//! call and runs at load, before `activate`.  `save` may run while `run` does,
//! per `state.h`, so a capture needs no pause.

use std::ffi::{c_void, CStr, CString};
use std::ptr;

use lv2_sys::{
    LV2_Feature, LV2_Handle, LV2_State_Flags, LV2_State_Interface, LV2_State_Retrieve_Function,
    LV2_State_Status, LV2_State_Status_LV2_STATE_ERR_BAD_FLAGS as LV2_STATE_ERR_BAD_FLAGS,
    LV2_State_Status_LV2_STATE_ERR_BAD_TYPE as LV2_STATE_ERR_BAD_TYPE,
    LV2_State_Status_LV2_STATE_ERR_NO_FEATURE as LV2_STATE_ERR_NO_FEATURE,
    LV2_State_Status_LV2_STATE_ERR_NO_PROPERTY as LV2_STATE_ERR_NO_PROPERTY,
    LV2_State_Status_LV2_STATE_ERR_NO_SPACE as LV2_STATE_ERR_NO_SPACE,
    LV2_State_Status_LV2_STATE_ERR_UNKNOWN as LV2_STATE_ERR_UNKNOWN,
    LV2_State_Status_LV2_STATE_SUCCESS as LV2_STATE_SUCCESS, LV2_URID,
};

use crate::audio::plugins::MAX_STATE_BLOB;

use super::ttl::{self, Object, TtlStore};
use super::urid;

const STATE_STATE: &str = "http://lv2plug.in/ns/ext/state#state";
const STATE_LOAD_DEFAULT: &str = "http://lv2plug.in/ns/ext/state#loadDefaultState";
const LV2_REQUIRED_FEATURE: &str = "http://lv2plug.in/ns/lv2core#requiredFeature";
const LV2_OPTIONAL_FEATURE: &str = "http://lv2plug.in/ns/lv2core#optionalFeature";
const XSD: &str = "http://www.w3.org/2001/XMLSchema#";
const ATOM_PATH: &std::ffi::CStr = c"http://lv2plug.in/ns/ext/atom#Path";
const ATOM_URID: &std::ffi::CStr = c"http://lv2plug.in/ns/ext/atom#URID";
const ATOM_INT: &std::ffi::CStr = c"http://lv2plug.in/ns/ext/atom#Int";
const ATOM_LONG: &std::ffi::CStr = c"http://lv2plug.in/ns/ext/atom#Long";
const ATOM_FLOAT: &std::ffi::CStr = c"http://lv2plug.in/ns/ext/atom#Float";
const ATOM_DOUBLE: &std::ffi::CStr = c"http://lv2plug.in/ns/ext/atom#Double";
const ATOM_BOOL: &std::ffi::CStr = c"http://lv2plug.in/ns/ext/atom#Bool";
const ATOM_STRING: &std::ffi::CStr = c"http://lv2plug.in/ns/ext/atom#String";

/// The first eight bytes of every blob.
const MAGIC: &[u8; 8] = b"RWL2ST01";
/// The layout version inside the magic.
const VERSION: u32 = 1;
/// Magic, version and count, before the first record.
const HEADER_BYTES: usize = 8 + 4 + 4;
/// The four length/flag words every record starts with, so a count cannot
/// claim more records than the blob could hold.
const MIN_RECORD_BYTES: usize = 4 + 4 + 4 + 4;

/// One property a plugin stored: the key and type as URIs, the flags it gave
/// them, and the value bytes.
struct Record {
    key: CString,
    type_: CString,
    flags: u32,
    value: Vec<u8>,
}

impl Record {
    /// The bytes this record adds to the blob, length prefixes included.
    fn encoded_len(&self) -> usize {
        4 + self.key.as_bytes().len() + 4 + self.type_.as_bytes().len() + 4 + 4 + self.value.len()
    }
}

/// What one `save` accumulates: the records and the encoded size so far.
/// The `store` callback's handle.
#[derive(Default)]
struct SaveSink {
    records: Vec<Record>,
    encoded: usize,
}

impl SaveSink {
    /// Whether one more `encoded` bytes still fit under the cap.
    fn has_room(&self, encoded: usize) -> bool {
        HEADER_BYTES
            .checked_add(self.encoded)
            .and_then(|total| total.checked_add(encoded))
            .is_some_and(|total| total <= MAX_STATE_BLOB)
    }
}

/// Saves the plugin's state as a blob.  UI thread; `state.h` allows this to
/// overlap `run`.  The handle is the plugin's instance handle, `interface`
/// its state extension, and `features` the null-terminated feature array it
/// was instantiated with.
pub(crate) fn save(
    handle: LV2_Handle,
    interface: *const LV2_State_Interface,
    features: *const *const LV2_Feature,
) -> Result<Vec<u8>, String> {
    let Some(save) = (unsafe { interface.as_ref() }).and_then(|interface| interface.save) else {
        return Err("The LV2 plugin has no state interface that saves.".to_owned());
    };
    let mut sink = SaveSink::default();
    // SAFETY: `handle` is the live instance and the sink outlives the call;
    // the store callback only touches the sink and the URID map.
    let status = unsafe {
        save(
            handle,
            Some(store),
            &mut sink as *mut SaveSink as *mut c_void,
            u32::from(LV2_State_Flags::LV2_STATE_IS_POD),
            features,
        )
    };
    if status != LV2_STATE_SUCCESS {
        // The plugin reports what it understood: a plugin that handles
        // `NO_SPACE` itself and stores something smaller still succeeds.
        return if status == LV2_STATE_ERR_NO_SPACE {
            Err(format!(
                "The LV2 plugin's state is larger than {} MB.",
                MAX_STATE_BLOB / (1024 * 1024)
            ))
        } else {
            Err(format!(
                "The LV2 plugin could not save its state ({status}: {}).",
                status_name(status)
            ))
        };
    }
    Ok(encode(&sink.records))
}

/// Writes the records in the layout the module doc describes.  Callers have
/// already kept the total under the cap.
fn encode(records: &[Record]) -> Vec<u8> {
    let size = HEADER_BYTES + records.iter().map(Record::encoded_len).sum::<usize>();
    let mut bytes = Vec::with_capacity(size);
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&VERSION.to_le_bytes());
    bytes.extend_from_slice(&(records.len() as u32).to_le_bytes());
    for record in records {
        let key = record.key.as_bytes();
        bytes.extend_from_slice(&(key.len() as u32).to_le_bytes());
        bytes.extend_from_slice(key);
        let type_ = record.type_.as_bytes();
        bytes.extend_from_slice(&(type_.len() as u32).to_le_bytes());
        bytes.extend_from_slice(type_);
        bytes.extend_from_slice(&record.flags.to_le_bytes());
        bytes.extend_from_slice(&(record.value.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&record.value);
    }
    bytes
}

/// `LV2_State_Interface.save`'s store callback: copies one value into the
/// sink.  A value that cannot be saved gets the status that says why — a
/// non-POD or native value `LV2_STATE_ERR_BAD_FLAGS`, an unknown key or type
/// `LV2_STATE_ERR_UNKNOWN`, a value that does not fit
/// `LV2_STATE_ERR_NO_SPACE` — and stays out of the blob.
///
/// A zero-byte value is stored as an empty record.  `state.h` asks plugins
/// for at least one byte, but some store their default empty string or path
/// anyway (`eg-params` does), and refusing that fails their whole save.
///
/// A top-level `atom:URID` is stored as the URI it names, not the `u32`.
/// A value that is not four native-endian bytes, or an id the map does not
/// know, is refused here.  The plugin's `save` decides whether that fails
/// the whole save.  Nested atoms are not walked.
unsafe extern "C" fn store(
    handle: *mut c_void,
    key: LV2_URID,
    value: *const c_void,
    size: usize,
    type_: LV2_URID,
    flags: u32,
) -> LV2_State_Status {
    // SAFETY: the handle is the `SaveSink` `save` passed, alive for the call.
    let Some(sink) = (unsafe { (handle as *mut SaveSink).as_mut() }) else {
        return LV2_STATE_ERR_UNKNOWN;
    };
    if flags & u32::from(LV2_State_Flags::LV2_STATE_IS_NATIVE) != 0 {
        eprintln!("[plugins] not saving an LV2 state value: it is only valid in this process");
        return LV2_STATE_ERR_BAD_FLAGS;
    }
    if flags & u32::from(LV2_State_Flags::LV2_STATE_IS_POD) == 0 {
        eprintln!("[plugins] not saving an LV2 state value: it is not plain data");
        return LV2_STATE_ERR_BAD_FLAGS;
    }
    let map = urid::global();
    let (Some(key), Some(type_)) = (map.uri(key), map.uri(type_)) else {
        eprintln!("[plugins] not saving an LV2 state value: its key or type is not a known URI");
        return LV2_STATE_ERR_UNKNOWN;
    };
    if size > MAX_STATE_BLOB {
        // Larger than a whole blob, so it can never fit; do not even make a
        // slice from it.
        return LV2_STATE_ERR_NO_SPACE;
    }
    let value = if type_.to_bytes() == ATOM_URID.to_bytes() {
        match stored_urid(value, size) {
            Ok(bytes) => bytes,
            Err(status) => return status,
        }
    } else if size == 0 {
        // An empty value has no bytes to read, and `value` may even be null.
        // Store it as empty rather than fail the plugin's whole save.
        Vec::new()
    } else if value.is_null() {
        // A null pointer with a length names no readable bytes.
        eprintln!("[plugins] not saving an LV2 state value: it has no readable bytes");
        return LV2_STATE_ERR_UNKNOWN;
    } else {
        // SAFETY: the plugin promises `size` readable bytes at `value`.
        unsafe { std::slice::from_raw_parts(value as *const u8, size) }.to_vec()
    };
    let record = Record {
        key: key.to_owned(),
        type_: type_.to_owned(),
        flags,
        value,
    };
    let encoded = record.encoded_len();
    if !sink.has_room(encoded) {
        return LV2_STATE_ERR_NO_SPACE;
    }
    sink.encoded += encoded;
    sink.records.push(record);
    LV2_STATE_SUCCESS
}

/// The bytes a top-level `atom:URID` is stored as: the URI, with no trailing
/// NUL.  The numeric id belongs to this process.  `unmap` returning null, a
/// null pointer, or a size other than 4 refuses the value.
fn stored_urid(value: *const c_void, size: usize) -> Result<Vec<u8>, LV2_State_Status> {
    if size != std::mem::size_of::<u32>() {
        eprintln!("[plugins] not saving an LV2 state value: an atom:URID is not 4 bytes");
        return Err(LV2_STATE_ERR_BAD_TYPE);
    }
    if value.is_null() {
        eprintln!("[plugins] not saving an LV2 state value: an atom:URID has no readable bytes");
        return Err(LV2_STATE_ERR_UNKNOWN);
    }
    // SAFETY: the plugin promises `size` readable bytes, and `size` is 4.
    // Copying into an array avoids reading a `u32` that may be unaligned.
    let mut native = [0u8; 4];
    native.copy_from_slice(unsafe { std::slice::from_raw_parts(value as *const u8, size) });
    let id = u32::from_ne_bytes(native);
    let pointer = urid::global().unmap(id);
    if pointer.is_null() {
        eprintln!("[plugins] not saving an LV2 state value: its atom:URID is not a known URI");
        return Err(LV2_STATE_ERR_UNKNOWN);
    }
    // SAFETY: a non-null `unmap` pointer is a NUL-terminated URI that lives
    // for the process.
    let uri = unsafe { CStr::from_ptr(pointer) };
    Ok(uri.to_bytes().to_vec())
}

/// Restores a state blob into the plugin.  An instantiation-class call: UI
/// thread, before `activate` or with processing paused.  The handle is the
/// plugin's instance handle, `interface` its state extension, and `features`
/// the null-terminated feature array it was instantiated with.
///
/// `false` for a missing interface, a malformed blob, or a restore the
/// plugin refused; the caller then falls back to the saved control values.
pub(crate) fn restore(
    handle: LV2_Handle,
    interface: *const LV2_State_Interface,
    bytes: &[u8],
    features: *const *const LV2_Feature,
) -> bool {
    if interface.is_null() {
        eprintln!("[plugins] not restoring an LV2 plugin's state: it has no state interface");
        return false;
    }
    let Some(restore) = (unsafe { interface.as_ref() }).and_then(|interface| interface.restore)
    else {
        eprintln!("[plugins] not restoring an LV2 plugin's state: it cannot restore");
        return false;
    };
    let Some(records) = decode(bytes) else {
        return false;
    };
    restore_records(restore, handle, &records, features)
}

/// Hands `records` to the plugin's `restore`.  `false`, with one log line,
/// when the plugin refused them.
fn restore_records(
    restore: unsafe extern "C" fn(
        LV2_Handle,
        LV2_State_Retrieve_Function,
        *mut c_void,
        u32,
        *const *const LV2_Feature,
    ) -> LV2_State_Status,
    handle: LV2_Handle,
    records: &Vec<RestoreRecord>,
    features: *const *const LV2_Feature,
) -> bool {
    // SAFETY: `handle` is the live instance, the records outlive the call,
    // and the retrieve callback only reads them.
    let status = unsafe {
        restore(
            handle,
            Some(retrieve),
            records as *const Vec<RestoreRecord> as *mut c_void,
            0,
            features,
        )
    };
    if status != LV2_STATE_SUCCESS {
        eprintln!(
            "[plugins] not restoring an LV2 plugin's state: the plugin reported {status} ({})",
            status_name(status)
        );
        return false;
    }
    true
}

/// A plugin's `state:state` from its bundle, as records ready to restore.
pub(crate) struct DefaultState {
    records: Vec<RestoreRecord>,
}

/// Whether `plugin` lists `state:loadDefaultState` as `lv2:requiredFeature`.
/// An optional listing does not count: with no state to restore, the load
/// omits the feature instead of failing.
pub(crate) fn requires_load_default(store: &TtlStore, plugin: &str) -> bool {
    store
        .iris(plugin, LV2_REQUIRED_FEATURE)
        .iter()
        .any(|feature| feature == STATE_LOAD_DEFAULT)
}

/// The default state to load for `plugin`: its `state:state` when it lists
/// `state:loadDefaultState` as a required or optional feature, or `None`.
/// UI thread; the URIs are mapped here.
pub(crate) fn default_state(store: &TtlStore, plugin: &str) -> Option<DefaultState> {
    let wants = store
        .iris(plugin, LV2_REQUIRED_FEATURE)
        .into_iter()
        .chain(store.iris(plugin, LV2_OPTIONAL_FEATURE))
        .any(|feature| feature == STATE_LOAD_DEFAULT);
    if !wants {
        return None;
    }
    let node = store.objects(plugin, STATE_STATE).into_iter().next()?;
    let map = urid::global();
    let mut records = Vec::new();
    for (key, object) in store.statements(&node) {
        let Some((type_, flags, value)) = default_value(&object) else {
            eprintln!("[plugins] an LV2 default state value for {key} is not supported");
            continue;
        };
        let Ok(key_uri) = CString::new(key) else {
            continue;
        };
        let key = map.map(&key_uri);
        let type_ = map.map(type_);
        if key == 0 || type_ == 0 {
            continue;
        }
        records.push(RestoreRecord {
            key,
            type_,
            flags,
            value,
        });
    }
    Some(DefaultState { records })
}

/// The atom type, flags and body a Turtle value restores as, or `None` for
/// a kind lilv would build an atom for that this host does not.
fn default_value(object: &Object) -> Option<(&'static std::ffi::CStr, u32, Vec<u8>)> {
    let pod = u32::from(LV2_State_Flags::LV2_STATE_IS_POD);
    let portable = pod | u32::from(LV2_State_Flags::LV2_STATE_IS_PORTABLE);
    match object {
        Object::Iri(iri) => match ttl::path_from_file_uri(iri) {
            // A path names a file on this system, so it is not portable.
            Ok(path) => {
                let mut bytes = path.to_string_lossy().into_owned().into_bytes();
                bytes.push(0);
                Some((ATOM_PATH, pod, bytes))
            }
            Err(_) => {
                let id = urid::global().map(&CString::new(iri.as_str()).ok()?);
                (id != 0).then(|| (ATOM_URID, portable, id.to_ne_bytes().to_vec()))
            }
        },
        Object::Blank(_) => None,
        Object::Literal {
            value,
            datatype: None,
            language: false,
        } => {
            let mut bytes = value.clone().into_bytes();
            bytes.push(0);
            Some((ATOM_STRING, portable, bytes))
        }
        Object::Literal {
            value,
            datatype: Some(datatype),
            language: false,
        } => {
            let value = value.trim();
            let (type_, bytes) = match datatype.strip_prefix(XSD)? {
                "int" | "integer" => (ATOM_INT, value.parse::<i32>().ok()?.to_ne_bytes().to_vec()),
                "long" => (ATOM_LONG, value.parse::<i64>().ok()?.to_ne_bytes().to_vec()),
                "float" | "decimal" => (
                    ATOM_FLOAT,
                    value.parse::<f32>().ok()?.to_ne_bytes().to_vec(),
                ),
                "double" => (
                    ATOM_DOUBLE,
                    value.parse::<f64>().ok()?.to_ne_bytes().to_vec(),
                ),
                "boolean" => {
                    let truth = matches!(value, "true" | "1");
                    (ATOM_BOOL, i32::from(truth).to_ne_bytes().to_vec())
                }
                "string" => {
                    let mut bytes = value.as_bytes().to_vec();
                    bytes.push(0);
                    (ATOM_STRING, bytes)
                }
                _ => return None,
            };
            Some((type_, portable, bytes))
        }
        Object::Literal { .. } => None,
    }
}

/// Restores a plugin's default state.  Instantiation class, at load before
/// `activate`, like [`restore`]; `false` when it has no interface that
/// restores or the plugin refused the values.
pub(crate) fn restore_default(
    handle: LV2_Handle,
    interface: *const LV2_State_Interface,
    state: &DefaultState,
    features: *const *const LV2_Feature,
) -> bool {
    let Some(restore) = (unsafe { interface.as_ref() }).and_then(|interface| interface.restore)
    else {
        eprintln!("[plugins] not loading an LV2 plugin's default state: it cannot restore");
        return false;
    };
    restore_records(restore, handle, &state.records, features)
}

/// What a parsed record holds before its URIs are mapped: the whole blob is
/// checked first, so a malformed one never grows the process-wide URID table.
struct DecodedRecord {
    key: CString,
    type_: CString,
    flags: u32,
    value: Vec<u8>,
}

/// One accepted record with its URIDs, kept in a `Vec` so the value pointers
/// the retrieve callback hands out stay put for the whole restore.
struct RestoreRecord {
    key: LV2_URID,
    type_: LV2_URID,
    flags: u32,
    value: Vec<u8>,
}

/// Parses a blob into records, logging once when it cannot.  A blob from an
/// older or newer build is refused, never trusted and never allowed to
/// panic.
fn decode(bytes: &[u8]) -> Option<Vec<RestoreRecord>> {
    match parse(bytes).and_then(map_records) {
        Ok(records) => Some(records),
        Err(reason) => {
            eprintln!("[plugins] not restoring an LV2 plugin's state: {reason}");
            None
        }
    }
}

fn parse(bytes: &[u8]) -> Result<Vec<DecodedRecord>, &'static str> {
    if bytes.len() < HEADER_BYTES {
        return Err("the blob is too short");
    }
    if &bytes[..8] != MAGIC {
        return Err("the blob is not LV2 state");
    }
    let mut reader = Reader { bytes, offset: 8 };
    let version = reader.u32().ok_or("the version is missing")?;
    if version != VERSION {
        return Err("the version is unknown");
    }
    let count = reader.u32().ok_or("the record count is missing")? as usize;
    if count > reader.remaining() / MIN_RECORD_BYTES {
        return Err("the record count does not fit");
    }
    let mut records = Vec::with_capacity(count);
    for _ in 0..count {
        let key = reader.uri()?;
        let type_ = reader.uri()?;
        let flags = reader.u32().ok_or("a record's flags are missing")?;
        let length = reader.u32().ok_or("a value length is missing")? as usize;
        if length > reader.remaining() {
            return Err("a value length does not fit");
        }
        let value = reader.take(length).ok_or("a value is truncated")?.to_vec();
        records.push(DecodedRecord {
            key,
            type_,
            flags,
            value,
        });
    }
    if reader.remaining() != 0 {
        return Err("the blob has trailing bytes");
    }
    Ok(records)
}

/// Maps the URIs of an accepted blob.  UI thread; the map may insert.  A
/// top-level `atom:URID` value is the URI text and becomes the id the map
/// returns for it now.  An empty, non-UTF8 or interior-NUL URI refuses the
/// blob, as an unmappable key does.  Nested atoms are left as stored.
fn map_records(records: Vec<DecodedRecord>) -> Result<Vec<RestoreRecord>, &'static str> {
    let map = urid::global();
    records
        .into_iter()
        .map(|record| {
            let key = map.map(&record.key);
            let type_ = map.map(&record.type_);
            if key == 0 || type_ == 0 {
                return Err("a key or type URI cannot be mapped");
            }
            let value = if record.type_.as_bytes() == ATOM_URID.to_bytes() {
                mapped_urid(&record.value)?
            } else {
                record.value
            };
            Ok(RestoreRecord {
                key,
                type_,
                flags: record.flags,
                value,
            })
        })
        .collect()
}

/// A stored top-level `atom:URID`: URI bytes in, the current id's
/// native-endian bytes out.
fn mapped_urid(bytes: &[u8]) -> Result<Vec<u8>, &'static str> {
    if bytes.is_empty() || std::str::from_utf8(bytes).is_err() {
        return Err("a key or type URI cannot be mapped");
    }
    let uri = CString::new(bytes).map_err(|_| "a key or type URI cannot be mapped")?;
    let id = urid::global().map(&uri);
    if id == 0 {
        return Err("a key or type URI cannot be mapped");
    }
    Ok(id.to_ne_bytes().to_vec())
}

/// The forward-only reader over a blob.  Every accessor returns `None` when
/// the bytes are not there.
struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    fn remaining(&self) -> usize {
        self.bytes.len() - self.offset
    }

    fn take(&mut self, count: usize) -> Option<&'a [u8]> {
        let end = self.offset.checked_add(count)?;
        let slice = self.bytes.get(self.offset..end)?;
        self.offset = end;
        Some(slice)
    }

    fn u32(&mut self) -> Option<u32> {
        let bytes: [u8; 4] = self.take(4)?.try_into().ok()?;
        Some(u32::from_le_bytes(bytes))
    }

    /// A length-prefixed URI, as a `CString` the URID map can take again.
    fn uri(&mut self) -> Result<CString, &'static str> {
        let length = self.u32().ok_or("a URI length is missing")? as usize;
        if length > self.remaining() {
            return Err("a URI length does not fit");
        }
        let text = self.take(length).ok_or("a URI is truncated")?;
        CString::new(text).map_err(|_| "a URI contains a NUL byte")
    }
}

/// The pointer [`retrieve`] returns for an empty value: non-null and valid
/// for zero bytes.  An empty `Vec`'s `as_ptr` is dangling (though never
/// dereferenced at size 0), and a null pointer would look like "absent".
static EMPTY_VALUE: [u8; 0] = [];

/// `LV2_State_Interface.restore`'s retrieve callback: the last record with
/// `key` (a plugin that stored a key twice meant the later value), or null
/// when the blob has none.  The value stays valid until the restore returns,
/// because the records are not touched during it.  An empty record answers
/// with [`EMPTY_VALUE`], not null, so it is distinguishable from absence.
unsafe extern "C" fn retrieve(
    handle: *mut c_void,
    key: LV2_URID,
    size: *mut usize,
    type_: *mut u32,
    flags: *mut u32,
) -> *const c_void {
    // SAFETY: the handle is the `Vec` `restore` passed, alive for the call.
    let Some(records) = (unsafe { (handle as *const Vec<RestoreRecord>).as_ref() }) else {
        return ptr::null();
    };
    let Some(record) = records.iter().rev().find(|record| record.key == key) else {
        return ptr::null();
    };
    // SAFETY: each pointer the plugin passed is either null or writable for
    // its type, per the retrieve signature.
    unsafe {
        if !size.is_null() {
            *size = record.value.len();
        }
        if !type_.is_null() {
            *type_ = record.type_;
        }
        if !flags.is_null() {
            *flags = record.flags;
        }
    }
    if record.value.is_empty() {
        EMPTY_VALUE.as_ptr() as *const c_void
    } else {
        record.value.as_ptr() as *const c_void
    }
}

/// The name of a state status, for the one log line a failure gets.
fn status_name(status: LV2_State_Status) -> &'static str {
    if status == LV2_STATE_SUCCESS {
        "SUCCESS"
    } else if status == LV2_STATE_ERR_UNKNOWN {
        "ERR_UNKNOWN"
    } else if status == LV2_STATE_ERR_BAD_TYPE {
        "ERR_BAD_TYPE"
    } else if status == LV2_STATE_ERR_BAD_FLAGS {
        "ERR_BAD_FLAGS"
    } else if status == LV2_STATE_ERR_NO_FEATURE {
        "ERR_NO_FEATURE"
    } else if status == LV2_STATE_ERR_NO_PROPERTY {
        "ERR_NO_PROPERTY"
    } else if status == LV2_STATE_ERR_NO_SPACE {
        "ERR_NO_SPACE"
    } else {
        "an unknown status"
    }
}
