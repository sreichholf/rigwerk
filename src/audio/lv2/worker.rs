//! The LV2 worker byte rings.
//!
//! A plugin with `worker:interface` asks the host, from `run`, to call its
//! `work` method later on a non-realtime thread.  The request and the
//! response are opaque byte messages, and the audio side may not lock or
//! allocate for them.  Each instance therefore gets two preallocated SPSC
//! rings: [`WorkerRings::request`] runs audio thread → worker thread, and
//! `response` runs worker thread → audio thread.  This module is only the
//! rings; the worker thread is wired up in a later commit.
//!
//! A message is a 4-byte little-endian length followed by the bytes.  The
//! producer writes the bytes, then publishes the new head with `Release`;
//! the consumer reads the head with `Acquire`, copies the message, then
//! publishes the tail.  A message that does not fit whole is refused, never
//! truncated.

use std::cell::UnsafeCell;
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use lv2_sys::{
    LV2_Worker_Schedule, LV2_Worker_Schedule_Handle, LV2_Worker_Status,
    LV2_Worker_Status_LV2_WORKER_ERR_NO_SPACE as LV2_WORKER_ERR_NO_SPACE,
    LV2_Worker_Status_LV2_WORKER_ERR_UNKNOWN as LV2_WORKER_ERR_UNKNOWN,
    LV2_Worker_Status_LV2_WORKER_SUCCESS as LV2_WORKER_SUCCESS,
};

/// The size of each ring, in bytes, including the length prefixes.  It is a
/// compile-time constant so the caller can allocate one pop buffer at load
/// and never again.
pub(crate) const RING_BYTES: usize = 64 * 1024;

const LENGTH_BYTES: usize = 4;

/// One single-producer single-consumer byte ring.
struct MessageRing {
    /// Owns the fixed buffer.  The hot path never reads it: taking the bytes
    /// out of the `Box` would form a reference to the whole buffer, and the
    /// producer's and consumer's spans are disjoint.
    #[allow(dead_code)]
    buffer: UnsafeCell<Box<[u8]>>,
    /// The first byte of `buffer`, fixed in [`MessageRing::new`].  The
    /// producer writes only between tail and head, the consumer reads only
    /// between tail and head as the producer published it, and each side
    /// has its own index, so no byte needs an atomic.
    storage: *mut u8,
    /// Total bytes ever published.  The producer is the only writer.
    head: AtomicUsize,
    /// Total bytes ever consumed.  The consumer is the only writer.
    tail: AtomicUsize,
}

// SAFETY: the ring is SPSC by construction.  The producer alone writes
// `head` and the bytes it publishes; the consumer alone writes `tail`.  The
// producer never writes beyond the space the consumer's `tail` has released,
// and the consumer only reads bytes a `Release` on `head` has published, so
// the two sides never touch the same byte concurrently.  The request ring
// has two possible producers (the audio thread and, during a restore, the
// UI thread); `WorkerRings::scheduling` serialises them before either
// writes, so it is still one producer at a time.
unsafe impl Sync for MessageRing {}

// SAFETY: sending the ring moves the `Box` that owns the buffer with it, and
// `storage` is only its fixed address.  The SPSC discipline above is what
// makes sharing the ring sound.
unsafe impl Send for MessageRing {}

impl MessageRing {
    fn new() -> Self {
        let mut buffer = vec![0; RING_BYTES].into_boxed_slice();
        // A mutable pointer, so writes through it carry write permission
        // for the whole allocation; the buffer is never moved afterwards.
        let storage = buffer.as_mut_ptr();
        Self {
            buffer: UnsafeCell::new(buffer),
            storage,
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
        }
    }

    #[cfg_attr(not(test), allow(dead_code))] // The worker thread (commit 5) drains it.
    fn pending(&self) -> bool {
        self.head.load(Ordering::Acquire) != self.tail.load(Ordering::Acquire)
    }

