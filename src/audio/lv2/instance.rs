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
//! processor's.  `state.save` is a UI call that may overlap `run`, so a
//! capture needs no pause.  There is no restart: LV2 has no call that asks
//! for one, and a new sample rate loads a new instance.
//!
//! The instance's values are native per range.  Restored values are written
//! straight into the control buffers before activation, so the plugin sees
//! them from its first block and the panel shows them; the `run(handle, 0)`
//! after activation updates the control outputs and the latency report
//! without processing audio.

use std::ffi::{c_char, CString};
use std::path::Path;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use lv2_sys::{LV2_Descriptor, LV2_Handle, LV2_STATE__interface, LV2_State_Interface};

use crate::audio::clap::StateSave;
use crate::audio::plugins::{
    param_transfer, ParamIds, ParamOut, ParamReceiver, PluginParamView, PluginShared, Restore,
};

use super::host::HostFeatures;
use super::module::{self, ModuleHandle};
use super::ports::{Lv2Range, PortBuffers, PortTable};
use super::scan;
use super::state;
use super::worker::WorkerRings;

/// What both halves of one plugin point into.  Fields drop in declaration
/// order, which is the teardown order LV2 needs: the plugin is deactivated
/// and cleaned up in `Drop`, then the features, the rings and the bundle path
/// go, and only then the module (which unloads the library if this was its
/// last user).
pub(super) struct Lv2Core {
    pub(super) handle: LV2_Handle,
    pub(super) descriptor: *const LV2_Descriptor,
    /// The feature array the plugin was instantiated with.  Boxed before the
    /// pointers were taken, so a moved handle never moves it.
    pub(super) _features: Box<HostFeatures>,
    /// The instance's worker rings; the schedule feature points at them.
    /// The worker thread is wired up in commit 5.
    pub(super) _rings: Arc<WorkerRings>,
    /// The bundle path `instantiate` was given.  A plugin may keep the
    /// pointer, so it lives until after `cleanup`, as lilv keeps it.
    pub(super) _bundle_path: CString,
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
            _features: HostFeatures::new(48_000.0, 16, super::ports::ATOM_CAPACITY, None),
            _rings: Arc::new(WorkerRings::new()),
            _bundle_path: CString::default(),
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
            rate,
            max_block,
            restore,
            shared,
        )
    }

    /// The load from a resolved descriptor on: instantiate, connect, restore,
    /// activate, the zero-frame run and the worker.  UI thread.  `module`
    /// keeps the descriptor's library loaded; `None` only for the fake
    /// descriptors the tests build without a binary.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn load_descriptor(
        bundle: &Path,
        mut table: PortTable,
        descriptor: *const LV2_Descriptor,
        module: Option<ModuleHandle>,
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
        let features =
            HostFeatures::new(rate, block, table.sequence_size(), Some(Arc::clone(&rings)));
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
        // From here an early return deactivates and cleans the instance up
        // through the core's drop, then unloads the module.
        let core = Arc::new(Lv2Core {
            handle,
            descriptor,
            _features: features,
            _rings: rings,
            _bundle_path: bundle_path,
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
            core: Arc::clone(&core),
        };
        instance.activate();
        instance.run_zero(&mut buffers);
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
            core: Arc::clone(&core),
        };
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
    /// an empty input sequence and a cleared output.
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
        // SAFETY: the ports are connected, the instance is active, and no
        // other block is inside the plugin: the processor does not exist yet.
        unsafe { run(self.core.handle, 0) };
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

    /// The native value of a normalized one at `index` of the table.
    #[allow(dead_code)] // For the direct control write from a pause.
    pub(crate) fn native(&self, index: usize, value: f64) -> Option<(u32, f64)> {
        let range = self.ranges.get(index)?;
        Some((range.id, range.denormalise(value)))
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
            core,
        }
    }

    /// Points the instance at a fake state extension, for the tests that
    /// exercise [`Lv2Instance::save_state`] without a binary.
    pub(super) fn set_state_interface_for_test(&mut self, interface: *const LV2_State_Interface) {
        self.state = interface;
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
