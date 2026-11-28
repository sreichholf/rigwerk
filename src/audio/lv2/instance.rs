//! UI-thread half of a loaded LV2 plugin.
//!
//! [`Lv2Instance`] is behind the UI-only lock.  Both halves point into one
//! [`Lv2Core`].  The last half to go runs `deactivate` (only if `activate`
//! ran) and `cleanup`, then unloads the library.
//!
//! Instantiation-class calls (`instantiate`, `activate`, `deactivate`,
//! `cleanup`, `state.restore`) run on the UI thread, never while a block is
//! inside `run`.  `connect_port` runs here at load, before any `run`.
//! `state.save` may overlap `run`, so a capture needs no pause.  A restore's
//! worker requests run on this thread, before `activate`, because the
//! instance is not registered yet.  After registration, `work` runs only on
//! the worker thread; the core unregisters before `deactivate`/`cleanup`.
//! Unregistering serves queued requests and delivers responses through
//! `work_response`, as `worker.h` requires, with the processor already gone.
//! There is no restart: LV2 has no such call, and a new sample rate loads a
//! new instance.
//!
//! Restored values are written into the control buffers before activation.
//! `run(handle, 0)` after that delivers responses restore's `work` queued,
//! then updates control outputs, without processing audio.  The worker is
//! registered after that run.  A request `run(0)` schedules stays queued;
//! it is not run inline.

use std::ffi::{c_char, c_void, CString};
use std::path::Path;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};

use lv2_sys::{
    LV2_Descriptor, LV2_Handle, LV2_STATE__interface, LV2_State_Interface, LV2_WORKER__interface,
    LV2_Worker_Interface, LV2_Worker_Status,
};

use crate::audio::plugins::host::{
    apply_reported, drain_values, finish_capture, HostIo, PluginHostUi,
};
use crate::audio::plugins::{
    param_transfer, Capture, ParamIds, ParamOut, ParamReceiver, PluginParamView, PluginShared,
    Restore, StateSave,
};

use super::host::HostFeatures;
use super::module::{self, ModuleHandle};
use super::ports::{Lv2Range, PortBuffers, PortTable};
use super::scan;
use super::state;
use super::worker::{self, WorkerInstance, WorkerRings};

/// Fields drop in declaration order: `Drop` deactivates and cleans up,
/// then the features, rings, bundle path and retired buffers, and only then
/// the module unloads the library.
pub(super) struct Lv2Core {
    pub(super) handle: LV2_Handle,
    pub(super) descriptor: *const LV2_Descriptor,
    /// Boxed before the pointers were taken, so a moved handle never moves it.
    pub(super) _features: Box<HostFeatures>,
    /// The schedule feature points at them.
    pub(super) _rings: Arc<WorkerRings>,
    /// A plugin may keep the pointer until after `cleanup`, as lilv does.
    pub(super) _bundle_path: CString,
    /// Port buffers after the processor drops them.  The plugin still points
    /// in, so they are freed only after `cleanup`.
    pub(super) retired: OnceLock<PortBuffers>,
    /// Empty without `worker:interface`, and for test fakes.  Dropped after
    /// unregister.
    pub(super) worker: OnceLock<Arc<WorkerInstance>>,
    /// The load's `activate` has run, or none was needed.  The processor
    /// reads this.
    pub(super) active: AtomicBool,
    /// `activate` was called, so `deactivate` may be.  A plugin without
    /// `activate` is active without this.
    pub(super) activated: AtomicBool,
    /// As `f32` bits.
    pub(super) latency_raw: AtomicU32,
    /// The latency port moved since the UI last read it.
    pub(super) latency_changed: AtomicBool,
    /// Declared last: the library must stay loaded until after `cleanup`.
    /// `None` only for the fakes the tests build without a binary.
    pub(super) _module: Option<ModuleHandle>,
}