    /// Appends one length-prefixed message.  Refuses a message that does not
    /// fit whole.  Producer thread only.
    fn push(&self, data: &[u8]) -> bool {
        let Ok(length) = u32::try_from(data.len()) else {
            return false;
        };
        let total = LENGTH_BYTES + data.len();
        if total > RING_BYTES {
            return false;
        }
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        if head.wrapping_sub(tail) + total > RING_BYTES {
            return false;
        }
        let start = head % RING_BYTES;
        // SAFETY: the producer is the only writer of this region until the
        // new head is published, and the spans it writes are the ones the
        // consumer's tail has already released.
        unsafe {
            let storage = self.storage;
            write_wrapped(storage, start, &length.to_le_bytes());
            write_wrapped(storage, (start + LENGTH_BYTES) % RING_BYTES, data);
        }
        self.head.store(head.wrapping_add(total), Ordering::Release);
        true
    }

    /// Copies the next message into `out` and returns its length, or `None`
    /// when the ring is empty.  `out` must be at least [`RING_BYTES`]: the
    /// caller allocates it once at load, so a pop never allocates and never
    /// copies more than the ring can hold.  Consumer thread only.
    #[cfg_attr(not(test), allow(dead_code))] // The worker thread (commit 5) drains it.
    fn pop_into(&self, out: &mut [u8]) -> Option<usize> {
        debug_assert!(
            out.len() >= RING_BYTES,
            "the pop buffer must hold a whole ring"
        );
        if out.len() < RING_BYTES {
            return None;
        }
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        let used = head.wrapping_sub(tail);
        if used == 0 {
            return None;
        }
        let start = tail % RING_BYTES;
        let mut length_bytes = [0; LENGTH_BYTES];
        // SAFETY: everything between tail and head was published by the
        // producer with `Release` and is no longer written; the consumer
        // touches only that span.
        unsafe {
            let storage = self.storage as *const u8;
            read_wrapped(storage, start, &mut length_bytes);
            let length = u32::from_le_bytes(length_bytes) as usize;
            if LENGTH_BYTES + length > used {
                return None;
            }
            read_wrapped(
                storage,
                (start + LENGTH_BYTES) % RING_BYTES,
                &mut out[..length],
            );
            self.tail
                .store(tail.wrapping_add(LENGTH_BYTES + length), Ordering::Release);
            Some(length)
        }
    }
}

/// Copies `data` into `storage` at `start`, wrapping at the end once.
///
/// # Safety
/// `start` must be below `storage.len()`; the caller must own the region.
unsafe fn write_wrapped(storage: *mut u8, start: usize, data: &[u8]) {
    let first = (RING_BYTES - start).min(data.len());
    // SAFETY: the two spans are inside the ring and do not overlap each
    // other or the source buffer, which the caller guarantees.
    unsafe {
        ptr::copy_nonoverlapping(data.as_ptr(), storage.add(start), first);
        ptr::copy_nonoverlapping(data.as_ptr().add(first), storage, data.len() - first);
    }
}

/// Copies `out.len()` bytes out of `storage` at `start`, wrapping once.
///
/// # Safety
/// `start` must be below `storage.len()`; `out.len()` must not exceed the
/// published region, which the caller checks.
#[cfg_attr(not(test), allow(dead_code))] // The worker thread (commit 5) reads messages.
unsafe fn read_wrapped(storage: *const u8, start: usize, out: &mut [u8]) {
    let length = out.len();
    let first = (RING_BYTES - start).min(length);
    let destination = out.as_mut_ptr();
    // SAFETY: the source spans are the published, unconsumed part of the
    // ring and `out` is a separate buffer, so no span overlaps.
    unsafe {
        ptr::copy_nonoverlapping(storage.add(start), destination, first);
        ptr::copy_nonoverlapping(storage, destination.add(first), length - first);
    }
}

