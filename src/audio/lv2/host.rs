//! The LV2 host features RigWerk supplies.
//!
//! A plugin receives one null-terminated `LV2_Feature` array at
//! `instantiate`.  Every string and struct that array points at is owned by
//! [`HostFeatures`], which is boxed before the pointers are taken, so a
//! moved handle never moves what a plugin holds.
//!
//! Supplied: `urid:map` and `urid:unmap` (the process-wide map, seeded
//! here), `options:options` (the block bounds, the sequence size and the
//! sample rate), `buf-size:boundedBlockLength`, `worker:schedule` when the
//! instance has rings, and `state:mapPath`/`state:freePath` as identity
//! paths.  A mapped path is a C-heap copy, so a plugin may release it with
//! `free()` (the contract before 1.18) or with `state:freePath`.
//! `state:loadDefaultState` (no data) is supplied only when the bundle has a
//! `state:state`.  A saved blob still gets the feature: that blob is the
//! restore before `run`, and a plugin that requires the feature must see it
//! at instantiate.  It stays in [`SUPPORTED`], so a plugin that requires it
//! is still allowed by the scan; a required feature with no state to restore
//! fails the load instead of instantiating without it.
//! `buf-size:minBlockLength` is 0, because the load calls
//! `run(handle, 0)` and the buf-size spec allows a minimum of zero.  There
//! is no `buf-size:nominalBlockLength`: a plugin runs the bridge's block,
//! which a load does not know, so no value would be true.  The loader
//! always passes the instance's rings, so the scan's claim that
//! `worker:schedule` is supplied holds for every load; its `schedule_work`
//! refuses requests until the load has found the plugin's `work`.  `log:log`
//! is not supplied: its variadic callbacks cannot be written in stable Rust.
//! The scan refuses a plugin that requires a feature outside [`SUPPORTED`].
//!
//! Three core features carry no data and are met by how the host runs a
//! plugin, so they are accepted as required but not put in the array, as
//! jalv does: `lv2:inPlaceBroken` (every port has a buffer of its own, so
//! no input shares an output's memory), `lv2:isLive` (the plugin runs on the
//! live audio thread, block by block) and `lv2:hardRTCapable` (a promise the
//! plugin makes, which asks nothing of the host).

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

/// The features the host supports in phase 1: the ones it supplies plus the
/// three it meets without data.  A required feature outside this list is listed with a
/// scan error and refuses to load later.
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

/// The supported features that are met without an entry in the feature
/// array (see the module doc), for the test that checks the array.
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

/// The options the host supplies in `options:options`.  A plugin that names
/// another as `opts:requiredOption` is listed with a scan error and refuses
/// to load, like an unsupported required feature.
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

/// The features one load hands to `instantiate`, with every string, value
/// and struct they point at.
pub(crate) struct HostFeatures {
    /// The C strings the feature array points at.  A `CString` buffer does
    /// not move when this `Vec` reallocates.
    strings: Vec<CString>,
    urid_map: LV2_URID_Map,
    urid_unmap: LV2_URID_Unmap,
    /// The zero-terminated option array.  Boxed before the feature array is
    /// built, so its address is final.
    options: Box<[LV2_Options_Option]>,
    min_block: i32,
    max_block: i32,
    sequence_size: i32,
    sample_rate: f32,
    /// The instance's rings, owned so the schedule struct's handle cannot
    /// outlive them.
    rings: Option<Arc<WorkerRings>>,
    /// The `worker:schedule` feature data.  Its handle is the `Arc` above.
    schedule: Option<LV2_Worker_Schedule>,
    map_path: LV2_State_Map_Path,
    free_path: LV2_State_Free_Path,
    features: Vec<LV2_Feature>,
    /// `&features[i]` plus the null terminator `as_ptr` returns.
    pointers: Vec<*const LV2_Feature>,
}

impl HostFeatures {
    /// Builds the feature set for one instance.  `sequence_size` is the
    /// largest atom port buffer in bytes, header included
    /// (`PortTable::sequence_size`).  The loader always passes the
    /// instance's rings; `None` only exists so the feature-set tests can
    /// build the same array without `worker:schedule`.  `load_default` is
    /// whether the bundle has a `state:state`: only then is
    /// `state:loadDefaultState` in the array.  `urid::seed` runs here so
    /// the host's URIs have the lowest ids.
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

    /// The null-terminated feature array `instantiate` receives.
    pub(crate) fn as_ptr(&self) -> *const *const LV2_Feature {
        self.pointers.as_ptr()
    }

    /// The option array, terminator included.
    #[cfg(test)]
    pub(crate) fn options(&self) -> &[LV2_Options_Option] {
        &self.options
    }

    /// The path callbacks, for the test that round-trips a string.
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
            // No data: the host meets it by restoring `state:state` at load.
            // Absent when this load has no default state to restore.
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

/// Every URI the host maps or advertises, for the test that ties the seeds
/// to the feature set.  No second copy of the strings.
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

/// `state:mapPath` identity: hand out a copy of `path` on the C heap.
/// Only called by the plugin from inside a state call, on the UI thread,
/// never from `run`, so the copy needs no lock.
///
/// The copy comes from `malloc`, never from Rust's allocator: `state.h`
/// before 1.18 says the caller frees the returned path "with free()", and
/// a plugin written to it (LV2's own `eg-sampler` and `eg-params` still
/// are) never calls `state:freePath`.  A null return is the allocation
/// failure `malloc` reports; the plugin sees no path.  Not `strdup`:
/// the MSVC C runtime only exports it as `_strdup`.
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

/// `state:freePath`: release a path [`map_path`] handed out, with the C
/// `free` it was allocated for, so either way of freeing it is the same.
unsafe extern "C" fn free_path(_handle: LV2_State_Free_Path_Handle, path: *mut c_char) {
    // SAFETY: `path` came from `map_path`'s `malloc`, or is null, which
    // `free` ignores.
    unsafe { libc::free(path.cast()) };
}
