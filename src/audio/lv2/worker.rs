//! The LV2 worker rings and the process-wide worker thread.
//!
//! A plugin with `worker:interface` asks the host, from `run`, to call its
//! `work` method later on a non-realtime thread.  The request and the
//! response are opaque byte messages, and the audio side may not lock or
//! allocate for them.  Each instance therefore gets two preallocated SPSC
//! rings: [`WorkerRings::request`] runs audio thread → worker thread, and
//! `response` runs worker thread → audio thread.
//!
//! One thread, `rigwerk-lv2-worker`, serves every registered instance.  It
//! takes a snapshot of the registry under the registry lock, drops the lock,
//! and for each instance takes that instance's gate and drains its requests
//! into one preallocated buffer.  The UI's `service` unparks it when a
//! request is waiting, on each frame the UI draws.  An idle UI may not
//! repaint, so that is not the bound: while any instance is registered the
//! thread also parks for at most [`PARK_TIMEOUT`], never in a busy loop, and
//! a request waits at most that long plus the `work` calls ahead of it.
//! With none registered it parks without a timeout: [`register`] unparks
//! it, so an idle process has no thread waking up.
//!
//! Thread rules: `work` runs on the worker thread, one call per instance at
//! a time, never concurrently with `deactivate`/`cleanup` after
//! [`unregister`].  The load is the exception: requests a restore queued are
//! drained on the UI thread, by [`drain_requests`], before the instance is
//! registered and before any `run`, so nothing else is inside the plugin
//! then.  [`unregister`] (UI thread) marks the instance removed, takes it
//! out of the registry and then waits on its gate, so a `work` already
//! inside finishes before the caller may clean the plugin up.  It then
//! finishes the exchange itself, before `deactivate`: `worker.h` says an
//! accepted request must reach `work` and an accepted response must reach
//! `work_response`, so it serves the queued requests and delivers the
//! responses, in rounds, until both rings are empty (bounded by
//! [`TEARDOWN_ROUNDS`]).  `work_response` is an audio-class call, not an
//! audio-thread one; the processor is gone by then, so no `run` or other
//! audio-class call can be inside, as at the load's zero-frame run.
//! `end_run` is not called there: `worker.h` ties it to a `run` cycle, and
//! no `run` precedes the teardown's deliveries.  The worker thread is a non-realtime context: a plugin's `work` may
//! lock, allocate and read files there.
//!
//! The thread runs for the life of the process and is never shut down.  Its
//! handle is set once, so a thread that exits could not be restarted; the
//! loop has no exit today, which is what makes that safe.
//!
//! A message is a 4-byte little-endian length followed by the bytes.  The
//! producer writes the bytes, then publishes the new head with `Release`;
//! the consumer reads the head with `Acquire`, copies the message, then
//! publishes the tail.  A message that does not fit whole is refused, never
//! truncated.  The response ring's producer is the worker thread after
//! registration.  At load, before that, the UI thread writes the responses
//! for the requests it drained and reads them back before `run(0)`.  At
//! teardown, once the processor is gone and the worker thread is out, the
//! UI thread is both its producer and its consumer.

use std::cell::UnsafeCell;
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use lv2_sys::{
    LV2_Handle, LV2_Worker_Interface, LV2_Worker_Respond_Handle, LV2_Worker_Schedule,
    LV2_Worker_Schedule_Handle, LV2_Worker_Status,
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
// writes, so it is still one producer at a time.  The response ring is
// written by the worker thread, and by the UI thread at load before the
// instance is registered and at teardown after `unregister` shut the worker
// thread out, so those writers never overlap.  Its reader is the processor,
// or the UI thread when no processor exists.
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
    /// Responses for the audio side.  The worker thread writes it after
    /// registration.  At load, before that, the UI thread writes the
    /// responses for requests it drained and reads them in the zero-frame
    /// run.
    response: MessageRing,
    /// Held while a request is being written.  The plugin normally calls
    /// `schedule_work` from `run`, but may also call it while a restore runs
    /// on another thread; the gate keeps one producer at a time without a
    /// mutex.  A busy gate is reported as no space.
    scheduling: AtomicBool,
    /// Whether a request can ever reach `work`: set by the load once it
    /// found a worker interface with a `work` function, before the restore
    /// and the first `run`.  Until then `schedule_work` refuses, so an
    /// instance without a worker never has a request accepted.
    accepting: AtomicBool,
}

