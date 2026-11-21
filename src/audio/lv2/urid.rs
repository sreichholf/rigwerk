//! The process-wide LV2 URID map.
//!
//! A plugin maps its URIs once and then passes the numbers around, but some
//! plugins map a URI from inside `run`.  The map therefore has to answer,
//! and insert, from the audio thread without a lock and without allocating.
//! It is one append-only table for the whole process, so the same URI has
//! the same id in every instance and from every thread, as `urid.h`
//! requires.  State blobs store URIs, not numbers, so nothing depends on the
//! numbers across runs.
//!
//! The table is lock-free.  A new URI's bytes are copied into a fixed arena
//! (a bump allocator over a static buffer, claimed with a compare-exchange
//! on its fill), and the entry is claimed with a compare-exchange from null
//! on the first empty slot.  A writer that loses a slot to another URI tries
//! the next; one that loses it to the same URI returns that slot's id.  Ids
//! are slot index + 1, and slots fill without holes, so a lookup walks the
//! entries until the first null.  The CAS publishes the bytes with
//! `Release`; a reader loads an entry with `Acquire`.  Nothing is ever freed
//! or moved, so every pointer `unmap` hands out lives for the process.
//!
//! A full table or arena answers 0, which the spec allows as failure, and
//! keeps answering 0 for every URI it did not already hold, so an id never
//! changes.  The one exception is a race at the moment it fills: a thread
//! refused for want of space can answer 0 while another, which claimed the
//! last space for the same URI, answers its id.  The limits are far above
//! what a host maps.  [`seed`] maps RigWerk's own URIs at startup, so they
//! have the lowest ids.

use std::cell::UnsafeCell;
use std::ffi::{c_char, CStr};
use std::ptr;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use std::sync::Once;

use lv2_sys::{LV2_URID_Map_Handle, LV2_URID_Unmap_Handle, LV2_URID};

/// How many URIs one process may map.  A host sees a few hundred; the limit
/// only exists so the table can be preallocated.  A full table answers 0.
const CAPACITY: usize = 4096;

/// The bytes every mapped URI shares, NUL terminators included.  A static
/// buffer, so it costs address space and only the pages that are used.
const ARENA_BYTES: usize = 1 << 20;

/// The URIs RigWerk's own features use, mapped at startup.  Public to the
/// LV2 module so the test can tie the host's URIs to it.
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

/// The append-only URI table.  Ids start at 1; 0 is failure.
pub(crate) struct UridMap {
    /// Published entries; index `id - 1`.  Each is set once, from null, by a
    /// compare-exchange, and points into `arena`.
    entries: [AtomicPtr<c_char>; CAPACITY],
    /// The URI bytes.  A writer owns the span it claimed through `used` and
    /// writes it before the entry that points at it is published.
    arena: UnsafeCell<[u8; ARENA_BYTES]>,
    /// How many arena bytes are claimed.  Only grows.
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

    /// The id of `uri`, mapping it if it is new.  Safe from any thread, the
    /// audio thread included: it neither locks nor allocates.  Returns 0
    /// only when the table or the arena is full.
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

    /// The stored string for `id`, or null.  The pointer never moves, so the
    /// caller may keep it for as long as the process lives.
    pub(crate) fn unmap(&self, id: LV2_URID) -> *const c_char {
        if id == 0 || id as usize > CAPACITY {
            return ptr::null();
        }
        self.entries[(id - 1) as usize].load(Ordering::Acquire)
    }

    /// The safe form of [`Self::unmap`] for state code.
    pub(crate) fn uri(&self, id: LV2_URID) -> Option<&CStr> {
        let pointer = self.unmap(id);
        if pointer.is_null() {
            None
        } else {
            Some(unsafe { CStr::from_ptr(pointer) })
        }
    }

    /// The id of `uri` if it is published, and the index of the first empty
    /// slot the walk reached (`CAPACITY` when the table is full).
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

    /// Copies `uri` with its NUL into a span of the arena this call claims,
    /// or `None` when it no longer fits.  The span is unpublished until the
    /// caller's compare-exchange stores a pointer to it.
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

/// The one map for the whole process.
pub(crate) fn global() -> &'static UridMap {
    static MAP: UridMap = UridMap::new();
    &MAP
}

/// Maps the URIs RigWerk's features use, so they have the lowest ids.
/// Idempotent and cheap after the first call.
pub(crate) fn seed() {
    static SEEDED_ONCE: Once = Once::new();
    SEEDED_ONCE.call_once(|| {
        let map = global();
        for uri in SEEDED {
            let _ = map.map(uri);
        }
    });
}

/// `LV2_URID_Map.map`: the handle is the process-wide map.
pub(crate) unsafe extern "C" fn map_callback(
    _handle: LV2_URID_Map_Handle,
    uri: *const c_char,
) -> LV2_URID {
    if uri.is_null() {
        return 0;
    }
    global().map(unsafe { CStr::from_ptr(uri) })
}

/// `LV2_URID_Unmap.unmap`: the handle is the process-wide map.
pub(crate) unsafe extern "C" fn unmap_callback(
    _handle: LV2_URID_Unmap_Handle,
    urid: LV2_URID,
) -> *const c_char {
    global().unmap(urid)
}
