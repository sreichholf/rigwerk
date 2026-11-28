//! Process-wide LV2 URID map.  Some plugins map a URI from inside `run`, so
//! it answers and inserts from the audio thread without a lock or an
//! allocation.  One append-only table: the same URI has the same id in every
//! instance, as `urid.h` requires.  Blobs store URIs, not numbers.
//!
//! A new URI is copied into a fixed arena claimed by compare-exchange, then
//! published with `Release`.  A reader loads with `Acquire`.  Nothing is
//! freed or moved, so every pointer `unmap` hands out lives for the process.
//! A full table answers 0, which the spec allows, and keeps answering 0 for
//! a URI it does not hold, so an id never changes.  The exception is a race
//! as it fills: a thread refused for space can answer 0 while another, which
//! claimed the last space for the same URI, answers its id.  [`seed`] maps
//! RigWerk's URIs first, so they have the lowest ids.

use std::cell::UnsafeCell;
use std::ffi::{c_char, CStr};
use std::ptr;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use std::sync::Once;

use lv2_sys::{LV2_URID_Map_Handle, LV2_URID_Unmap_Handle, LV2_URID};

/// Preallocated, so a map from `run` cannot allocate.  Full answers 0.
const CAPACITY: usize = 4096;

/// Static arena: address space only, pages paid as they are used.
const ARENA_BYTES: usize = 1 << 20;

pub(super) const SEEDED: &[&CStr] = &[
    c"http://lv2plug.in/ns/ext/urid#map",
    c"http://lv2plug.in/ns/ext/urid#unmap",
    c"http://lv2plug.in/ns/ext/options#options",
    c"http://lv2plug.in/ns/ext/buf-size#minBlockLength",
    c"http://lv2plug.in/ns/ext/buf-size#maxBlockLength",
    c"http://lv2plug.in/ns/ext/buf-size#nominalBlockLength",
    c"http://lv2plug.in/ns/ext/buf-size#sequenceSize",
    c"http://lv2plug.in/ns/ext/buf-size#boundedBlockLength",
    c"http://lv2plug.in/ns/ext/buf-size#fixedBlockLength",
    c"http://lv2plug.in/ns/ext/buf-size#powerOf2BlockLength",
    c"http://lv2plug.in/ns/ext/parameters#sampleRate",
    c"http://lv2plug.in/ns/ext/worker#schedule",
    c"http://lv2plug.in/ns/ext/state#mapPath",
    c"http://lv2plug.in/ns/ext/state#freePath",
    c"http://lv2plug.in/ns/ext/state#loadDefaultState",
    c"http://lv2plug.in/ns/ext/state#interface",
    c"http://lv2plug.in/ns/ext/state#threadSafeRestore",
    c"http://lv2plug.in/ns/ext/atom#Float",
    c"http://lv2plug.in/ns/ext/atom#Int",
    c"http://lv2plug.in/ns/ext/atom#Bool",
    c"http://lv2plug.in/ns/ext/atom#URID",
    c"http://lv2plug.in/ns/ext/atom#String",
    c"http://lv2plug.in/ns/ext/atom#Path",
    c"http://lv2plug.in/ns/ext/atom#Sequence",
    c"http://lv2plug.in/ns/ext/atom#Chunk",
    c"http://lv2plug.in/ns/ext/atom#Object",
];

/// Append-only.  Ids start at 1; 0 is failure.
pub(crate) struct UridMap {
    /// Index `id - 1`.  Set once, from null, and points into `arena`.
    entries: [AtomicPtr<c_char>; CAPACITY],
    /// A writer owns the span it claimed through `used` and writes it before
    /// the entry is published.
    arena: UnsafeCell<[u8; ARENA_BYTES]>,
    /// Only grows.
    used: AtomicUsize,
}

// SAFETY: the arena is only written in spans that one writer claimed through
// `used`, before a `Release` compare-exchange publishes the entry pointing at
// them, and only read through entries loaded with `Acquire`.  No byte is
// written after it is published.
unsafe impl Sync for UridMap {}

