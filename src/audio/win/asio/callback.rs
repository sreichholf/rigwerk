//! `azo`'s callbacks carry no user data, so the driver reaches one
//! process-global owner (`ASIO_HOST`): at most one session per role, plus
//! the process-wide reset count.  A reset reaches only the registration
//! that was installed when it arrived, and the message path touches that
//! count only, never a context another thread may free.
//!
//! A buffer index is valid only until the next `bufferSwitch`, so sample
//! work stays in the callback.  The bridge thread only watches atomics.
//!
//! `outputReady` is never called.  It must follow the fill, which only the
//! callback thread can guarantee; azo routes every driver call through its
//! host window, so calling it here would block the audio thread, and
//! calling it later can mark a buffer ready after playback started.

use std::ffi::{c_long, c_void};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

use azo::sys::SampleType;

use crate::audio::input::InputStage;
use crate::audio::mixer::OutputMixer;
use crate::audio::record::{push_device_frame, RecordSource};
use crate::audio::rt_guard::RtGuard;
use crate::audio::shared::Shared;

use super::samples::{read_asio_sample, write_asio_sample};

/// The callback only touches these atomics; the bridge thread uses them
/// for the watchdog.
pub(crate) struct AsioSignal {
    thread: Thread,
    callbacks: AtomicU64,
    /// A panicked callback (a plugin, most likely).  The context is left
    /// alone and the bridge ends with an error.
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

/// Installed before `driver.start()` and reclaimed only after `stop()`
/// and `dispose_buffers()`.
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

