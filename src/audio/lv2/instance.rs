//! The UI-thread half of a loaded LV2 plugin, and its load sequence.
//!
//! [`Lv2Instance`] lives behind the plugin's UI-only lock (`PluginHandle`):
//! the port ranges, the current values, latency and the activation state.
//! The audio-thread half is `processor.rs`.  Both point into one [`Lv2Core`],
//! which owns the instance handle, its descriptor, the library it came from
//! and the feature set the plugin was instantiated with.  The last of the two
//! halves to go runs `deactivate` (only if `activate` actually ran) and
//! `cleanup`, and only then unloads the library.
//!
//! LV2 splits its C API by threading class.  `instantiate`, `activate`,
//! `deactivate`, `cleanup` and `state.restore` are instantiation-class calls:
//! they run on the UI thread, never while a block is inside `run`.
//! `connect_port`, `run` and the worker's `work_response`/`end_run` are
//! audio-class: `connect_port` is done here at load, before the processor is
//! handed to a slot and before any `run` can have started; `run` is the
//! processor's, and the load's `run(handle, 0)` delivers worker responses
//! first.  `state.save` is a UI call that may overlap `run`, so a capture
//! needs no pause.  Requests a restore queued are run on this thread, before
//! `activate`, because the instance is not registered yet and no audio thread
//! is inside `run`.  After that, the worker's `work` is called on the
//! process-wide worker thread (`worker.rs`) only once this load has
//! registered the instance, and the core unregisters it before
//! `deactivate`/`cleanup`, so no `work` can reach a plugin that is being torn
//! down.  Unregistering serves the requests still queued and delivers the
//! responses still queued through `work_response`, as `worker.h` requires;
//! the processor is gone by then, so no `run` is inside.  There is no
//! restart: LV2 has no call that asks for one, and a new sample rate loads a
//! new instance.
//!
//! The instance's values are native per range.  Restored values are written
//! straight into the control buffers before activation, so the plugin sees
//! them from its first block and the panel shows them; the `run(handle, 0)`
//! after activation delivers the responses that restore's `work` queued,
//! then updates the control outputs and the latency report without processing
//! audio.  The worker is registered after that run.  A request `run(0)`
//! schedules stays queued for the worker thread; it is not run inline.

use std::ffi::{c_char, c_void, CString};
use std::path::Path;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};

use lv2_sys::{
    LV2_Descriptor, LV2_Handle, LV2_STATE__interface, LV2_State_Interface, LV2_WORKER__interface,
    LV2_Worker_Interface, LV2_Worker_Status,
};

use crate::audio::clap::StateSave;
use crate::audio::plugins::{
    param_transfer, ParamIds, ParamOut, ParamReceiver, PluginParamView, PluginShared, Restore,
};

use super::host::HostFeatures;
use super::module::{self, ModuleHandle};
use super::ports::{Lv2Range, PortBuffers, PortTable};
use super::scan;
use super::state;
use super::worker::{self, WorkerInstance, WorkerRings};

