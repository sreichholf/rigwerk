//! Worker rings and the one process-wide worker thread.
//!
//! Each instance has two preallocated SPSC rings: request is audio to
//! worker, response is worker to audio.  The audio side may not lock or
//! allocate.  A message that does not fit whole is refused.
//!
//! `rigwerk-lv2-worker` snapshots the registry under the lock, drops the
//! lock, then takes each instance's gate.  The UI unparks it when a request
//! is waiting.  An idle UI may not repaint, so while any instance is
//! registered the thread parks for at most [`PARK_TIMEOUT`].  With none
//! registered it parks without a timeout; [`register`] unparks it.
//!
//! `work` runs on that thread, one call per instance, never concurrently
//! with `deactivate`/`cleanup` after [`unregister`].  The exception is load:
//! [`drain_requests`] runs on the UI thread before registration and before
//! any `run`.  [`unregister`] marks the instance removed, drops it from the
//! registry, then waits on its gate.  It finishes the exchange before
//! `deactivate`: `worker.h` says an accepted request must reach `work` and
//! an accepted response must reach `work_response`.  `work_response` is
//! audio-class, not audio-thread; the processor is gone, so no `run` is
//! inside.  `end_run` is not called: `worker.h` ties it to a `run` cycle.
//! `work` may lock, allocate and read files.
//!
//! The thread is never shut down.  Its handle is set once, so a thread that
//! exited could not be restarted.
//!
//! Before registration the UI thread writes and reads the response ring.
//! After [`unregister`] it is both producer and consumer.  Those writers
//! never overlap the worker thread.

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

/// Compile-time, so one pop buffer allocated at load is enough.
pub(crate) const RING_BYTES: usize = 64 * 1024;

const LENGTH_BYTES: usize = 4;

struct MessageRing {
    /// The hot path never reads this: a reference to the whole buffer would
    /// alias the producer's and consumer's disjoint spans.
    #[allow(dead_code)]
    buffer: UnsafeCell<Box<[u8]>>,
    /// Fixed in [`MessageRing::new`].  Each side has its own index, so no
    /// byte needs an atomic.
    storage: *mut u8,
    /// The producer is the only writer.
    head: AtomicUsize,
    /// The consumer is the only writer.
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
        // Writes through it carry write permission for the allocation.  The
        // buffer is never moved afterwards.
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

    /// Refuses a message that does not fit whole.  Producer thread only.
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

    /// `out` must be at least [`RING_BYTES`], allocated once at load.
    /// Consumer thread only.
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

/// # Safety
///
/// `start` is below the ring length; the caller owns the region.
unsafe fn write_wrapped(storage: *mut u8, start: usize, data: &[u8]) {
    let first = (RING_BYTES - start).min(data.len());
    // SAFETY: the two spans are inside the ring and do not overlap each
    // other or the source buffer, which the caller guarantees.
    unsafe {
        ptr::copy_nonoverlapping(data.as_ptr(), storage.add(start), first);
        ptr::copy_nonoverlapping(data.as_ptr().add(first), storage, data.len() - first);
    }
}

/// # Safety
///
/// `start` is below the ring length.  `out.len()` does not exceed the
/// published region.
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

/// `HostFeatures` keeps the `Arc` alive while the plugin holds the schedule
/// struct that points at it.
pub(crate) struct WorkerRings {
    request: MessageRing,
    /// The worker thread writes it after registration.  Before that, and at
    /// teardown, the UI thread does.
    response: MessageRing,
    /// One producer at a time.  `schedule_work` is normally `run`, but a
    /// restore may call it on another thread.  A busy gate is no space.
    scheduling: AtomicBool,
    /// Set once the load has found `work`, before restore and the first
    /// `run`.  Until then `schedule_work` refuses.
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

    /// UI thread, at load, once `work` is known.  A restore's requests are
    /// drained here before [`register`].
    pub(crate) fn accept_requests(&self) {
        self.accepting.store(true, Ordering::Release);
    }

    pub(crate) fn request_pending(&self) -> bool {
        self.request.pending()
    }

    pub(crate) fn response_pending(&self) -> bool {
        self.response.pending()
    }

    /// False when the ring is full or another thread is already writing.
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

    /// `out` must be at least [`RING_BYTES`].
    pub(crate) fn pop_request(&self, out: &mut [u8]) -> Option<usize> {
        self.request.pop_into(out)
    }

    pub(crate) fn push_response(&self, data: &[u8]) -> bool {
        self.response.push(data)
    }

