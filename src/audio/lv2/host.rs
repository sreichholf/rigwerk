//! Features handed to `instantiate`.  [`HostFeatures`] is boxed before the
//! pointers are taken, so a moved handle never moves what a plugin holds.
//!
//! `state:mapPath` returns a C-heap copy: a plugin may `free()` it (the
//! contract before 1.18) or call `state:freePath`.  `state:loadDefaultState`
//! is in the array only when the bundle has a `state:state`.  A saved blob
//! still gets it, because a plugin that requires the feature must see it at
//! instantiate; with nothing to restore, the load fails.  It stays in
//! [`SUPPORTED`], so the scan still allows a plugin that requires it.
//! `buf-size:minBlockLength` is 0 because the load calls `run(handle, 0)`.
//! There is no `nominalBlockLength`: a load does not know the bridge's
//! block.  `schedule_work` refuses until the load has found `work`.
//! `log:log` is omitted: its variadic callbacks cannot be written in stable
//! Rust.  A required feature outside [`SUPPORTED`] is a scan error and the
//! loader refuses it.
//!
//! `lv2:inPlaceBroken`, `lv2:isLive` and `lv2:hardRTCapable` are accepted as
//! required but not put in the array, as jalv does.  Each port has its own
//! buffer, the plugin runs on the live audio thread, and `hardRTCapable` is
//! the plugin's promise.

use std::ffi::{c_char, c_void, CStr, CString};
use std::ptr;
use std::sync::Arc;

use lv2_sys::{
    LV2_Feature, LV2_Options_Context_LV2_OPTIONS_INSTANCE, LV2_Options_Option, LV2_State_Free_Path,
    LV2_State_Free_Path_Handle, LV2_State_Map_Path, LV2_State_Map_Path_Handle, LV2_URID_Map,
    LV2_URID_Unmap, LV2_Worker_Schedule, LV2_URID,
};

use super::urid;
use super::worker::{self, WorkerRings};

/// A required feature outside this list is a scan error and refuses to load.
pub(crate) const SUPPORTED: &[&str] = &[
    "http://lv2plug.in/ns/ext/urid#map",
    "http://lv2plug.in/ns/ext/urid#unmap",
    "http://lv2plug.in/ns/ext/options#options",
    "http://lv2plug.in/ns/ext/buf-size#boundedBlockLength",
    "http://lv2plug.in/ns/ext/worker#schedule",
    "http://lv2plug.in/ns/ext/state#mapPath",
    "http://lv2plug.in/ns/ext/state#freePath",
    "http://lv2plug.in/ns/ext/state#loadDefaultState",
    IN_PLACE_BROKEN,
    IS_LIVE,
    HARD_RT_CAPABLE,
];

/// Supported, but met without an array entry.
#[cfg(test)]
pub(super) const SATISFIED: &[&str] = &[IN_PLACE_BROKEN, IS_LIVE, HARD_RT_CAPABLE];

const IN_PLACE_BROKEN: &str = "http://lv2plug.in/ns/lv2core#inPlaceBroken";
const IS_LIVE: &str = "http://lv2plug.in/ns/lv2core#isLive";
const HARD_RT_CAPABLE: &str = "http://lv2plug.in/ns/lv2core#hardRTCapable";

const URID_MAP: &CStr = c"http://lv2plug.in/ns/ext/urid#map";
const URID_UNMAP: &CStr = c"http://lv2plug.in/ns/ext/urid#unmap";
const OPTIONS_OPTIONS: &CStr = c"http://lv2plug.in/ns/ext/options#options";
const BOUNDED_BLOCK_LENGTH: &CStr = c"http://lv2plug.in/ns/ext/buf-size#boundedBlockLength";
const WORKER_SCHEDULE: &CStr = c"http://lv2plug.in/ns/ext/worker#schedule";
const STATE_MAP_PATH: &CStr = c"http://lv2plug.in/ns/ext/state#mapPath";
const STATE_FREE_PATH: &CStr = c"http://lv2plug.in/ns/ext/state#freePath";
const STATE_LOAD_DEFAULT: &CStr = c"http://lv2plug.in/ns/ext/state#loadDefaultState";

/// A required option outside this list is a scan error and refuses to load.
pub(crate) const SUPPLIED_OPTIONS: &[&str] = &[
    "http://lv2plug.in/ns/ext/buf-size#minBlockLength",
    "http://lv2plug.in/ns/ext/buf-size#maxBlockLength",
    "http://lv2plug.in/ns/ext/buf-size#sequenceSize",
    "http://lv2plug.in/ns/ext/parameters#sampleRate",
];

