//! Everything the ASIO driver callback needs, owned by one bridge and
//! published through an atomic pointer only while that driver runs.
//!
//! ASIO guarantees a buffer index only until the next `bufferSwitch`, so all
//! sample work happens inside the callback.  The bridge thread only watches
//! atomics: a callback counter for the watchdog, a flag for a callback that
//! panicked, and a counter of driver reset requests.
//!
//! `outputReady` is deliberately never called.  It is an optional ASIO
//! optimisation that must follow the fill of the current buffer, which only
//! the callback thread can guarantee; azo routes every driver call through its
//! host window, so calling it from the callback would block the audio thread,
//! and calling it later from another thread can tell the driver a buffer is
//! ready after it already started playing it.  Without it every driver plays
//! the filled buffer at the next switch, which is the ASIO default.

use std::ffi::{c_long, c_void};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

use azo::sys::SampleType;

use crate::audio::mixer::OutputMixer;
use crate::audio::shared::Shared;

use super::samples::{read_asio_sample, write_asio_sample};

/// The bridge-owned half of a callback session.  The callback only touches
/// these atomics; the bridge thread uses them for the watchdog.
pub(crate) struct AsioSignal {
    thread: Thread,
    callbacks: AtomicU64,
    /// Set when a callback panicked (a plugin, most likely).  The context is
    /// then left alone and the bridge ends with an error.
    failed: AtomicBool,
}

impl AsioSignal {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            thread: thread::current(),
            callbacks: AtomicU64::new(0),
            failed: AtomicBool::new(false),
        })
    }
}

/// Bumped by `kAsioResetRequest`.  The driver asks for a full teardown there
/// (its rate or buffer size changed in its control panel), which the bridge
/// reports instead of running on with a stale rate.
static RESET_REQUESTS: AtomicU64 = AtomicU64::new(0);

pub(crate) fn note_driver_reset() {
    RESET_REQUESTS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) const DRIVER_RESET_MESSAGE: &str =
    "The ASIO driver changed its sample rate or buffer size and asked for a restart. Start the bridge again to use the new settings.";
pub(crate) const CALLBACK_PANIC_MESSAGE: &str =
    "The audio callback failed (most likely inside a plugin). The bridge was stopped; remove the plugin or start the bridge again.";

struct InputBuffers {
    buffers: Vec<[*mut c_void; 2]>,
    types: Vec<SampleType>,
}

struct OutputBuffers {
    buffers: Vec<[*mut c_void; 2]>,
    types: Vec<SampleType>,
    mixer: OutputMixer,
}

/// The per-bridge callback context.  It is installed before `driver.start()`
/// and only reclaimed after `driver.stop()` and `driver.dispose_buffers()`.
pub(crate) struct AsioCallbackContext {
    shared: Arc<Shared>,
    signal: Arc<AsioSignal>,
    frames: usize,
    input: Option<InputBuffers>,
    output: Option<OutputBuffers>,
}

impl AsioCallbackContext {
    pub(crate) fn new(shared: Arc<Shared>, signal: Arc<AsioSignal>, frames: usize) -> Self {
        Self {
            shared,
            signal,
            frames,
            input: None,
            output: None,
        }
    }

    pub(crate) fn with_input(
        mut self,
        buffers: Vec<[*mut c_void; 2]>,
        types: Vec<SampleType>,
    ) -> Self {
        self.input = Some(InputBuffers { buffers, types });
        self
    }

    pub(crate) fn with_output(
        mut self,
        buffers: Vec<[*mut c_void; 2]>,
        types: Vec<SampleType>,
        mixer: OutputMixer,
    ) -> Self {
        self.output = Some(OutputBuffers {
            buffers,
            types,
            mixer,
        });
        self
    }

    /// Runs one buffer switch.  A panic must not unwind into the driver
    /// (that aborts the process), so it is caught here: the output buffers
    /// are silenced and the context is never processed again.
    fn dispatch(&mut self, buffer_index: c_long) {
        if self.signal.failed.load(Ordering::Relaxed) {
            self.silence(buffer_index);
            return;
        }
        if catch_unwind(AssertUnwindSafe(|| self.process(buffer_index))).is_err() {
            self.signal.failed.store(true, Ordering::Relaxed);
            self.silence(buffer_index);
            self.signal.thread.unpark();
        }
    }