/// One instance's two worker rings.  The caller keeps the `Arc` alive for as
/// long as the plugin holds the schedule struct that points at it;
/// `HostFeatures` owns that `Arc` for the feature's lifetime.
pub(crate) struct WorkerRings {
    request: MessageRing,
    /// The worker thread (commit 5) writes it; the audio side reads it.
    #[cfg_attr(not(test), allow(dead_code))]
    response: MessageRing,
    /// Held while a request is being written.  The plugin normally calls
    /// `schedule_work` from `run`, but may also call it while a restore runs
    /// on another thread; the gate keeps one producer at a time without a
    /// mutex.  A busy gate is reported as no space.
    scheduling: AtomicBool,
}

impl WorkerRings {
    pub(crate) fn new() -> Self {
        Self {
            request: MessageRing::new(),
            response: MessageRing::new(),
            scheduling: AtomicBool::new(false),
        }
    }

    /// Whether the worker thread has a request to pick up.
    #[cfg_attr(not(test), allow(dead_code))] // Wired to the worker thread in commit 5.
    pub(crate) fn request_pending(&self) -> bool {
        self.request.pending()
    }

    /// Whether the audio thread has a response to deliver.
    #[cfg_attr(not(test), allow(dead_code))] // Wired to the worker thread in commit 5.
    pub(crate) fn response_pending(&self) -> bool {
        self.response.pending()
    }

    /// Queues one request from the audio side.  False when the ring is full
    /// or another thread is already writing one.
    pub(crate) fn push_request(&self, data: &[u8]) -> bool {
        if self
            .scheduling
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return false;
        }
        let pushed = self.request.push(data);
        self.scheduling.store(false, Ordering::Release);
        pushed
    }

    /// Takes the oldest request; `out` must be at least [`RING_BYTES`].
    #[cfg_attr(not(test), allow(dead_code))] // Wired to the worker thread in commit 5.
    pub(crate) fn pop_request(&self, out: &mut [u8]) -> Option<usize> {
        self.request.pop_into(out)
    }

    /// Queues one response from the worker side.
    #[cfg_attr(not(test), allow(dead_code))] // Wired to the worker thread in commit 5.
    pub(crate) fn push_response(&self, data: &[u8]) -> bool {
        self.response.push(data)
    }

    /// Takes the oldest response; `out` must be at least [`RING_BYTES`].
    #[cfg_attr(not(test), allow(dead_code))] // Wired to the worker thread in commit 5.
    pub(crate) fn pop_response(&self, out: &mut [u8]) -> Option<usize> {
        self.response.pop_into(out)
    }
}

/// A `LV2_Worker_Schedule` whose handle is `rings`.  The caller keeps the
/// `Arc` alive for as long as the plugin may use the struct.
pub(crate) fn schedule_struct(rings: &Arc<WorkerRings>) -> LV2_Worker_Schedule {
    LV2_Worker_Schedule {
        handle: Arc::as_ptr(rings) as *mut c_void,
        schedule_work: Some(schedule_work),
    }
}

/// `worker:schedule`: copy one message into the request ring.  Audio thread
/// only, so it never locks or allocates.
pub(super) unsafe extern "C" fn schedule_work(
    handle: LV2_Worker_Schedule_Handle,
    size: u32,
    data: *const c_void,
) -> LV2_Worker_Status {
    if data.is_null() && size > 0 {
        return LV2_WORKER_ERR_UNKNOWN;
    }
    let Some(rings) = (unsafe { (handle as *const WorkerRings).as_ref() }) else {
        return LV2_WORKER_ERR_UNKNOWN;
    };
    let payload: &[u8] = if data.is_null() {
        &[]
    } else {
        // SAFETY: the plugin promises `size` readable bytes for this call.
        unsafe { std::slice::from_raw_parts(data as *const u8, size as usize) }
    };
    if rings.push_request(payload) {
        LV2_WORKER_SUCCESS
    } else {
        LV2_WORKER_ERR_NO_SPACE
    }
}