const BUF_SIZE_MIN: &CStr = c"http://lv2plug.in/ns/ext/buf-size#minBlockLength";
const BUF_SIZE_MAX: &CStr = c"http://lv2plug.in/ns/ext/buf-size#maxBlockLength";
const BUF_SIZE_SEQUENCE: &CStr = c"http://lv2plug.in/ns/ext/buf-size#sequenceSize";
const PARAMETERS_SAMPLE_RATE: &CStr = c"http://lv2plug.in/ns/ext/parameters#sampleRate";
const ATOM_INT: &CStr = c"http://lv2plug.in/ns/ext/atom#Int";
const ATOM_FLOAT: &CStr = c"http://lv2plug.in/ns/ext/atom#Float";

/// Owns every string and struct the feature array points at.
pub(crate) struct HostFeatures {
    /// A `CString` buffer does not move when this `Vec` reallocates.
    strings: Vec<CString>,
    urid_map: LV2_URID_Map,
    urid_unmap: LV2_URID_Unmap,
    /// Boxed before the feature array is built, so its address is final.
    options: Box<[LV2_Options_Option]>,
    min_block: i32,
    max_block: i32,
    sequence_size: i32,
    sample_rate: f32,
    /// Owned so the schedule struct's handle cannot outlive the rings.
    rings: Option<Arc<WorkerRings>>,
    /// Its handle is the rings `Arc`.
    schedule: Option<LV2_Worker_Schedule>,
    map_path: LV2_State_Map_Path,
    free_path: LV2_State_Free_Path,
    features: Vec<LV2_Feature>,
    /// `&features[i]`, plus the null terminator `as_ptr` returns.
    pointers: Vec<*const LV2_Feature>,
}

impl HostFeatures {
    /// `sequence_size` is the largest atom port buffer in bytes, header
    /// included.  `rings` is `None` only so tests can omit `worker:schedule`.
    /// `load_default` puts `state:loadDefaultState` in the array only when
    /// the bundle has a `state:state`.  `urid::seed` runs here so the host's
    /// URIs have the lowest ids.
    pub(crate) fn new(
        sample_rate: f64,
        max_block: usize,
        sequence_size: u32,
        rings: Option<Arc<WorkerRings>>,
        load_default: bool,
    ) -> Box<Self> {
        urid::seed();
        // Box first: the option values and feature data point into these
        // fields, so they must already be at their final address.
        let mut host = Box::new(Self {
            strings: Vec::new(),
            urid_map: LV2_URID_Map {
                handle: urid::global() as *const _ as *mut c_void,
                map: Some(urid::map_callback),
            },
            urid_unmap: LV2_URID_Unmap {
                handle: urid::global() as *const _ as *mut c_void,
                unmap: Some(urid::unmap_callback),
            },
            options: Vec::new().into_boxed_slice(),
            // The load calls `run(handle, 0)`, and the buf-size spec allows
            // a minimum of zero.
            min_block: 0,
            max_block: max_block as i32,
            sequence_size: sequence_size as i32,
            sample_rate: sample_rate as f32,
            rings,
            schedule: None,
            map_path: LV2_State_Map_Path {
                handle: ptr::null_mut(),
                abstract_path: Some(map_path),
                absolute_path: Some(map_path),
            },
            free_path: LV2_State_Free_Path {
                handle: ptr::null_mut(),
                free_path: Some(free_path),
            },
            features: Vec::new(),
            pointers: Vec::new(),
        });
        host.schedule = host.rings.as_ref().map(worker::schedule_struct);
        host.build_options();
        host.build_features(load_default);
        host
    }

    pub(crate) fn as_ptr(&self) -> *const *const LV2_Feature {
        self.pointers.as_ptr()
    }

    #[cfg(test)]
    pub(crate) fn options(&self) -> &[LV2_Options_Option] {
        &self.options
    }

    #[cfg(test)]
    pub(crate) fn paths(&self) -> (&LV2_State_Map_Path, &LV2_State_Free_Path) {
        (&self.map_path, &self.free_path)
    }

    fn build_options(&mut self) {
        let map = urid::global();
        let int = map.map(ATOM_INT);
        let float = map.map(ATOM_FLOAT);
        let min = map.map(BUF_SIZE_MIN);
        let max = map.map(BUF_SIZE_MAX);
        let sequence = map.map(BUF_SIZE_SEQUENCE);
        let sample_rate = map.map(PARAMETERS_SAMPLE_RATE);
        let value_size = std::mem::size_of::<i32>() as u32;
        self.options = vec![
            option(min, int, value_size, &self.min_block),
            option(max, int, value_size, &self.max_block),
            option(sequence, int, value_size, &self.sequence_size),
            option(sample_rate, float, value_size, &self.sample_rate),
            // A zeroed option terminates the array.
            LV2_Options_Option {
                context: 0,
                subject: 0,
                key: 0,
                size: 0,
                type_: 0,
                value: ptr::null(),
            },
        ]
        .into_boxed_slice();
    }