    fn silence(&self, buffer_index: c_long) {
        let index = buffer_index.clamp(0, 1) as usize;
        if let Some(output) = self.output.as_ref() {
            for (buffer, sample_type) in output.buffers.iter().zip(&output.types) {
                for frame in 0..self.frames {
                    unsafe {
                        write_asio_sample(buffer[index].cast::<u8>(), frame, *sample_type, 0.0);
                    }
                }
            }
        }
    }

    /// Reads input, renders output or does both for one ASIO buffer index.
    /// Runs on the driver's callback thread; it must not allocate or block.
    fn process(&mut self, buffer_index: c_long) {
        self.signal.callbacks.fetch_add(1, Ordering::Relaxed);
        let index = buffer_index.clamp(0, 1) as usize;

        if self.input.is_some() {
            self.process_input(index);
        }

        if let Some(output) = self.output.as_mut() {
            let OutputBuffers {
                buffers,
                types,
                mixer,
            } = output;
            mixer.render(&self.shared, self.frames, |frame, out| {
                for (channel, buffer) in buffers.iter().enumerate() {
                    let value = out.get(channel).copied().unwrap_or(0.0);
                    let sample_type = types
                        .get(channel)
                        .copied()
                        .unwrap_or(SampleType::PCM_F32_LSB);
                    unsafe {
                        write_asio_sample(buffer[index].cast::<u8>(), frame, sample_type, value);
                    }
                }
            });
        }
    }

    fn process_input(&mut self, index: usize) {
        let Some(input) = self.input.as_ref() else {
            return;
        };
        let gain = self.shared.input_gain();
        let enabled_l = self.shared.input_enabled[0].load(Ordering::Relaxed);
        let enabled_r = self.shared.input_enabled[1].load(Ordering::Relaxed);
        let Some(left_ptr) = input
            .buffers
            .first()
            .map(|buffer| buffer[index].cast::<u8>())
        else {
            return;
        };
        let Some(left_type) = input.types.first().copied() else {
            return;
        };
        let right = input
            .buffers
            .get(1)
            .zip(input.types.get(1))
            .map(|(buffer, sample_type)| (buffer[index].cast::<u8>(), *sample_type));

        let mut peak_l = 0.0f32;
        let mut peak_r = 0.0f32;
        self.shared.note_input_frames(self.frames);
        for frame in 0..self.frames {
            let mut left = unsafe { read_asio_sample(left_ptr, frame, left_type) } * gain;
            let mut right = match right {
                Some((pointer, sample_type)) => {
                    (unsafe { read_asio_sample(pointer, frame, sample_type) }) * gain
                }
                None => left,
            };
            if !enabled_l {
                left = 0.0;
            }
            if !enabled_r {
                right = 0.0;
            }
            peak_l = peak_l.max(left.abs());
            peak_r = peak_r.max(right.abs());
            self.shared.push_frame(left, right);
        }
        self.shared.in_l.store(peak_l.to_bits(), Ordering::Relaxed);
        self.shared.in_r.store(peak_r.to_bits(), Ordering::Relaxed);
    }
}

/// The two callback roles a bridge can own.  They are separate because an
/// ASIO output feeding from a different ASIO input uses two drivers.
#[derive(Clone, Copy)]
pub(crate) enum AsioCallbackSlot {
    Input,
    Output,
}

static INPUT_CONTEXT: AtomicPtr<AsioCallbackContext> = AtomicPtr::new(ptr::null_mut());
static OUTPUT_CONTEXT: AtomicPtr<AsioCallbackContext> = AtomicPtr::new(ptr::null_mut());

impl AsioCallbackSlot {
    fn pointer(self) -> &'static AtomicPtr<AsioCallbackContext> {
        match self {
            Self::Input => &INPUT_CONTEXT,
            Self::Output => &OUTPUT_CONTEXT,
        }
    }
}

/// Publishes a context for the driver callbacks.  The box stays alive until
/// [`CallbackRegistration::clear`] runs after the driver has stopped.
pub(crate) struct CallbackRegistration {
    slot: AsioCallbackSlot,
    pointer: *mut AsioCallbackContext,
    signal: Arc<AsioSignal>,
}