impl WorkerRings {
    pub(crate) fn new() -> Self {
        Self {
            request: MessageRing::new(),
            response: MessageRing::new(),
            scheduling: AtomicBool::new(false),
            accepting: AtomicBool::new(false),
        }
    }

    /// Lets `schedule_work` accept requests.  UI thread, at load, once the
    /// plugin's worker interface is known to have `work`.  Requests a restore
    /// queues are drained on this thread before [`register`]; requests queued
    /// after that wait for the worker thread.
    pub(crate) fn accept_requests(&self) {
        self.accepting.store(true, Ordering::Release);
    }

    /// Whether the worker thread has a request to pick up.
    pub(crate) fn request_pending(&self) -> bool {
        self.request.pending()
    }

    /// Whether the audio thread has a response to deliver.
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
    pub(crate) fn pop_request(&self, out: &mut [u8]) -> Option<usize> {
        self.request.pop_into(out)
    }

    /// Queues one response from the worker side.
    pub(crate) fn push_response(&self, data: &[u8]) -> bool {
        self.response.push(data)
    }

    /// Takes the oldest response; `out` must be at least [`RING_BYTES`].
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

/// `worker:schedule`: copy one message into the request ring.  It never runs
/// `work` and never locks or allocates.  `run` calls it, and so do a restore
/// and `work` during the load drain, both on the UI thread.  `worker.h`
/// makes an accepted request a promise that it reaches `work`, so an
/// instance whose rings do not accept requests (no worker interface, or one
/// without `work`) gets `LV2_WORKER_ERR_UNKNOWN`.
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
    if !rings.accepting.load(Ordering::Acquire) {
        return LV2_WORKER_ERR_UNKNOWN;
    }
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

/// How long the worker thread sits parked when no one wakes it while an
/// instance is registered.  It bounds how long a request waits when the UI
/// draws no frame (an idle UI may not repaint), and it is the safety net for
/// a wake that raced a request.
const PARK_TIMEOUT: Duration = Duration::from_millis(50);

/// One registered plugin instance, as the worker thread sees it.
///
/// Thread rules: `handle` and `interface` are dereferenced only while the
/// instance's `gate` is held: on the worker thread, inside `work`, while
/// `removed` is false, and in [`unregister`] on the UI thread, which sets
/// `removed` and then waits on `gate` before it finishes the exchange.  Only
/// then may the caller call `deactivate`, `cleanup` or unload the module, so
/// a `work` already inside finishes first.  `rings` is shared with the
/// schedule feature and the processor.
pub(crate) struct WorkerInstance {
    /// The instance `work` is called on.  Kept by [`super::instance::Lv2Core`]
    /// until after [`unregister`].
    handle: LV2_Handle,
    /// Points into the module the core keeps loaded.  Its `work`,
    /// `work_response` and `end_run` functions live there.
    interface: *const LV2_Worker_Interface,
    /// The instance's rings; also the handle [`respond`] receives.
    rings: Arc<WorkerRings>,
    /// Set by [`unregister`] before it waits on the gate.  The worker thread
    /// checks it while holding the gate, so no call starts after the wait.
    removed: AtomicBool,
    /// Whether a non-success `work` status has been logged.  One line per
    /// instance, not one per request, from the worker thread.
    failed: AtomicBool,
    /// Held by the worker thread for the whole of one instance's `work`
    /// calls, and taken by [`unregister`] to wait for an in-flight call.
    gate: Mutex<()>,
}

// SAFETY: sharing only needs the fields to be read safely.  `interface` is
// immutable data in a library the core keeps loaded for longer than the
// registration, and `handle` is only passed to `work` on the worker thread.
// The rings are the SPSC pair above.  What a `work` call does with the
// instance is the plugin's own contract, and LV2 forbids concurrent calls,
// which the per-instance gate and the single thread enforce.
unsafe impl Send for WorkerInstance {}
unsafe impl Sync for WorkerInstance {}

/// Every registered instance.  The worker thread clones the `Arc`s out under
/// this lock and drops it before it calls any plugin.
static INSTANCES: Mutex<Vec<Arc<WorkerInstance>>> = Mutex::new(Vec::new());

/// The worker thread's handle, set when the thread is first spawned.
static THREAD: OnceLock<std::thread::Thread> = OnceLock::new();

/// Serialises the first spawn, so two concurrent loads start one thread, not
/// two.
static STARTING: Mutex<()> = Mutex::new(());

/// The registry, also when a previous holder panicked.
fn registry() -> std::sync::MutexGuard<'static, Vec<Arc<WorkerInstance>>> {
    INSTANCES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Starts the process-wide worker thread on first use and returns its handle.
fn thread() -> Result<&'static std::thread::Thread, String> {
    if let Some(thread) = THREAD.get() {
        return Ok(thread);
    }
    let starting = STARTING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(thread) = THREAD.get() {
        return Ok(thread);
    }
    let spawned = std::thread::Builder::new()
        .name("rigwerk-lv2-worker".to_owned())
        .spawn(worker_loop)
        .map_err(|error| format!("The LV2 worker thread could not start: {error}"))?;
    THREAD
        .set(spawned.thread().clone())
        .expect("the STARTING lock keeps the worker thread's handle empty here");
    drop(starting);
    Ok(THREAD.get().expect("the worker thread was just started"))
}