// SAFETY: the core only owns the instance and the library it came from.  Each
// half calls the plugin per LV2's thread rules (see the module doc), and the
// last half to go drops the core on the UI thread: slots and handles are
// dropped there, or deferred to it.
unsafe impl Send for Lv2Core {}
unsafe impl Sync for Lv2Core {}

impl Drop for Lv2Core {
    fn drop(&mut self) {
        // The worker thread must not be inside `work` when the plugin is
        // deactivated, cleaned up and its module unloaded.  `unregister`
        // waits out an in-flight call, then serves what is queued and
        // delivers the responses while the plugin is still active; this
        // runs on the UI thread, and the processor is gone.
        if let Some(worker) = self.worker.get() {
            worker::unregister(worker);
        }
        if self.descriptor.is_null() {
            return;
        }
        // SAFETY: the module kept the library loaded, and this is the last
        // reference to the core, so no block is inside `run`.
        unsafe {
            let descriptor = &*self.descriptor;
            if self.activated.load(Ordering::Acquire) {
                if let Some(deactivate) = descriptor.deactivate {
                    deactivate(self.handle);
                }
            }
            if let Some(cleanup) = descriptor.cleanup {
                cleanup(self.handle);
            }
        }
    }
}

#[cfg(test)]
impl Lv2Core {
    pub(super) fn for_test(active: bool) -> Self {
        Self {
            handle: std::ptr::null_mut(),
            descriptor: std::ptr::null(),
            _features: HostFeatures::new(48_000.0, 16, super::ports::ATOM_CAPACITY, None, false),
            _rings: Arc::new(WorkerRings::new()),
            _bundle_path: CString::default(),
            retired: OnceLock::new(),
            worker: OnceLock::new(),
            active: AtomicBool::new(active),
            activated: AtomicBool::new(false),
            latency_raw: AtomicU32::new(0),
            latency_changed: AtomicBool::new(false),
            _module: None,
        }
    }
}

pub(crate) struct Lv2Instance {
    /// The processor's table order.
    pub(super) ranges: Arc<[Lv2Range]>,
    /// Native, one per range.  What the panel and the plugin last agreed.
    pub(super) values: Vec<f32>,
    pub(super) shared: Arc<PluginShared>,
    /// Valid for the instance's life (the descriptor promises that).  Null
    /// when the plugin has none.
    state: *const LV2_State_Interface,
    /// The load's zero-frame run delivers queued responses through it
    /// before `run`.
    worker_response:
        Option<unsafe extern "C" fn(LV2_Handle, u32, *const c_void) -> LV2_Worker_Status>,
    /// Every `run` must be followed by it, including the load's zero-frame
    /// run, which does not go through the processor.
    worker_end_run: Option<unsafe extern "C" fn(LV2_Handle) -> LV2_Worker_Status>,
    /// Last: outlives everything above that points into it.
    pub(super) core: Arc<Lv2Core>,
}

// SAFETY: the instance only makes LV2's UI-thread calls (`activate`, the
// state extension's `save` and `restore`, `deactivate` and `cleanup` through
// the core's drop) and, through `reread_latency_if_changed`, reads atomics.
// It is reached only through the UI-only lock of its `PluginHandle`, one
// thread at a time.
unsafe impl Send for Lv2Instance {}

pub(crate) struct Lv2Loaded {
    pub(crate) instance: Lv2Instance,
    pub(crate) processor: super::processor::Lv2Processor,
    pub(crate) out: Arc<ParamOut>,
    pub(crate) from_audio: ParamReceiver,
    pub(crate) ids: Arc<ParamIds>,
    pub(crate) views: Vec<PluginParamView>,
    pub(crate) has_editor: bool,
    /// The saved blob restored, and no newer value had to go on top.
    pub(crate) state_current: bool,
    /// Restored value ids still queued for the processor.
    pub(crate) undelivered: Vec<u32>,
}