/// What both halves of one plugin point into.  Fields drop in declaration
/// order, which is the teardown order LV2 needs: the plugin is deactivated
/// and cleaned up in `Drop`, then the features, the rings, the bundle path
/// and the port buffers it was connected to go, and only then the module
/// (which unloads the library if this was its last user).
pub(super) struct Lv2Core {
    pub(super) handle: LV2_Handle,
    pub(super) descriptor: *const LV2_Descriptor,
    /// The feature array the plugin was instantiated with.  Boxed before the
    /// pointers were taken, so a moved handle never moves it.
    pub(super) _features: Box<HostFeatures>,
    /// The instance's worker rings; the schedule feature points at them.
    pub(super) _rings: Arc<WorkerRings>,
    /// The bundle path `instantiate` was given.  A plugin may keep the
    /// pointer, so it lives until after `cleanup`, as lilv keeps it.
    pub(super) _bundle_path: CString,
    /// The port buffers, once the processor that owned them is gone.  The
    /// plugin still points into them, and the instance may keep the core
    /// for longer, so the processor hands them here and they are freed only
    /// after `cleanup`.
    pub(super) retired: OnceLock<PortBuffers>,
    /// The instance as the worker thread knows it, once the load registered
    /// it.  Empty for a plugin without `worker:interface` and for the test
    /// fakes.  Dropped after `Drop::drop` unregistered it.
    pub(super) worker: OnceLock<Arc<WorkerInstance>>,
    /// Whether processing is allowed: the load's `activate` has run (or none
    /// was needed).  The processor reads this.
    pub(super) active: AtomicBool,
    /// Whether `activate` was actually called, so `deactivate` may be called.
    /// A plugin without an `activate` function is active without this.
    pub(super) activated: AtomicBool,
    /// The latency port's last value, as `f32` bits.
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
    /// A core with no plugin behind it, for the processor tests.
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

/// The UI half.
pub(crate) struct Lv2Instance {
    /// The control ranges in the processor's table order.
    pub(super) ranges: Arc<[Lv2Range]>,
    /// One native value per range, as the panel and the plugin last agreed.
    pub(super) values: Vec<f32>,
    pub(super) shared: Arc<PluginShared>,
    /// The plugin's state extension, valid for as long as the instance (the
    /// descriptor promises that).  Null when the plugin has none.
    state: *const LV2_State_Interface,
    /// The worker interface's `work_response`, when it has one.  The load's
    /// zero-frame run delivers queued responses through it before `run`.
    worker_response:
        Option<unsafe extern "C" fn(LV2_Handle, u32, *const c_void) -> LV2_Worker_Status>,
    /// The worker interface's `end_run`, when it has one.  Every `run` must
    /// be followed by it, including the load's zero-frame run, which does
    /// not go through the processor.
    worker_end_run: Option<unsafe extern "C" fn(LV2_Handle) -> LV2_Worker_Status>,
    /// Last: the handle outlives everything above that points into it.
    pub(super) core: Arc<Lv2Core>,
}

// SAFETY: the instance only makes LV2's UI-thread calls (`activate`, the
// state extension's `save` and `restore`, `deactivate` and `cleanup` through
// the core's drop) and, through `reread_latency_if_changed`, reads atomics.
// It is reached only through the UI-only lock of its `PluginHandle`, one
// thread at a time.
unsafe impl Send for Lv2Instance {}

/// What a load hands to `LoadedPlugin`: the two halves and the UI side's
/// parts of the parameter transfers.  Mirrors [`super::super::clap::ClapLoaded`].
pub(crate) struct Lv2Loaded {
    pub(crate) instance: Lv2Instance,
    pub(crate) processor: super::processor::Lv2Processor,
    pub(crate) out: Arc<ParamOut>,
    pub(crate) from_audio: ParamReceiver,
    pub(crate) ids: Arc<ParamIds>,
    pub(crate) views: Vec<PluginParamView>,
    pub(crate) has_editor: bool,
    /// The saved state restored and no newer value had to go on top.
    pub(crate) state_current: bool,
    /// The ids of the restored values still queued for the processor.
    pub(crate) undelivered: Vec<u32>,
}

impl Lv2Instance {
    /// Parses the bundle, opens its binary and loads one plugin from it, in
    /// the order the LV2 headers require.  UI thread.  Nothing is processed
    /// here: the processor starts with the first audio block.
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
        let default_state = state::default_state(&store, plugin_uri);
        // A required feature this load cannot meet must fail before
        // instantiate.  An optional one with no state just omits the feature.
        if default_state.is_none() && state::requires_load_default(&store, plugin_uri) {
            return Err(
                "The LV2 plugin requires state:loadDefaultState, but its bundle has no state:state to restore."
                    .to_owned(),
            );
        }
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

    /// The load from a resolved descriptor on: instantiate, connect, restore,
    /// the worker requests that restore queued, activate, the zero-frame run
    /// and the worker registration.  UI thread.  `module`
    /// keeps the descriptor's library loaded; `None` only for the fake
    /// descriptors the tests build without a binary.  `default_state` is the
    /// bundle's `state:state` for a plugin that asks for it, restored when no
    /// saved blob was.
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
        // The extension is per plugin, valid for the instance's lifetime.
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
        // The worker extension, resolved the same way.  Null when the plugin
        // has none; an interface whose `work` is null has nothing to run.
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
        // Only a plugin whose requests can reach `work` may schedule.  This is
        // before the restore, so a restore can queue requests.  They are
        // drained on this thread before activate.  A request the zero-frame
        // run schedules waits until the worker is registered below.
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

        // The control input buffers already hold their declared defaults
        // (`PortBuffers::new`), hidden controls included, so a fresh plugin
        // sees `lv2:default` rather than zero.
        let ranges: Arc<[Lv2Range]> =
            Arc::from(std::mem::take(&mut table.ranges).into_boxed_slice());
        let ids = Arc::clone(&table.ids);
        let mut native: Vec<f32> = ranges.iter().map(|range| range.default as f32).collect();

        // The state blob goes in first, then the saved controls on top.  LV2
        // state holds no control ports, so every saved value still applies
        // when the blob loaded: `params` is asked for the full list, not the
        // blob's snapshot rule.  Both are before activation, so no block can
        // see the defaults in between.  A blob the plugin cannot take is
        // logged once by `state::restore`; the saved parameters still
        // describe the effect.
        let state_loaded = match restore.state.as_deref() {
            Some(bytes) => state::restore(handle, state_interface, bytes, core._features.as_ptr()),
            None => false,
        };
        // `state:loadDefaultState`: the bundle's default state stands in for
        // a saved one, also when the saved blob did not load.  It is not the
        // saved state, so it does not make the load current.
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
        // Requests the restores queued are run here, before activate and
        // before the instance is registered, so no audio thread is inside
        // `run` and the worker thread does not know this instance.  `work`
        // may schedule another request, so the drain continues until the
        // ring is empty.  Responses wait for `run_zero`.  A request `run(0)`
        // schedules later is left in the ring.
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
        // The audio half needs the interface's two audio-class functions;
        // either may be absent.
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
        // The first value goes out with the views of the new effect.
        shared.take_latency_change();