    /// A panic must not unwind into the driver (that aborts).  The output
    /// buffers are silenced and the context is never processed again.
    fn dispatch(&mut self, buffer_index: c_long) {
        // Every path counts, including silence and a caught panic.  Nothing
        // here may allocate (see `audio::rt_guard`).
        let _no_allocations = RtGuard::enter();
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

    /// Driver callback thread: no allocation and no blocking.
    fn process(&mut self, buffer_index: c_long) {
        // FTZ for input gain and the output chain; the driver's code gets
        // its mode back afterwards.  The allocation guard is `dispatch`'s.
        // Input tests call `process_input` directly and run outside it.
        let _denormals = crate::audio::denormal::DenormalGuard::new();
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
            // The mixer maps onto `buffers.len()` channels; this only
            // converts each buffer.  One output plays the L/R average
            // (the mono rule, as on cpal) instead of L alone.
            mixer.render(&self.shared, self.frames, |frame, mapped| {
                let channels = buffers.len();
                if channels > 0 {
                    push_device_frame(&self.shared.recorder, RecordSource::Clock, mapped, channels);
                }
                for (channel, buffer) in buffers.iter().enumerate() {
                    let value = mapped.sample(channel);
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
        let mut stage = InputStage::new(&self.shared);
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

        self.shared.note_input_frames(self.frames);
        for frame in 0..self.frames {
            let raw_left = unsafe { read_asio_sample(left_ptr, frame, left_type) };
            let raw_right = match right {
                Some((pointer, sample_type)) => unsafe {
                    read_asio_sample(pointer, frame, sample_type)
                },
                None => raw_left,
            };
            let (left, right) = stage.frame(&self.shared, raw_left, raw_right);
            self.shared.push_frame(left, right);
        }
        stage.publish(&self.shared);
    }
}

/// Separate because an ASIO output fed by another ASIO input uses two
/// drivers.
#[derive(Clone, Copy)]
pub(crate) enum AsioCallbackSlot {
    Input,
    Output,
}

/// At most one session per role.  Callbacks have no user data, so they
/// reach a session only through this table.
pub(crate) struct AsioHost {
    input: AtomicPtr<AsioCallbackContext>,
    output: AtomicPtr<AsioCallbackContext>,
    /// Only the count is read or written, never a context, so the driver
    /// thread can raise a reset safely.
    resets: AtomicU64,
}

impl AsioHost {
    pub(crate) const fn new() -> Self {
        Self {
            input: AtomicPtr::new(ptr::null_mut()),
            output: AtomicPtr::new(ptr::null_mut()),
            resets: AtomicU64::new(0),
        }
    }

    fn slot(&self, slot: AsioCallbackSlot) -> &AtomicPtr<AsioCallbackContext> {
        match slot {
            AsioCallbackSlot::Input => &self.input,
            AsioCallbackSlot::Output => &self.output,
        }
    }

    /// One atomic add, from the driver thread: no allocation, no lock,
    /// and no context pointer.
    pub(crate) fn request_reset(&self) {
        self.resets.fetch_add(1, Ordering::SeqCst);
    }

    fn reset_count(&self) -> u64 {
        self.resets.load(Ordering::SeqCst)
    }
}

pub(crate) static ASIO_HOST: AsioHost = AsioHost::new();

/// The box stays alive until [`CallbackRegistration::clear`], after the
/// driver has stopped.
pub(crate) struct CallbackRegistration {
    host: &'static AsioHost,
    slot: AsioCallbackSlot,
    pointer: *mut AsioCallbackContext,
    signal: Arc<AsioSignal>,
    /// A reset that arrived earlier belongs to whoever held the slot then.
    resets_seen: AtomicU64,
}

impl CallbackRegistration {
    pub(crate) fn install(
        host: &'static AsioHost,
        slot: AsioCallbackSlot,
        context: Box<AsioCallbackContext>,
        signal: Arc<AsioSignal>,
    ) -> Result<Self, String> {
        // Read before the CAS, so a reset raised during install stays
        // pending for the session that takes the slot.
        let resets_seen = host.reset_count();
        let pointer = Box::into_raw(context);
        match host.slot(slot).compare_exchange(
            ptr::null_mut(),
            pointer,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Ok(Self {
                host,
                slot,
                pointer,
                signal,
                resets_seen: AtomicU64::new(resets_seen),
            }),
            Err(_) => {
                // Another session still owns this slot.  Its context stays;
                // this one was never published.
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

    /// Consumes the reset, so one bump ends one wait.  Only the bridge's
    /// waiter calls it, so this is a per-session latch, not a queue.
    pub(crate) fn take_reset(&self) -> bool {
        let now = self.host.reset_count();
        if now == self.resets_seen.load(Ordering::SeqCst) {
            return false;
        }
        self.resets_seen.store(now, Ordering::SeqCst);
        true
    }

    /// The driver stopped calling back.  A callback stuck in `process`
    /// may still use the context, so it is leaked rather than freed.
    pub(crate) fn abandon(self) {
        drop(self);
    }

    /// Only after `stop()` and `dispose_buffers()` have guaranteed no
    /// more callbacks.
    pub(crate) fn clear(mut self) {
        let pointer = std::mem::replace(&mut self.pointer, ptr::null_mut());
        if pointer.is_null() {
            return;
        }
        if self
            .host
            .slot(self.slot)
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
        // A panic may unwind past the explicit teardown.  Detach the slot
        // so a later bridge can start, but never free a context a callback
        // might still be using.
        if !self.pointer.is_null() {
            let _ = self.host.slot(self.slot).compare_exchange(
                self.pointer,
                ptr::null_mut(),
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
    }
}

pub(crate) enum CallbackWait {
    Stopped,
    /// No callback for two seconds: unplugged, or stuck inside a callback.
    /// The context must be abandoned, not freed.
    Hung,
    /// Rate or buffer size changed.
    Reset,
    /// A callback panicked.  The context is idle and may be freed.
    Failed,
}

/// The watchdog checks every 80 ms; a failed callback wakes it at once.
/// When `tick` returns `true` it blocked inside the driver's panel, and
/// that wait is not a missing callback.
pub(crate) fn wait_for_callbacks(
    registration: &CallbackRegistration,
    mut stopped: impl FnMut() -> bool,
    mut tick: impl FnMut() -> bool,
) -> CallbackWait {
    let signal = registration.signal();
    let mut last_count = 0;
    let mut last_callback = Instant::now();
    loop {
        let blocked = tick();
        if stopped() {
            return CallbackWait::Stopped;
        }
        if signal.failed.load(Ordering::Relaxed) {
            return CallbackWait::Failed;
        }
        // A reset raised before this wait, even in the install-to-wait gap,
        // ends this wait exactly once.
        if registration.take_reset() {
            return CallbackWait::Reset;
        }
        let count = signal.callbacks.load(Ordering::Relaxed);
        if count != last_count {
            last_count = count;
            last_callback = Instant::now();
        }
        if blocked {
            // The panel held this thread.  Silence then is not an unplugged
            // interface.
            last_callback = Instant::now();
        }
        if last_callback.elapsed() >= Duration::from_secs(2) {
            return CallbackWait::Hung;
        }
        thread::park_timeout(Duration::from_millis(80));
    }
}

/// The load is acquire.  The context is cleared only after the driver
/// stopped, so a non-null pointer is valid.
fn dispatch(slot: &AtomicPtr<AsioCallbackContext>, buffer_index: c_long) {
    let pointer = slot.load(Ordering::Acquire);
    if pointer.is_null() {
        return;
    }
    // SAFETY: the registration owns this box until `dispose_buffers` returned.
    unsafe { (*pointer).dispatch(buffer_index) };
}

pub(crate) fn dispatch_input(buffer_index: c_long) {
    dispatch(ASIO_HOST.slot(AsioCallbackSlot::Input), buffer_index);
}

pub(crate) fn dispatch_output(buffer_index: c_long) {
    dispatch(ASIO_HOST.slot(AsioCallbackSlot::Output), buffer_index);
}

#[cfg(test)]
mod tests;