impl Lv2Instance {
    /// UI thread.  The processor starts with the first audio block.
    pub(crate) fn load(
        bundle: &Path,
        plugin_uri: &str,
        sample_rate: u32,
        max_block: usize,
        restore: &Restore<'_>,
        shared: &Arc<PluginShared>,
    ) -> Result<Lv2Loaded, String> {
        let (store, plugins) = scan::read_bundle(bundle)?;
        // `lv2:sampleRate` scaling must use the rate `instantiate` will get,
        // including the 48 kHz fallback for a zero rate.
        let rate = if sample_rate == 0 {
            48_000.0
        } else {
            f64::from(sample_rate)
        };
        let table = PortTable::read_store(&store, &plugins, plugin_uri, rate.round() as u32)?;
        // `read_store` already refused a required `state:loadDefaultState`
        // with no state to load; an optional one with none omits the feature.
        let default_state = state::default_state(&store, plugin_uri);
        let binary = module::native_binary(&store, plugin_uri, bundle)?;
        let module = module::acquire_module(&binary, &trailing_separator(bundle))?;
        let descriptor = module
            .descriptor_for_uri(plugin_uri)
            .ok_or_else(|| format!("The LV2 binary does not contain {plugin_uri}."))?;
        Self::load_descriptor(
            bundle,
            table,
            descriptor,
            Some(module),
            default_state.as_ref(),
            rate,
            max_block,
            restore,
            shared,
        )
    }