        // The zero-frame run may have updated read-only controls (meters,
        // gain reduction).  Control inputs belong to the panel and the saved
        // values; a buffer is never copied back over them.
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
        // The processor reports an output only when its value moves, so the
        // baseline is what the load's zero-frame run left in the buffers.
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
        // The worker is registered only now.  Requests a restore scheduled
        // were drained on this thread, and the zero-frame run is done, so no
        // `work` can race them.  A request `run(0)` scheduled stays in the
        // ring and is served from here on.  A plugin without a `work`
        // function had every request refused.  The processor exists already,
        // so a failure here hands the buffers to the core like any teardown,
        // and they outlive `cleanup`; the requests queued so far go with the
        // instance.
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

    /// Wakes the process-wide worker thread when this instance has a request
    /// waiting, so the request need not wait for the thread's park timeout.
    /// UI thread, once per frame drawn.  An idle UI may not repaint; then
    /// that timeout is what bounds the wait.
    pub(crate) fn service_worker(&self) {
        if let Some(worker) = self.core.worker.get() {
            worker::wake(worker);
        }
    }

    /// `activate`, once, at load.  `activate` returns void, so it cannot
    /// fail.
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

    /// Runs the plugin for zero frames so it updates its control outputs and
    /// latency port.  An audio-class call, made at load before the processor
    /// has the buffers; the atom ports are re-armed first, so the plugin sees
    /// an empty input sequence and a cleared output.  Worker responses a
    /// restore's `work` queued are delivered before `run`, and `end_run`
    /// follows it.
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
        // The worker interface's `end_run` follows every `run`, this
        // zero-frame one included.
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

    /// Reads the latency port's last reported value into [`PluginShared`].
    /// The port itself lives in the processor's buffers; the processor
    /// updates [`Lv2Core::latency_raw`] after every block.
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

    /// The control views, from the values the panel and the plugin last
    /// agreed on.
    pub(crate) fn parameters(&self) -> Vec<PluginParamView> {
        self.ranges
            .iter()
            .enumerate()
            .map(|(index, range)| range.view(f64::from(self.values[index])))
            .collect()
    }

    /// The views every control has at its declared default.
    pub(crate) fn default_views(&self) -> Vec<PluginParamView> {
        self.ranges
            .iter()
            .map(|range| range.view(range.default))
            .collect()
    }

    /// The writable controls' defaults, `(index, normalized)` in the
    /// processor's table.
    pub(crate) fn default_values(&self) -> Vec<(usize, f64)> {
        self.ranges
            .iter()
            .enumerate()
            .filter(|(_, range)| !range.read_only)
            .map(|(index, range)| (index, range.normalise(range.default)))
            .collect()
    }

    /// The plugin's text for a normalized value of `id`.
    pub(crate) fn display_for(&self, id: u32, value: f64) -> String {
        let Some(range) = self.ranges.iter().find(|range| range.id == id) else {
            return String::new();
        };
        range.display(range.denormalise(value))
    }

    /// Records a normalized value for the control at `index`: one the panel
    /// set (or reset), or one the audio side reported.
    pub(crate) fn note_value(&mut self, index: usize, normalized: f64) {
        if let Some(range) = self.ranges.get(index) {
            self.values[index] = range.denormalise(normalized) as f32;
        }
    }

    /// Whether `id` is a control the plugin reports, which the host never
    /// sets.
    pub(crate) fn is_read_only(&self, id: u32) -> bool {
        self.ranges
            .iter()
            .any(|range| range.id == id && range.read_only)
    }

    /// Captures the plugin's state into the blob the preferences store.  UI
    /// thread; `state.h` lets `save` overlap `run`, so no pause is needed.  A
    /// plugin without a state interface saves nothing, and so does a failed
    /// save: [`StateSave::Unavailable`] keeps its parameters as the only
    /// description.
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
}

#[cfg(test)]
impl Lv2Instance {
    /// An instance with no plugin behind it, for the handle-level tests.
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

    /// Points the instance at a fake state extension, for the tests that
    /// exercise [`Lv2Instance::save_state`] without a binary.
    pub(super) fn set_state_interface_for_test(&mut self, interface: *const LV2_State_Interface) {
        self.state = interface;
    }

    /// Gives the instance a fake worker `end_run`, for the zero-frame run
    /// test.
    pub(super) fn set_worker_end_run_for_test(
        &mut self,
        end_run: Option<unsafe extern "C" fn(LV2_Handle) -> LV2_Worker_Status>,
    ) {
        self.worker_end_run = end_run;
    }
}

/// The bundle path with a trailing directory separator, as `instantiate`
/// requires.
fn trailing_separator(bundle: &Path) -> String {
    let mut path = bundle.to_string_lossy().into_owned();
    while path.ends_with(['/', '\\']) {
        path.pop();
    }
    path.push(std::path::MAIN_SEPARATOR);
    path
}

/// A latency report as whole frames, never negative.
fn frames_from(raw: f32) -> u32 {
    raw.max(0.0).round() as u32
}
