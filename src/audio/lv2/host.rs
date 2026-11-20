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
//! paths.  There is no `buf-size:nominalBlockLength`: a plugin runs the
//! bridge's block, which a load does not know, so no value would be true.  The
//! loader always passes the instance's rings, so the scan's claim that
//! `worker:schedule` is supplied holds for every load.  `log:log` is not
//! supplied: its variadic callbacks cannot be written in stable Rust.  The
//! scan refuses a plugin that requires a feature outside [`SUPPORTED`].
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

#[cfg_attr(not(test), allow(dead_code))] // Wired to the loader in the next commit.
const URID_MAP: &CStr = c"http://lv2plug.in/ns/ext/urid#map";
#[cfg_attr(not(test), allow(dead_code))] // Wired to the loader in the next commit.
const URID_UNMAP: &CStr = c"http://lv2plug.in/ns/ext/urid#unmap";
#[cfg_attr(not(test), allow(dead_code))] // Wired to the loader in the next commit.
const OPTIONS_OPTIONS: &CStr = c"http://lv2plug.in/ns/ext/options#options";
#[cfg_attr(not(test), allow(dead_code))] // Wired to the loader in the next commit.
const BOUNDED_BLOCK_LENGTH: &CStr = c"http://lv2plug.in/ns/ext/buf-size#boundedBlockLength";
#[cfg_attr(not(test), allow(dead_code))] // Wired to the loader in the next commit.
const WORKER_SCHEDULE: &CStr = c"http://lv2plug.in/ns/ext/worker#schedule";
#[cfg_attr(not(test), allow(dead_code))] // Wired to the loader in the next commit.
const STATE_MAP_PATH: &CStr = c"http://lv2plug.in/ns/ext/state#mapPath";
#[cfg_attr(not(test), allow(dead_code))] // Wired to the loader in the next commit.
const STATE_FREE_PATH: &CStr = c"http://lv2plug.in/ns/ext/state#freePath";

#[cfg_attr(not(test), allow(dead_code))] // Wired to the loader in the next commit.
const BUF_SIZE_MIN: &CStr = c"http://lv2plug.in/ns/ext/buf-size#minBlockLength";
#[cfg_attr(not(test), allow(dead_code))] // Wired to the loader in the next commit.
const BUF_SIZE_MAX: &CStr = c"http://lv2plug.in/ns/ext/buf-size#maxBlockLength";
#[cfg_attr(not(test), allow(dead_code))] // Wired to the loader in the next commit.
const BUF_SIZE_SEQUENCE: &CStr = c"http://lv2plug.in/ns/ext/buf-size#sequenceSize";
#[cfg_attr(not(test), allow(dead_code))] // Wired to the loader in the next commit.
const PARAMETERS_SAMPLE_RATE: &CStr = c"http://lv2plug.in/ns/ext/parameters#sampleRate";
#[cfg_attr(not(test), allow(dead_code))] // Wired to the loader in the next commit.
const ATOM_INT: &CStr = c"http://lv2plug.in/ns/ext/atom#Int";
#[cfg_attr(not(test), allow(dead_code))] // Wired to the loader in the next commit.
const ATOM_FLOAT: &CStr = c"http://lv2plug.in/ns/ext/atom#Float";

/// The features one load hands to `instantiate`, with every string, value
/// and struct they point at.
#[cfg_attr(not(test), allow(dead_code))] // Wired to the loader in the next commit.
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

#[cfg_attr(not(test), allow(dead_code))] // Wired to the loader in the next commit.
impl HostFeatures {
    /// Builds the feature set for one instance.  The loader always passes
    /// the instance's rings; `None` only exists so the feature-set tests can
    /// build the same array without `worker:schedule`.  `urid::seed` runs
    /// here so the host's URIs have the lowest ids.
    pub(crate) fn new(
        sample_rate: f64,
        max_block: usize,
        atom_capacity: u32,
        rings: Option<Arc<WorkerRings>>,
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
            min_block: 1,
            max_block: max_block as i32,
            sequence_size: atom_capacity as i32,
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
        host.build_features();
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

    fn build_features(&mut self) {
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
        BUF_SIZE_MIN,
        BUF_SIZE_MAX,
        BUF_SIZE_SEQUENCE,
        PARAMETERS_SAMPLE_RATE,
        ATOM_INT,
        ATOM_FLOAT,
    ]
}

#[cfg_attr(not(test), allow(dead_code))] // Wired to the loader in the next commit.
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

/// `state:mapPath` identity: duplicate the absolute path and hand it out
/// raw.  Only called by the plugin from inside a state call, on the UI
/// thread, never from `run`, so the copy needs no lock.
#[cfg_attr(not(test), allow(dead_code))] // Wired to the loader in the next commit.
unsafe extern "C" fn map_path(
    _handle: LV2_State_Map_Path_Handle,
    path: *const c_char,
) -> *mut c_char {
    if path.is_null() {
        return ptr::null_mut();
    }
    let owned = unsafe { CStr::from_ptr(path) }.to_owned();
    owned.into_raw()
}

/// `state:freePath`: reclaim a path [`map_path`] handed out.
#[cfg_attr(not(test), allow(dead_code))] // Wired to the loader in the next commit.
unsafe extern "C" fn free_path(_handle: LV2_State_Free_Path_Handle, path: *mut c_char) {
    if path.is_null() {
        return;
    }
    drop(unsafe { CString::from_raw(path) });
}