    fn build_features(&mut self, load_default: bool) {
        let mut strings: Vec<CString> = Vec::with_capacity(SUPPORTED.len());
        let mut features: Vec<LV2_Feature> = Vec::with_capacity(SUPPORTED.len());
        {
            let mut push = |uri: &CStr, data: *mut c_void| {
                let owned = uri.to_owned();
                let pointer = owned.as_ptr();
                strings.push(owned);
                features.push(LV2_Feature { URI: pointer, data });
            };
            push(
                URID_MAP,
                &self.urid_map as *const LV2_URID_Map as *mut c_void,
            );
            push(
                URID_UNMAP,
                &self.urid_unmap as *const LV2_URID_Unmap as *mut c_void,
            );
            push(OPTIONS_OPTIONS, self.options.as_ptr() as *mut c_void);
            push(BOUNDED_BLOCK_LENGTH, ptr::null_mut());
            if let Some(schedule) = &self.schedule {
                push(
                    WORKER_SCHEDULE,
                    schedule as *const LV2_Worker_Schedule as *mut c_void,
                );
            }
            push(
                STATE_MAP_PATH,
                &self.map_path as *const LV2_State_Map_Path as *mut c_void,
            );
            push(
                STATE_FREE_PATH,
                &self.free_path as *const LV2_State_Free_Path as *mut c_void,
            );
            // No data: met by restoring `state:state` at load.
            if load_default {
                push(STATE_LOAD_DEFAULT, ptr::null_mut());
            }
        }
        self.strings = strings;
        self.features = features;
        let mut pointers: Vec<*const LV2_Feature> = self
            .features
            .iter()
            .map(|feature| feature as *const LV2_Feature)
            .collect();
        pointers.push(ptr::null());
        self.pointers = pointers;
    }
}

/// By reference, so the seed test and the feature set share one copy.
#[cfg(test)]
pub(super) fn used_uris() -> &'static [&'static CStr] {
    &[
        URID_MAP,
        URID_UNMAP,
        OPTIONS_OPTIONS,
        BOUNDED_BLOCK_LENGTH,
        WORKER_SCHEDULE,
        STATE_MAP_PATH,
        STATE_FREE_PATH,
        STATE_LOAD_DEFAULT,
        BUF_SIZE_MIN,
        BUF_SIZE_MAX,
        BUF_SIZE_SEQUENCE,
        PARAMETERS_SAMPLE_RATE,
        ATOM_INT,
        ATOM_FLOAT,
    ]
}

fn option<T>(key: LV2_URID, type_: LV2_URID, size: u32, value: *const T) -> LV2_Options_Option {
    LV2_Options_Option {
        context: LV2_Options_Context_LV2_OPTIONS_INSTANCE,
        subject: 0,
        key,
        size,
        type_,
        value: value as *const c_void,
    }
}

/// `state:mapPath`: a C-heap copy of `path`.  Called only from a state call
/// on the UI thread, never from `run`.
///
/// `malloc`, not Rust's allocator: `state.h` before 1.18 says the caller
/// frees the path with `free()`, and `eg-sampler` and `eg-params` still do,
/// never calling `state:freePath`.  Null is `malloc` failure.  Not
/// `strdup`: MSVC exports it as `_strdup`.
unsafe extern "C" fn map_path(
    _handle: LV2_State_Map_Path_Handle,
    path: *const c_char,
) -> *mut c_char {
    if path.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: the plugin passes a null-terminated string, live for the
    // call; the copy is the string and its NUL, into a fresh allocation of
    // exactly that size.
    unsafe {
        let size = CStr::from_ptr(path).to_bytes_with_nul().len();
        let copy = libc::malloc(size).cast::<c_char>();
        if !copy.is_null() {
            ptr::copy_nonoverlapping(path, copy, size);
        }
        copy
    }
}

/// `state:freePath`: `free` a path [`map_path`] allocated, matching `free()`.
unsafe extern "C" fn free_path(_handle: LV2_State_Free_Path_Handle, path: *mut c_char) {
    // SAFETY: `path` came from `map_path`'s `malloc`, or is null, which
    // `free` ignores.
    unsafe { libc::free(path.cast()) };
}