    /// UI thread.  `module` is `None` only for test fakes.  The descriptor
    /// must outlive the instance: `Drop` calls `deactivate` and `cleanup`
    /// through the pointer, and `run` is read from it every block.  A fake
    /// must live as long as the instance, not merely this call.
    /// `default_state` is restored when no saved blob was.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn load_descriptor(
        bundle: &Path,
        mut table: PortTable,
        descriptor: *const LV2_Descriptor,
        module: Option<ModuleHandle>,
        default_state: Option<&state::DefaultState>,
        rate: f64,
        max_block: usize,
        restore: &Restore<'_>,
        shared: &Arc<PluginShared>,
    ) -> Result<Lv2Loaded, String> {
        // SAFETY: `descriptor` points into the module's library, which the
        // core keeps loaded until after `cleanup`.
        let (instantiate, run) = unsafe {
            let descriptor = &*descriptor;
            let Some(instantiate) = descriptor.instantiate else {
                return Err("The LV2 plugin cannot be instantiated.".to_owned());
            };
            let Some(run) = descriptor.run else {
                return Err("The LV2 plugin cannot be run.".to_owned());
            };
            if descriptor.connect_port.is_none() {
                return Err("The LV2 plugin has no connect_port.".to_owned());
            }
            (instantiate, run)
        };

        let block = max_block.clamp(1, crate::audio::plugins::MAX_BLOCK);
        let rings = Arc::new(WorkerRings::new());
        let features = HostFeatures::new(
            rate,
            block,
            table.sequence_size(),
            Some(Arc::clone(&rings)),
            default_state.is_some(),
        );
        // The path must end in a separator, so a plugin can append a filename
        // to it.  The core owns it: a plugin may keep the pointer.
        let bundle_path = CString::new(trailing_separator(bundle))
            .map_err(|_| "The LV2 bundle path contains a NUL byte.".to_owned())?;
        // SAFETY: the descriptor, the feature array and the bundle path are
        // valid, and the features outlive the instance in the core.
        let handle =
            unsafe { instantiate(descriptor, rate, bundle_path.as_ptr(), features.as_ptr()) };
        if handle.is_null() {
            return Err("The LV2 plugin could not be instantiated.".to_owned());
        }
        // Valid for the instance's lifetime.
        // SAFETY: `descriptor` points into the module the core keeps loaded,
        // and the URI is the null-terminated one the header names.
        let state_interface = unsafe {
            let extension_data = (*descriptor).extension_data;
            match extension_data {
                Some(extension_data) => {
                    let data = extension_data(LV2_STATE__interface.as_ptr() as *const c_char);
                    data as *const LV2_State_Interface
                }
                None => ptr::null(),
            }
        };
        // Null when the plugin has none.  An interface whose `work` is null
        // has nothing to run.
        // SAFETY: as above, `descriptor` points into the module the core
        // keeps loaded, and the URI is the null-terminated one the header
        // names; a non-null result points at the plugin's interface.
        let worker_interface = unsafe {
            let extension_data = (*descriptor).extension_data;
            match extension_data {
                Some(extension_data) => {
                    let data = extension_data(LV2_WORKER__interface.as_ptr() as *const c_char);
                    data as *const LV2_Worker_Interface
                }
                None => ptr::null(),
            }
        };
        // SAFETY: the interface points into the module the core keeps loaded
        // until after the core has unregistered the instance.
        let has_work = unsafe {
            worker_interface
                .as_ref()
                .is_some_and(|interface| interface.work.is_some())
        };
        // Only a plugin whose requests can reach `work` may schedule.
        // Before the restore, so it can queue.  Drained on this thread
        // before activate.  A request the zero-frame run schedules waits
        // for registration below.
        if has_work {
            rings.accept_requests();
        }
        // From here an early return deactivates and cleans the instance up
        // through the core's drop, then unloads the module.
        let core = Arc::new(Lv2Core {
            handle,
            descriptor,
            _features: features,
            _rings: rings,
            _bundle_path: bundle_path,
            retired: OnceLock::new(),
            worker: OnceLock::new(),
            active: AtomicBool::new(false),
            activated: AtomicBool::new(false),
            latency_raw: AtomicU32::new(0),
            latency_changed: AtomicBool::new(false),
            _module: module,
        });

        let mut buffers = PortBuffers::new(&table, block);
        // `connect_port` is an audio-class call.  It runs here on the UI
        // thread before the processor exists and before any `run`, which the
        // class rules allow: no audio-class call can overlap it.
        buffers.connect(handle, descriptor);

        // Hidden controls included: a fresh plugin sees `lv2:default`, not
        // zero.
        let ranges: Arc<[Lv2Range]> =
            Arc::from(std::mem::take(&mut table.ranges).into_boxed_slice());
        let ids = Arc::clone(&table.ids);
        let mut native: Vec<f32> = ranges.iter().map(|range| range.default as f32).collect();

        // The blob first, then saved controls.  LV2 state holds no control
        // ports, so every saved value still applies, and both are before
        // activation.  A blob the plugin refuses is logged by
        // `state::restore`; the saved parameters still describe the effect.
        let state_loaded = match restore.state.as_deref() {
            Some(bytes) => state::restore(handle, state_interface, bytes, core._features.as_ptr()),
            None => false,
        };
        // The bundle's default stands in when no saved blob loaded.  It is
        // not the saved state, so the load is not current.
        if !state_loaded {
            if let Some(default_state) = default_state {
                state::restore_default(
                    handle,
                    state_interface,
                    default_state,
                    core._features.as_ptr(),
                );
            }
        }
        // Before activate and before registration: no audio thread is inside
        // `run`, and the worker thread does not know this instance.  `work`
        // may schedule another request, so the drain runs until the ring is
        // empty.  Responses wait for `run_zero`.  A request `run(0)`
        // schedules later stays queued.
        if has_work && core._rings.request_pending() {
            let mut request = vec![0u8; worker::RING_BYTES];
            let failed = AtomicBool::new(false);
            // SAFETY: `has_work` means the interface has `work`, the module
            // stays loaded in `core`, and nothing else calls the plugin yet.
            unsafe {
                worker::drain_requests(
                    handle,
                    worker_interface,
                    &core._rings,
                    &mut request,
                    &failed,
                );
            }
        }
        let restored = restore.params(false);
        for (id, value) in &restored {
            let Some(index) = ids.index_of(*id) else {
                continue;
            };
            let range = &ranges[index];
            if range.read_only {
                continue;
            }
            let native_value = value.map_or(range.default, |value| range.denormalise(value));
            buffers.set_control(range.slot, native_value as f32);
            native[index] = native_value as f32;
        }

        let mut instance = Self {
            ranges: Arc::clone(&ranges),
            values: native,
            shared: Arc::clone(shared),
            state: state_interface,
            // SAFETY: the interface points into the module the core keeps
            // loaded until after the instance is gone.
            worker_response: unsafe {
                worker_interface
                    .as_ref()
                    .and_then(|interface| interface.work_response)
            },
            worker_end_run: unsafe {
                worker_interface
                    .as_ref()
                    .and_then(|interface| interface.end_run)
            },
            core: Arc::clone(&core),
        };
        instance.activate();
        instance.run_zero(&mut buffers);
        // SAFETY: the interface points into the module the core keeps loaded,
        // and the callbacks are only used while this instance exists.
        let worker_callbacks = unsafe {
            worker_interface
                .as_ref()
                .map(|interface| super::processor::WorkerCallbacks {
                    rings: Arc::clone(&core._rings),
                    responses: vec![0u8; super::worker::RING_BYTES],
                    work_response: interface.work_response,
                    end_run: interface.end_run,
                })
        };
        // So the views of the new effect are not reported as a change.
        shared.take_latency_change();

        // Read-only controls may have moved.  A control input is never
        // copied back over the panel's value.
        let updated: Vec<(usize, f32)> = ranges
            .iter()
            .enumerate()
            .filter(|(_, range)| range.read_only)
            .map(|(index, range)| (index, buffers.control(range.slot)))
            .collect();
        for (index, value) in updated {
            instance.values[index] = value;
        }
        let views = instance.parameters();

        let (to_audio, from_ui) = param_transfer(ids.len());
        let (to_ui, from_audio) = param_transfer(ids.len());
        let out = Arc::new(ParamOut::new(Arc::clone(&ids), to_audio));
        // An output is reported only when it moves, so the baseline is what
        // `run(0)` left.
        let reported: Vec<u32> = ranges
            .iter()
            .map(|range| buffers.control(range.slot).to_bits())
            .collect();
        let processor = super::processor::Lv2Processor {
            buffers,
            ranges,
            ids: Arc::clone(&ids),
            from_ui,
            to_ui,
            handle,
            run,
            reported,
            max_block: block,
            worker: worker_callbacks,
            core: Arc::clone(&core),
        };
        // After the restore drain and the zero-frame run, so `work` cannot
        // race them.  A request `run(0)` scheduled stays queued.  On failure
        // the buffers go to the core, as on any teardown, and outlive
        // `cleanup`.
        if has_work {
            let registered =
                worker::register(handle, worker_interface, Arc::clone(&core._rings))
                    .map_err(|error| format!("The LV2 worker could not be registered: {error}"))?;
            let _ = core.worker.set(registered);
        }
        Ok(Lv2Loaded {
            has_editor: instance.has_editor(),
            processor,
            out,
            from_audio,
            ids,
            views,
            state_current: state_loaded,
            undelivered: Vec::new(),
            instance,
        })
    }

    /// UI thread, once per frame.  An idle UI may not repaint; then the
    /// park timeout bounds the wait.
    pub(crate) fn service_worker(&self) {
        if let Some(worker) = self.core.worker.get() {
            worker::wake(worker);
        }
    }

    /// Once, at load.  `activate` returns void, so it cannot fail.
    fn activate(&mut self) {
        if self.core.active.load(Ordering::Acquire) {
            return;
        }
        if let Some(descriptor) = unsafe { self.core.descriptor.as_ref() } {
            if let Some(activate) = descriptor.activate {
                // SAFETY: instantiation class, and no block is inside `run`.
                unsafe { activate(self.core.handle) };
                self.core.activated.store(true, Ordering::Release);
            }
        }
        self.core.active.store(true, Ordering::Release);
    }

    /// Zero frames, so control outputs and latency update without audio.
    /// Audio-class, at load, before the processor has the buffers.  Atom
    /// ports are re-armed first.  Responses are delivered before `run`, and
    /// `end_run` follows.
    pub(super) fn run_zero(&self, buffers: &mut PortBuffers) {
        // SAFETY: the descriptor belongs to the module the core keeps loaded;
        // the test fake has a null descriptor, for which this is a no-op.
        let Some(descriptor) = (unsafe { self.core.descriptor.as_ref() }) else {
            return;
        };
        let Some(run) = descriptor.run else {
            return;
        };
        buffers.reset_atoms();
        if self.core._rings.response_pending() {
            // Allocated here, on the UI thread at load.  The processor gets
            // its own buffer afterwards and never allocates one in `run`.
            let mut responses = vec![0u8; worker::RING_BYTES];
            super::processor::deliver_responses(
                self.core.handle,
                &self.core._rings,
                &mut responses,
                self.worker_response,
            );
        }
        // SAFETY: the ports are connected, the instance is active, and no
        // other block is inside the plugin: the processor does not exist yet.
        unsafe { run(self.core.handle, 0) };
        // `end_run` follows every `run`, this zero-frame one included.
        if let Some(end_run) = self.worker_end_run {
            // SAFETY: `end_run` is audio-class, and this is the run context
            // the instance is in.
            unsafe { end_run(self.core.handle) };
        }
        if buffers.latency.is_some() {
            let raw = buffers.latency();
            self.core
                .latency_raw
                .store(raw.to_bits(), Ordering::Release);
            self.shared.set_latency(frames_from(raw));
        }
    }

    /// The port lives in the processor, which publishes
    /// [`Lv2Core::latency_raw`].
    pub(crate) fn reread_latency_if_changed(&mut self) {
        if self.core.latency_changed.swap(false, Ordering::AcqRel) {
            self.shared.set_latency(self.raw_latency());
        }
    }

    fn raw_latency(&self) -> u32 {
        frames_from(f32::from_bits(
            self.core.latency_raw.load(Ordering::Acquire),
        ))
    }

    pub(crate) fn parameters(&self) -> Vec<PluginParamView> {
        self.ranges
            .iter()
            .enumerate()
            .map(|(index, range)| range.view(f64::from(self.values[index])))
            .collect()
    }

    pub(crate) fn default_views(&self) -> Vec<PluginParamView> {
        self.ranges
            .iter()
            .map(|range| range.view(range.default))
            .collect()
    }

    pub(crate) fn default_values(&self) -> Vec<(usize, f64)> {
        self.ranges
            .iter()
            .enumerate()
            .filter(|(_, range)| !range.read_only)
            .map(|(index, range)| (index, range.normalise(range.default)))
            .collect()
    }

    pub(crate) fn display_for(&self, id: u32, value: f64) -> String {
        let Some(range) = self.ranges.iter().find(|range| range.id == id) else {
            return String::new();
        };
        range.display(range.denormalise(value))
    }

    pub(crate) fn note_value(&mut self, index: usize, normalized: f64) {
        if let Some(range) = self.ranges.get(index) {
            self.values[index] = range.denormalise(normalized) as f32;
        }
    }

    pub(crate) fn is_read_only(&self, id: u32) -> bool {
        self.ranges
            .iter()
            .any(|range| range.id == id && range.read_only)
    }

    /// UI thread.  `state.h` lets `save` overlap `run`, so no pause.  No
    /// interface, or a failed save, is [`StateSave::Unavailable`].
    pub(crate) fn save_state(&self) -> StateSave {
        if self.state.is_null() {
            return StateSave::Unavailable;
        }
        match state::save(self.core.handle, self.state, self.core._features.as_ptr()) {
            Ok(bytes) => StateSave::Saved(bytes),
            Err(error) => {
                eprintln!("[plugins] {error}");
                StateSave::Unavailable
            }
        }
    }

    pub(crate) fn has_editor(&self) -> bool {
        false
    }

    /// LV2 reads controls during `run`, so nothing is called from the UI.
    /// A stopped stream cannot flush them here.
    fn send_values(&self, io: &mut HostIo<'_>, values: &[(usize, f64)]) {
        for &(index, value) in values {
            io.out.push(index, value);
        }
        let ids = io.ids;
        io.undelivered.note(
            values.iter().filter_map(|&(index, _)| ids.id(index)),
            io.out.pushed(),
        );
    }
}