/// Registers one instance with the worker thread and makes sure the thread
/// runs.  UI thread, after `activate` and the load's `run(0)`, so a restore
/// cannot race a worker.  The caller keeps the returned `Arc` (in the core)
/// and passes it to [`unregister`] before cleanup.
///
/// An interface without a `work` function is refused here, not only by the
/// caller: an instance the thread can never serve would leave its requests
/// waiting forever.
pub(crate) fn register(
    handle: LV2_Handle,
    interface: *const LV2_Worker_Interface,
    rings: Arc<WorkerRings>,
) -> Result<Arc<WorkerInstance>, String> {
    if interface.is_null() {
        return Err("The LV2 plugin has no worker interface.".to_owned());
    }
    // SAFETY: the caller resolved the interface from the plugin's descriptor,
    // which the module keeps loaded for longer than the registration.
    let has_work = unsafe { (*interface).work.is_some() };
    if !has_work {
        return Err("The LV2 worker interface has no work function.".to_owned());
    }
    let thread = thread()?;
    let instance = Arc::new(WorkerInstance {
        handle,
        interface,
        rings,
        removed: AtomicBool::new(false),
        failed: AtomicBool::new(false),
        gate: Mutex::new(()),
    });
    registry().push(Arc::clone(&instance));
    thread.unpark();
    Ok(instance)
}

/// Wakes the worker thread when `instance` has a request waiting.  UI thread
/// (the per-frame service) and tests.  It only shortens the wait: a frame
/// the UI does not draw wakes nothing, and then [`PARK_TIMEOUT`] bounds it.
pub(crate) fn wake(instance: &WorkerInstance) {
    if instance.removed.load(Ordering::Acquire) {
        return;
    }
    if instance.rings.request_pending() {
        if let Some(thread) = THREAD.get() {
            thread.unpark();
        }
    }
}