    /// `out` must be at least [`RING_BYTES`].
    pub(crate) fn pop_response(&self, out: &mut [u8]) -> Option<usize> {
        self.response.pop_into(out)
    }
}

/// The caller keeps the `Arc` alive while the plugin may use the struct.
pub(crate) fn schedule_struct(rings: &Arc<WorkerRings>) -> LV2_Worker_Schedule {
    LV2_Worker_Schedule {
        handle: Arc::as_ptr(rings) as *mut c_void,
        schedule_work: Some(schedule_work),
    }
}

/// Never runs `work`, never locks or allocates.  Called from `run`, and
/// from a restore or `work` during the load drain (UI thread).  An instance
/// that is not accepting gets `LV2_WORKER_ERR_UNKNOWN`: `worker.h` makes an
/// accepted request a promise that it reaches `work`.
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

/// Bounds a request's wait when the UI draws no frame, and a wake that
/// raced a request.
const PARK_TIMEOUT: Duration = Duration::from_millis(50);

/// `handle` and `interface` are used only while `gate` is held: on the
/// worker thread inside `work` while `removed` is false, and in
/// [`unregister`], which sets `removed` and then waits.  Only then may the
/// caller deactivate, clean up, or unload.
pub(crate) struct WorkerInstance {
    /// Kept by the core until after [`unregister`].
    handle: LV2_Handle,
    /// Points into the module the core keeps loaded.
    interface: *const LV2_Worker_Interface,
    /// Also the handle [`respond`] receives.
    rings: Arc<WorkerRings>,
    /// Set by [`unregister`] before it waits on the gate.  The worker thread
    /// checks it while holding the gate, so no call starts after the wait.
    removed: AtomicBool,
    /// One non-success log line per instance, from the worker thread.
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

static THREAD: OnceLock<std::thread::Thread> = OnceLock::new();

/// Two concurrent loads start one thread.
static STARTING: Mutex<()> = Mutex::new(());

/// Also when a previous holder panicked.
fn registry() -> std::sync::MutexGuard<'static, Vec<Arc<WorkerInstance>>> {
    INSTANCES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

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

/// UI thread, after `activate` and the load's `run(0)`.  The caller passes
/// the `Arc` to [`unregister`] before cleanup.  An interface without `work`
/// is refused: its requests would wait forever.
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

/// UI thread.  Only shortens the wait; a frame the UI does not draw wakes
/// nothing, and then [`PARK_TIMEOUT`] bounds it.
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

/// UI thread, from the core's drop, once the processor is gone and before
/// `deactivate`/`cleanup`.  Once this returns, no `work` is inside or can
/// start.
///
/// Queued messages are finished under the gate.  `worker.h`: an accepted
/// request must reach `work`, an accepted response must reach
/// `work_response`.  `eg-sampler` frees the old sample by scheduling from
/// the response, so dropping either leaks.  `end_run` is not called: it
/// closes a cycle "after every run()", and these rounds follow no `run`.
/// A response may schedule another request; rounds stop at
/// [`TEARDOWN_ROUNDS`], then the rest is dropped and whatever they held
/// leaks.
///
/// The registry lock is not held while waiting.  The UI thread blocks for
/// the in-flight `work` plus the queued messages: a request served after
/// this returned could reach a plugin being cleaned up.
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

/// `eg-sampler`'s exchange takes one.  Past this, a plugin that answers
/// every response with a request is given up on.
pub(super) const TEARDOWN_ROUNDS: usize = 64;

/// # Safety
///
/// The caller holds the instance's gate.  The plugin is active, its module
/// is loaded, and no `run` or other audio-class call can be inside it.
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

#[cfg(test)]
pub(super) fn registered_for_test() -> usize {
    registry().len()
}

#[cfg(test)]
static LOOPS: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
pub(super) fn loops_for_test() -> usize {
    LOOPS.load(Ordering::Relaxed)
}

/// Locks the gate, which keeps `work` out until the guard drops.
#[cfg(test)]
pub(super) fn hold_gate(instance: &WorkerInstance) -> std::sync::MutexGuard<'_, ()> {
    instance
        .gate
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Non-realtime: the plugin may lock, allocate and read files.
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

/// # Safety
///
/// The caller holds the instance's gate.  The plugin is not cleaned up and
/// its module stays loaded.
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

/// The load calls this on the UI thread before registration.  [`serve`]
/// calls it on the worker thread.  `schedule_work` only enqueues.  A
/// non-success status is logged once per drain while `failed` is clear.
///
/// # Safety
///
/// `interface` is the plugin's worker interface, or null.  No other thread
/// is inside `work`, `run`, `deactivate` or `cleanup`, and the module stays
/// loaded.  At load the instance is not registered and the processor does
/// not exist.  [`serve`] holds the gate.
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

/// One response into the instance's ring.  Called from `work`: the UI
/// thread at load, the worker thread after.
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