impl UridMap {
    const fn new() -> Self {
        Self {
            entries: [const { AtomicPtr::new(ptr::null_mut()) }; CAPACITY],
            arena: UnsafeCell::new([0; ARENA_BYTES]),
            used: AtomicUsize::new(0),
        }
    }

    /// Safe from any thread, including the audio thread: no lock, no
    /// allocation.  0 only when the table or the arena is full.
    pub(crate) fn map(&self, uri: &CStr) -> LV2_URID {
        let (found, mut index) = self.find(uri);
        if let Some(id) = found {
            return id;
        }
        if index >= CAPACITY {
            return 0;
        }
        let Some(copy) = self.copy(uri) else {
            return 0;
        };
        while index < CAPACITY {
            match self.entries[index].compare_exchange(
                ptr::null_mut(),
                copy,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return index as LV2_URID + 1,
                Err(other) => {
                    // SAFETY: a published entry points at a NUL-terminated
                    // copy in the arena that is never written again.
                    if unsafe { CStr::from_ptr(other) } == uri {
                        return index as LV2_URID + 1;
                    }
                    index += 1;
                }
            }
        }
        0
    }

    /// The pointer never moves, so the caller may keep it for the process.
    pub(crate) fn unmap(&self, id: LV2_URID) -> *const c_char {
        if id == 0 || id as usize > CAPACITY {
            return ptr::null();
        }
        self.entries[(id - 1) as usize].load(Ordering::Acquire)
    }

    pub(crate) fn uri(&self, id: LV2_URID) -> Option<&CStr> {
        let pointer = self.unmap(id);
        if pointer.is_null() {
            None
        } else {
            Some(unsafe { CStr::from_ptr(pointer) })
        }
    }

    fn find(&self, uri: &CStr) -> (Option<LV2_URID>, usize) {
        for index in 0..CAPACITY {
            let pointer = self.entries[index].load(Ordering::Acquire);
            if pointer.is_null() {
                return (None, index);
            }
            // SAFETY: as in `map`, a published entry is a finished copy.
            if unsafe { CStr::from_ptr(pointer) } == uri {
                return (Some(index as LV2_URID + 1), index);
            }
        }
        (None, CAPACITY)
    }

    /// The claimed span stays unpublished until the caller's
    /// compare-exchange.
    fn copy(&self, uri: &CStr) -> Option<*mut c_char> {
        let bytes = uri.to_bytes_with_nul();
        let start = self
            .used
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                used.checked_add(bytes.len())
                    .filter(|end| *end <= ARENA_BYTES)
            })
            .ok()?;
        // SAFETY: `start..start + len` is inside the arena and was claimed
        // by this call alone; no entry points at it yet.
        unsafe {
            let destination = (self.arena.get() as *mut u8).add(start);
            ptr::copy_nonoverlapping(bytes.as_ptr(), destination, bytes.len());
            Some(destination as *mut c_char)
        }
    }

    #[cfg(test)]
    pub(crate) fn contains_for_test(&self, uri: &CStr) -> bool {
        self.find(uri).0.is_some()
    }
}

pub(crate) fn global() -> &'static UridMap {
    static MAP: UridMap = UridMap::new();
    &MAP
}

/// Maps RigWerk's feature URIs first, so they have the lowest ids.
pub(crate) fn seed() {
    static SEEDED_ONCE: Once = Once::new();
    SEEDED_ONCE.call_once(|| {
        let map = global();
        for uri in SEEDED {
            let _ = map.map(uri);
        }
    });
}

/// `LV2_URID_Map.map`.  The handle is ignored; the map is process-wide.
pub(crate) unsafe extern "C" fn map_callback(
    _handle: LV2_URID_Map_Handle,
    uri: *const c_char,
) -> LV2_URID {
    if uri.is_null() {
        return 0;
    }
    global().map(unsafe { CStr::from_ptr(uri) })
}

/// `LV2_URID_Unmap.unmap`.  The handle is ignored; the map is process-wide.
pub(crate) unsafe extern "C" fn unmap_callback(
    _handle: LV2_URID_Unmap_Handle,
    urid: LV2_URID,
) -> *const c_char {
    global().unmap(urid)
}