/// Takes `instance` out of the worker's reach and finishes its exchange.  UI
/// thread, from the core's drop, once the processor is gone and before
/// `deactivate`/`cleanup`: once this returns, no `work` call is inside or
/// can start, so the plugin may be cleaned up and its module unloaded.
///
/// What is still queued is finished here, under the gate.  `worker.h` says
/// a request `schedule_work` accepted must reach `work`, and a response
/// `respond` accepted must reach `work_response`; a plugin often frees what
/// it replaced that way (`eg-sampler` answers a loaded sample by scheduling
/// the old one's free), so dropping either would leak.  Each round serves
/// every queued request, then delivers every response.  `end_run` is not
/// called: `worker.h` makes it the close of a cycle "after every run()",
/// and these rounds follow no `run`.  A response may schedule another
/// request, so the rounds go on until both rings are empty.  A plugin still
/// exchanging messages after [`TEARDOWN_ROUNDS`] has the rest dropped, with
/// one log line, and whatever they held leaks.
///
/// The registry lock is not held while waiting: the worker thread takes it
/// only to copy the registry, never while it calls a plugin.
///
/// The UI thread is blocked for as long as the `work` call already inside
/// takes, plus the queued messages.  That is the price of the teardown
/// order: a request served after this returned could reach a plugin that
/// is being cleaned up.  `work` is meant for bounded jobs (loading or
/// freeing a sample), so the wait is the plugin's own latency, paid once,
/// when the effect is removed.
pub(crate) fn unregister(instance: &Arc<WorkerInstance>) {
    instance.removed.store(true, Ordering::Release);
    registry().retain(|registered| !Arc::ptr_eq(registered, instance));
    if let Some(thread) = THREAD.get() {
        thread.unpark();
    }
    // The worker checks `removed` while holding the gate, so holding it here
    // waits out an in-flight `work` and blocks a new one.
    let _gate = instance
        .gate
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if instance.rings.request_pending() || instance.rings.response_pending() {
        let mut message = vec![0u8; RING_BYTES];
        // SAFETY: the gate is held, the core has neither deactivated nor
        // cleaned the plugin up, and the processor is gone, so no `run` can
        // be inside.
        unsafe { finish(instance, &mut message) };
    }
}

/// How many rounds of requests and responses [`unregister`] runs before it
/// gives up on a plugin that keeps answering a response with a new request.
/// `eg-sampler`'s exchange (load, then free the old sample) takes one.
pub(super) const TEARDOWN_ROUNDS: usize = 64;

/// Serves `instance`'s queued requests and delivers its responses, in
/// rounds, until both rings are empty or [`TEARDOWN_ROUNDS`] rounds ran.
/// `message` is a whole ring.
///
/// # Safety
///
/// The caller holds the instance's gate.  The plugin is active, neither
/// deactivated nor cleaned up, its module is loaded, and no `run` or other
/// audio-class call can be inside it.
unsafe fn finish(instance: &WorkerInstance, message: &mut [u8]) {
    // SAFETY: `register` refused a null interface, and the module the core
    // keeps loaded owns it.
    let interface = unsafe { &*instance.interface };
    let mut rounds = 0;
    loop {
        // SAFETY: the caller's contract, which is `serve`'s.
        unsafe { serve(instance, message) };
        if !instance.rings.response_pending() {
            return;
        }
        if rounds == TEARDOWN_ROUNDS {
            eprintln!(
                "[plugins] an LV2 plugin still exchanged worker messages at teardown; the rest are dropped"
            );
            return;
        }
        rounds += 1;
        super::processor::deliver_responses(
            instance.handle,
            &instance.rings,
            message,
            interface.work_response,
        );
    }
}

/// How many instances are registered, for the tests that must leave none
/// behind.
#[cfg(test)]
pub(super) fn registered_for_test() -> usize {
    registry().len()
}

/// How many times the worker loop has taken a snapshot, for the test that
/// an empty registry keeps the thread asleep.
#[cfg(test)]
static LOOPS: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
pub(super) fn loops_for_test() -> usize {
    LOOPS.load(Ordering::Relaxed)
}

/// For the tests: locks `instance`'s gate, which keeps `work` out until the
/// guard drops.
#[cfg(test)]
pub(super) fn hold_gate(instance: &WorkerInstance) -> std::sync::MutexGuard<'_, ()> {
    instance
        .gate
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Drains every registered instance's requests.  Worker thread only, the
/// non-realtime context in which the plugin may lock, allocate and read
/// files.
fn worker_loop() {
    // One buffer for the whole thread: a pop never allocates.
    let mut request = vec![0u8; RING_BYTES];
    loop {
        // Cloning the `Arc`s under the lock and dropping it first keeps a
        // plugin call from holding the registry while it runs.
        let snapshot: Vec<Arc<WorkerInstance>> = registry().clone();
        #[cfg(test)]
        LOOPS.fetch_add(1, Ordering::Relaxed);
        if snapshot.is_empty() {
            // `register` pushes before it unparks, so a registration that
            // raced this snapshot left the token and `park` returns at once.
            std::thread::park();
            continue;
        }
        let mut did_work = false;
        for instance in snapshot {
            let gate = instance
                .gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if instance.removed.load(Ordering::Acquire) {
                continue;
            }
            // SAFETY: `interface` points into the module the core keeps
            // loaded until after `unregister`, `handle` belongs to the
            // instance, and the gate plus the single thread make this the
            // only call into `work` for the instance.
            did_work |= unsafe { serve(&instance, &mut request) };
            drop(gate);
        }
        if !did_work {
            std::thread::park_timeout(PARK_TIMEOUT);
        }
    }
}