impl CallbackRegistration {
    pub(crate) fn install(
        slot: AsioCallbackSlot,
        context: Box<AsioCallbackContext>,
        signal: Arc<AsioSignal>,
    ) -> Result<Self, String> {
        let pointer = Box::into_raw(context);
        match slot.pointer().compare_exchange(
            ptr::null_mut(),
            pointer,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Ok(Self {
                slot,
                pointer,
                signal,
            }),
            Err(_) => {
                // Another session still owns this callback slot.  Its context
                // stays in place; this new one was never published.
                unsafe { drop(Box::from_raw(pointer)) };
                Err(
                    "The previous ASIO session is still shutting down and has not released its driver callbacks."
                        .to_owned(),
                )
            }
        }
    }

    pub(crate) fn signal(&self) -> &AsioSignal {
        &self.signal
    }

    /// Detaches the slot without freeing the context.  Used when the driver
    /// stopped calling back: a callback stuck inside `process` (a hung
    /// plugin, a driver that never returns) may still be using the context,
    /// so it is leaked rather than freed under it.
    pub(crate) fn abandon(self) {
        drop(self);
    }

    /// Clears the slot and reclaims the context.  Call only after
    /// `stop()` and `dispose_buffers()` have guaranteed no more callbacks.
    pub(crate) fn clear(mut self) {
        let pointer = std::mem::replace(&mut self.pointer, ptr::null_mut());
        if pointer.is_null() {
            return;
        }
        if self
            .slot
            .pointer()
            .compare_exchange(
                pointer,
                ptr::null_mut(),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            unsafe { drop(Box::from_raw(pointer)) };
        }
    }
}

impl Drop for CallbackRegistration {
    fn drop(&mut self) {
        // A panic may unwind past the explicit teardown.  Detach the slot so
        // a later bridge can start, but never free a context that a callback
        // might still be using.
        if !self.pointer.is_null() {
            let _ = self.slot.pointer().compare_exchange(
                self.pointer,
                ptr::null_mut(),
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
    }
}

pub(crate) enum CallbackWait {
    /// The bridge was asked to stop.
    Stopped,
    /// No callback for two seconds: unplugged, or stuck inside a callback.
    /// The context must be abandoned, not freed.
    Hung,
    /// The driver asked for a reset (rate or buffer size changed).
    Reset,
    /// A callback panicked; the context is idle and may be freed.
    Failed,
}

/// Waits for the bridge stop flag while watching the driver callback.  The
/// watchdog checks every 80 ms; a failed callback wakes it at once.
pub(crate) fn wait_for_callbacks(
    signal: &AsioSignal,
    mut stopped: impl FnMut() -> bool,
) -> CallbackWait {
    let mut last_count = 0;
    let resets = RESET_REQUESTS.load(Ordering::Relaxed);
    let mut last_callback = Instant::now();
    loop {
        if stopped() {
            return CallbackWait::Stopped;
        }
        if signal.failed.load(Ordering::Relaxed) {
            return CallbackWait::Failed;
        }
        if RESET_REQUESTS.load(Ordering::Relaxed) != resets {
            return CallbackWait::Reset;
        }
        let count = signal.callbacks.load(Ordering::Relaxed);
        if count != last_count {
            last_count = count;
            last_callback = Instant::now();
        }
        if last_callback.elapsed() >= Duration::from_secs(2) {
            return CallbackWait::Hung;
        }
        thread::park_timeout(Duration::from_millis(80));
    }
}

/// Dispatches one input-side buffer switch.  The load is acquire; the context
/// is only cleared after the driver stopped, so a non-null pointer is valid.
pub(crate) fn dispatch_input(buffer_index: c_long) {
    let pointer = INPUT_CONTEXT.load(Ordering::Acquire);
    if pointer.is_null() {
        return;
    }
    // Safety: the registration owns this box until `dispose_buffers` returned.
    unsafe { (*pointer).dispatch(buffer_index) };
}

pub(crate) fn dispatch_output(buffer_index: c_long) {
    let pointer = OUTPUT_CONTEXT.load(Ordering::Acquire);
    if pointer.is_null() {
        return;
    }
    unsafe { (*pointer).dispatch(buffer_index) };
}