impl PluginHostUi for Lv2Instance {
    fn parameter_text(&self, id: u32, value: f64) -> Option<String> {
        Some(self.display_for(id, value))
    }

    fn is_read_only(&self, id: u32) -> bool {
        self.is_read_only(id)
    }

    fn write_parameter(&mut self, io: &mut HostIo<'_>, index: usize, value: f64) {
        self.note_value(index, value);
        self.send_values(io, &[(index, value)]);
    }

    fn read_parameters(&mut self, _io: &mut HostIo<'_>) -> Option<Vec<PluginParamView>> {
        // LV2 has no call that reads a control port (the host's buffer is
        // the value), so the views are always kept as they are.
        None
    }

    fn sync_parameters(&mut self, io: &mut HostIo<'_>) -> Option<Vec<PluginParamView>> {
        // The views are the values, and a queued one is not applied until
        // the next block (or flush), so reading now would show the old one.
        if io.out.has_queued() {
            return None;
        }
        Some(io.views.clone())
    }

    fn reset_parameters(&mut self, io: &mut HostIo<'_>) -> Vec<PluginParamView> {
        let defaults = self.default_values();
        let views = self.default_views();
        for &(index, value) in &defaults {
            self.note_value(index, value);
        }
        self.send_values(io, &defaults);
        views
    }