/// Calls `work` for every request queued for `instance`.  Returns whether
/// there was any.
///
/// # Safety
///
/// The caller holds the instance's gate, and the plugin is neither cleaned
/// up nor its module unloaded.
unsafe fn serve(instance: &WorkerInstance, request: &mut [u8]) -> bool {
    // SAFETY: the caller's contract is the one [`drain_requests`] requires,
    // and the gate makes this the only `work` for the instance.
    unsafe {
        drain_requests(
            instance.handle,
            instance.interface,
            &instance.rings,
            request,
            &instance.failed,
        )
    }
}

/// Calls `work` for every request queued on `rings`, and for any request
/// `work` itself schedules, until the ring is empty.  `respond` queues each
/// response.  A non-success status is logged once while `failed` is clear:
/// one drain does not flood, and the worker thread still logs one line per
/// instance.
///
/// The load calls this on the UI thread before the instance is registered.
/// [`serve`] calls it on the worker thread.  `schedule_work` itself only
/// enqueues, on either path.
///
/// # Safety
///
/// `interface` is the plugin's worker interface, or null.  `handle` is its
/// instance.  No other thread is inside `work`, `run`, `deactivate` or
/// `cleanup` for it, and the module stays loaded.  At load that holds
/// because the instance is not registered and the processor does not exist.
/// [`serve`] holds the instance gate.
pub(crate) unsafe fn drain_requests(
    handle: LV2_Handle,
    interface: *const LV2_Worker_Interface,
    rings: &Arc<WorkerRings>,
    request: &mut [u8],
    failed: &AtomicBool,
) -> bool {
    let Some(work) = (unsafe { interface.as_ref() }).and_then(|interface| interface.work) else {
        return false;
    };
    let mut served = false;
    while rings.request_pending() {
        let Some(size) = rings.pop_request(request) else {
            break;
        };
        served = true;
        // SAFETY: the caller's contract; `size` bytes of the request are live
        // in the buffer for the call.  `work` may schedule another request,
        // which the loop picks up.
        let status = unsafe {
            work(
                handle,
                Some(respond),
                Arc::as_ptr(rings) as *mut c_void,
                size as u32,
                request.as_ptr() as *const c_void,
            )
        };
        if status != LV2_WORKER_SUCCESS && !failed.swap(true, Ordering::AcqRel) {
            eprintln!("[plugins] an LV2 worker request failed with status {status}");
        }
    }
    served
}

/// The `respond` a `work` call gets: one response into the instance's
/// response ring.  The processor drains it before the next `run`, and the
/// load's zero-frame run drains what a restore's `work` queued.  Called from
/// `work`, which at load is the UI thread and afterwards the worker thread.
pub(super) unsafe extern "C" fn respond(
    handle: LV2_Worker_Respond_Handle,
    size: u32,
    data: *const c_void,
) -> LV2_Worker_Status {
    if data.is_null() && size > 0 {
        return LV2_WORKER_ERR_UNKNOWN;
    }
    // SAFETY: the handle is the `Arc`'s pointer, kept alive by the instance
    // until after `unregister`, which waits for this call to return.
    let Some(rings) = (unsafe { (handle as *const WorkerRings).as_ref() }) else {
        return LV2_WORKER_ERR_UNKNOWN;
    };
    let payload: &[u8] = if data.is_null() {
        &[]
    } else {
        // SAFETY: the plugin promises `size` readable bytes for this call.
        unsafe { std::slice::from_raw_parts(data as *const u8, size as usize) }
    };
    if rings.push_response(payload) {
        LV2_WORKER_SUCCESS
    } else {
        LV2_WORKER_ERR_NO_SPACE
    }
}