    fn capture_state(&mut self, io: &mut HostIo<'_>) -> Capture {
        io.undelivered.settle(io.out.applied());
        finish_capture(io, self.save_state(), self.state_holds_params())
    }

    fn service(&mut self, io: &mut HostIo<'_>) -> bool {
        // A worker request made during the last block goes out at once
        // rather than at the thread's safety timeout.
        self.service_worker();
        let reported = drain_values(io.from_audio);
        for &(index, value) in &reported {
            self.note_value(index, value);
        }
        let changed = apply_reported(io.ids, io.views, &reported, |id, value| {
            self.display_for(id, value)
        });
        self.reread_latency_if_changed();
        io.undelivered.settle(io.out.applied());
        changed
    }

    fn state_holds_params(&self) -> bool {
        false
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
impl Lv2Instance {
    pub(super) fn for_test(
        ranges: Arc<[Lv2Range]>,
        values: Vec<f32>,
        shared: Arc<PluginShared>,
        core: Arc<Lv2Core>,
    ) -> Self {
        Self {
            ranges,
            values,
            shared,
            state: ptr::null(),
            worker_response: None,
            worker_end_run: None,
            core,
        }
    }

    pub(super) fn set_state_interface_for_test(&mut self, interface: *const LV2_State_Interface) {
        self.state = interface;
    }

    pub(super) fn set_worker_end_run_for_test(
        &mut self,
        end_run: Option<unsafe extern "C" fn(LV2_Handle) -> LV2_Worker_Status>,
    ) {
        self.worker_end_run = end_run;
    }
}

/// A trailing separator, as `instantiate` requires.
fn trailing_separator(bundle: &Path) -> String {
    let mut path = bundle.to_string_lossy().into_owned();
    while path.ends_with(['/', '\\']) {
        path.pop();
    }
    path.push(std::path::MAIN_SEPARATOR);
    path
}

fn frames_from(raw: f32) -> u32 {
    raw.max(0.0).round() as u32
}
