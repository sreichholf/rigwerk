//! The main-thread half of a loaded CLAP plugin, and its load sequence.
//! [`ClapInstance`] lives behind the plugin's UI-only lock (`PluginHandle`):
//! parameters, state, latency, activation, timers, fds and the editor
//! (`gui.rs`).  The audio-thread half is `processor.rs`.
//!
//! Both halves point into one [`ClapCore`], which destroys the plugin when
//! the last of them goes.  The instance deactivates itself on drop, which
//! is the last handle's: processing was stopped by `LoadedPlugin`'s
//! teardown, and the editor is closed first.

use std::ffi::{c_char, c_void, CStr, CString};
use std::path::Path;
use std::ptr;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use clap_sys::ext::audio_ports::{clap_plugin_audio_ports, CLAP_EXT_AUDIO_PORTS};
use clap_sys::ext::gui::{clap_plugin_gui, CLAP_EXT_GUI};
use clap_sys::ext::latency::{clap_plugin_latency, CLAP_EXT_LATENCY};
use clap_sys::ext::params::{
    clap_param_info, clap_plugin_params, CLAP_EXT_PARAMS, CLAP_PARAM_IS_ENUM, CLAP_PARAM_IS_HIDDEN,
    CLAP_PARAM_IS_READONLY, CLAP_PARAM_IS_STEPPED,
};
use clap_sys::ext::posix_fd_support::{clap_plugin_posix_fd_support, CLAP_EXT_POSIX_FD_SUPPORT};
use clap_sys::ext::state::{clap_plugin_state, CLAP_EXT_STATE};
use clap_sys::ext::timer_support::{clap_plugin_timer_support, CLAP_EXT_TIMER_SUPPORT};
use clap_sys::host::clap_host;
use clap_sys::plugin::clap_plugin;
use clap_sys::string_sizes::CLAP_NAME_SIZE;
use clap_sys::version::CLAP_VERSION;

use crate::audio::plugins::{
    param_transfer, ParamIds, ParamOut, ParamReceiver, PluginParamView, PluginShared, Restore,
};

use super::gui::{choose_editor_api, session_candidates, EditorApi};
use super::host::{
    host_get_extension, host_request_callback, host_request_process, host_request_restart,
    AudioThreadScope, HostState, StateSave, HOST_NAME, HOST_URL, HOST_VENDOR, HOST_VERSION,
};
use super::module::{acquire_module, ModuleHandle};
use super::ports::{port_layout, read_audio_ports, PortBuffers};
use super::processor::{stop_processing, ClapProcessor, ClapRange, EventBuffer, OutputSink};
use super::scan::fixed_string;
use super::state::{StateReader, StateWriter, MAX_STATE_BYTES};

/// The largest block the insert chain accepts.  Kept in step with the VST3
/// side: plugins get set up with this size, so it must stay a block size that
/// fixed-buffer effects can handle, and longer bridge blocks are split.
pub(super) const MAX_BLOCK: usize = 512;

/// One CLAP parameter, cached while the plugin is loaded.
#[derive(Clone, Debug)]
pub(super) struct ClapParam {
    pub(super) id: u32,
    pub(super) name: String,
    pub(super) min: f64,
    pub(super) max: f64,
    pub(super) default: f64,
    pub(super) step_count: i32,
    pub(super) read_only: bool,
}

impl ClapParam {
    pub(super) fn range(&self) -> ClapRange {
        ClapRange {
            id: self.id,
            min: self.min,
            max: self.max,
            stepped: self.step_count > 0,
        }
    }
}

/// The plugin pointer `create_plugin` returned.  Dropping it destroys the
/// plugin, exactly once, whether or not `init` ran or succeeded (CLAP
/// allows `destroy` after a failed `init`).  It never deactivates: the
/// owner of the activation does that first.
pub(super) struct PluginPointer(pub(super) *const clap_plugin);

impl Drop for PluginPointer {
    fn drop(&mut self) {
        if let Some(destroy) = unsafe { (*self.0).destroy } {
            unsafe { destroy(self.0) };
        }
    }
}

/// What both halves of one plugin point into.  Fields drop in declaration
/// order, which is the teardown order CLAP needs: destroy the instance, free
/// the host the plugin pointed at, and only then release the shared module
/// (which deinits the entry and unloads the library if this was its last
/// user).
pub(crate) struct ClapCore {
    pub(super) _pointer: PluginPointer,
    pub(super) _host: Box<clap_host>,
    pub(super) host: Arc<HostState>,
    pub(super) _module: ModuleHandle,
}

// SAFETY: the core only owns the pointer; each half calls the plugin per
// CLAP's thread rules (see `ClapInstance` and `ClapProcessor`).  `destroy`
// runs when the last half goes, which is on the UI thread: slots and
// handles are dropped there (or deferred to it).
unsafe impl Send for ClapCore {}
unsafe impl Sync for ClapCore {}

/// The main-thread half.
pub(crate) struct ClapInstance {
    /// `core._pointer`, copied for brevity.
    pub(super) plugin: *const clap_plugin,
    pub(super) params_ext: *const clap_plugin_params,
    pub(super) gui_ext: *const clap_plugin_gui,
    pub(super) timer_ext: *const clap_plugin_timer_support,
    pub(super) state_ext: *const clap_plugin_state,
    pub(super) latency_ext: *const clap_plugin_latency,
    /// `posix-fd-support` is Linux-only.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(super) fd_ext: *const clap_plugin_posix_fd_support,
    /// The parameters as last scanned (the panel's list).
    pub(super) cached_params: Vec<ClapParam>,
    /// The table the processor was built with (index → id and range).  A
    /// rescan that changes it asks for a reload instead.
    pub(super) ranges: Arc<[ClapRange]>,
    pub(super) ids: Arc<ParamIds>,
    pub(super) active: bool,
    pub(super) sample_rate: f64,
    pub(super) max_block: usize,
    pub(super) editor_open: bool,
    /// The window API the editor opens with; `None` without an editor
    /// this session can show.
    pub(super) editor_api: Option<EditorApi>,
    pub(super) shared: Arc<PluginShared>,
    /// Last: the instance outlives everything above that points into it.
    pub(super) core: Arc<ClapCore>,
}

// SAFETY: the instance only makes CLAP main-thread calls, and audio-thread
// ones (a forced `stop_processing`, an active `params.flush`) only while no
// block is inside the plugin; it is reached only through the UI-only lock
// of its `PluginHandle`, one thread at a time.
unsafe impl Send for ClapInstance {}

/// What a load hands to `LoadedPlugin`: the two halves and the UI side's
/// parts of the parameter transfers.
pub(crate) struct ClapLoaded {
    pub(crate) instance: ClapInstance,
    pub(crate) processor: ClapProcessor,
    pub(crate) out: Arc<ParamOut>,
    pub(crate) from_audio: ParamReceiver,
    pub(crate) ids: Arc<ParamIds>,
    pub(crate) views: Vec<PluginParamView>,
    pub(crate) has_editor: bool,
    /// The saved state restored and no newer value had to go on top.
    pub(crate) state_current: bool,
}

impl ClapInstance {
    /// Opens the bundle and loads one plugin from it (see
    /// [`Self::instantiate`]).
    pub(crate) fn load(
        path: &Path,
        plugin_id: &str,
        sample_rate: u32,
        max_block: usize,
        restore: &Restore<'_>,
        shared: &Arc<PluginShared>,
    ) -> Result<ClapLoaded, String> {
        let module = acquire_module(path)?;
        Self::instantiate(module, plugin_id, sample_rate, max_block, restore, shared)
    }

    /// Creates, restores and activates one plugin from an initialised
    /// module, and builds its processor.  Every step may fail; the guards
    /// then tear down whatever was set up, in order, before the module is
    /// released.  Main thread.  Processing is not started: the first audio
    /// block does that (`start_processing` is an audio-thread call).
    pub(super) fn instantiate(
        module: ModuleHandle,
        plugin_id: &str,
        sample_rate: u32,
        max_block: usize,
        restore: &Restore<'_>,
        shared: &Arc<PluginShared>,
    ) -> Result<ClapLoaded, String> {
        let host_state = Arc::new(HostState::new(Arc::clone(shared)));
        let host = Box::new(make_host(&host_state));
        let factory = module.plugin_factory();
        if factory.is_null() {
            return Err("This CLAP bundle has no plugin factory.".to_owned());
        }
        let Some(create) = (unsafe { (*factory).create_plugin }) else {
            return Err("The CLAP factory cannot create plugins.".to_owned());
        };
        let id = CString::new(plugin_id)
            .map_err(|_| "The CLAP plugin id contains a NUL byte.".to_owned())?;
        let plugin = unsafe { create(factory, &*host, id.as_ptr()) };
        if plugin.is_null() {
            return Err("The CLAP plugin could not be created.".to_owned());
        }
        // From here an early return destroys the plugin, then frees the
        // host, then releases the module.
        let core = Arc::new(ClapCore {
            _pointer: PluginPointer(plugin),
            _host: host,
            host: host_state,
            _module: module,
        });
        let Some(init) = (unsafe { (*plugin).init }) else {
            return Err("The CLAP plugin has no init function.".to_owned());
        };
        if !unsafe { init(plugin) } {
            return Err("The CLAP plugin refused to start.".to_owned());
        }

        let audio_ports_ext = extension::<clap_plugin_audio_ports>(plugin, CLAP_EXT_AUDIO_PORTS);
        let rate = if sample_rate == 0 {
            48_000.0
        } else {
            sample_rate as f64
        };
        let block = max_block.clamp(1, MAX_BLOCK);
        let mut instance = Self {
            plugin,
            params_ext: extension(plugin, CLAP_EXT_PARAMS),
            gui_ext: extension(plugin, CLAP_EXT_GUI),
            timer_ext: extension(plugin, CLAP_EXT_TIMER_SUPPORT),
            state_ext: extension(plugin, CLAP_EXT_STATE),
            latency_ext: extension(plugin, CLAP_EXT_LATENCY),
            fd_ext: extension(plugin, CLAP_EXT_POSIX_FD_SUPPORT),
            cached_params: Vec::new(),
            ranges: Arc::from(Vec::new()),
            ids: Arc::new(ParamIds::new(Vec::new())),
            active: false,
            sample_rate: rate,
            max_block: block,
            editor_open: false,
            editor_api: None,
            shared: Arc::clone(shared),
            core,
        };
        instance.editor_api =
            choose_editor_api(instance.plugin, instance.gui_ext, &session_candidates());

        // Restore the saved state and controls while the plugin is still
        // inactive: `state.load` is a main-thread call, and so is
        // `params.flush` until activation, so both take effect before the
        // first audio block and the panel reads the restored values below.
        instance.scan_parameters();
        let state_loaded = restore
            .state
            .as_deref()
            .is_some_and(|bytes| instance.load_state(bytes));
        if state_loaded {
            // The state may bring a different parameter set.
            instance.scan_parameters();
        }
        let ranges: Arc<[ClapRange]> = instance
            .cached_params
            .iter()
            .map(ClapParam::range)
            .collect();
        let ids = Arc::new(ParamIds::new(ranges.iter().map(|range| range.id).collect()));
        instance.ranges = Arc::clone(&ranges);
        instance.ids = Arc::clone(&ids);
        let (to_audio, from_ui) = param_transfer(ids.len());
        let (to_ui, from_audio) = param_transfer(ids.len());
        let out = Arc::new(ParamOut::new(Arc::clone(&ids), to_audio));

        let values = restore.params(state_loaded);
        let queued = instance.restore_parameters(&values, &out);

        // The ports are read last before activation: the restored state may
        // have changed them, and they cannot change while the plugin is
        // active.  A rescan request from here on means the buffers are stale.
        instance
            .core
            .host
            .ports_fixed
            .store(true, Ordering::Release);
        let layout = port_layout(
            read_audio_ports(plugin, audio_ports_ext)?
                .as_ref()
                .map(|(inputs, outputs)| (inputs.as_slice(), outputs.as_slice())),
        )?;
        let inputs = PortBuffers::new(&layout.inputs, layout.main_input, block);
        let outputs = PortBuffers::new(&layout.outputs, layout.main_output, block);
        instance.activate()?;
        instance.query_latency();
        // The first value goes out with the views of the new insert.
        shared.take_latency_change();

        let mut views = instance.parameters();
        // A value queued for the first block is what the plugin has from
        // then on, so the panel shows it rather than the old one.
        for (index, value) in queued {
            if let Some(view) = views.get_mut(index) {
                view.value = value;
                view.display = instance.display_for(view.id, value);
            }
        }
        let events = EventBuffer::with_capacity(from_ui.capacity() + from_ui.params());
        let processor = ClapProcessor {
            plugin,
            params_ext: instance.params_ext,
            ranges,
            ids: Arc::clone(&ids),
            from_ui,
            to_ui,
            inputs,
            outputs,
            events,
            steady_time: 0,
            max_block: block,
            core: Arc::clone(&instance.core),
        };
        Ok(ClapLoaded {
            has_editor: instance.has_editor(),
            processor,
            out,
            from_audio,
            ids,
            views,
            state_current: state_loaded && values.is_empty(),
            instance,
        })
    }

    /// The callback state the editor window follows.
    pub(crate) fn host(&self) -> Arc<HostState> {
        Arc::clone(&self.core.host)
    }

    /// Hands saved state bytes to `clap_plugin_state.load`.
    pub(super) fn load_state(&mut self, bytes: &[u8]) -> bool {
        let Some(load) = (unsafe { self.state_ext.as_ref() }).and_then(|state| state.load) else {
            return false;
        };
        let mut reader = StateReader::new(bytes);
        let stream = reader.stream();
        unsafe { load(self.plugin, &stream) }
    }

    /// Applies saved normalized values (`None` meaning the default) before
    /// activation, through `params.flush` on the main thread.  A plugin
    /// without `flush` gets them through `out` with the first block; those
    /// are returned as `(index, normalized)` for the views.
    fn restore_parameters(
        &mut self,
        values: &[(u32, Option<f64>)],
        out: &ParamOut,
    ) -> Vec<(usize, f64)> {
        let mut native = Vec::new();
        let mut normalized = Vec::new();
        for (id, value) in values {
            let Some(index) = self.ids.index_of(*id) else {
                continue;
            };
            let param = &self.cached_params[index];
            if param.read_only {
                continue;
            }
            let range = param.range();
            let value = value.unwrap_or_else(|| range.normalise(param.default));
            native.push((*id, range.denormalise(value)));
            normalized.push((index, value));
        }
        if native.is_empty() {
            return Vec::new();
        }
        if self.can_flush() {
            self.flush_values(&native);
            return Vec::new();
        }
        for (index, value) in &normalized {
            out.push(*index, *value);
        }
        normalized
    }

    pub(super) fn activate(&mut self) -> Result<(), String> {
        let Some(activate) = (unsafe { (*self.plugin).activate }) else {
            return Err("The CLAP plugin cannot be activated.".to_owned());
        };
        if !unsafe { activate(self.plugin, self.sample_rate, 1, self.max_block as u32) } {
            return Err("The CLAP plugin refused to activate.".to_owned());
        }
        self.active = true;
        Ok(())
    }

    /// Whether the plugin is activated (a failed reactivation leaves it
    /// inactive for good).
    pub(crate) fn is_active(&self) -> bool {
        self.active
    }

    /// `deactivate`, if active.  Main thread, with processing stopped.
    pub(crate) fn deactivate(&mut self) {
        if !self.active {
            return;
        }
        if let Some(deactivate) = unsafe { (*self.plugin).deactivate } {
            unsafe { deactivate(self.plugin) };
        }
        self.active = false;
    }

    /// Reads `clap_plugin_latency.get` into [`PluginShared`].  CLAP allows
    /// the call on the main thread while the plugin is active; an inactive
    /// plugin (or one without the extension) reports 0.
    pub(super) fn query_latency(&mut self) {
        // Cleared first, so a change reported during the call is read again.
        self.core
            .host
            .latency_changed
            .store(false, Ordering::Release);
        let latency = if self.active {
            unsafe { self.latency_ext.as_ref() }
                .and_then(|latency| latency.get)
                .map_or(0, |get| unsafe { get(self.plugin) })
        } else {
            0
        };
        self.shared.set_latency(latency);
    }

    /// Deactivates and reactivates the plugin for `request_restart`, then
    /// reads its latency again.  Main thread, with the plugin paused (no
    /// block inside, processing stopped).  False when the reactivation
    /// failed: the plugin is then inactive and must not be resumed.
    pub(crate) fn restart(&mut self) -> bool {
        self.deactivate();
        // The plugin may change its latency (and call `latency.changed`)
        // inside `activate`; it is read once activation is done.
        let activated = self.activate().is_ok();
        self.query_latency();
        activated
    }

    /// `stop_processing` for a forced pause or a teardown, as the audio
    /// thread: the caller knows no block is inside the plugin.
    pub(crate) fn stop_processing(&mut self) {
        stop_processing(self.plugin);
    }

    /// Whether the plugin has `params.flush`.
    pub(crate) fn can_flush(&self) -> bool {
        unsafe { self.params_ext.as_ref() }.is_some_and(|params| params.flush.is_some())
    }

    /// Hands `(id, native)` values to `params.flush` and returns the output
    /// values it reported, `(index, normalized)`.  CLAP makes `flush` a
    /// main-thread call while inactive and an audio-thread one while
    /// active, never concurrent with `process`: so an active plugin must be
    /// paused with no block inside, and the call is made as its audio
    /// thread.
    pub(crate) fn flush_values(&mut self, values: &[(u32, f64)]) -> Vec<(usize, f64)> {
        let Some(flush) = (unsafe { self.params_ext.as_ref() }).and_then(|params| params.flush)
        else {
            return Vec::new();
        };
        let mut events = EventBuffer::with_capacity(values.len());
        for (id, value) in values {
            events.push_value(*id, *value, 0);
        }
        let mut reported = Vec::new();
        let mut emit = |index: usize, value: f64| reported.push((index, value));
        let mut sink = OutputSink {
            ids: &self.ids,
            ranges: &self.ranges,
            emit: &mut emit,
        };
        let input = events.as_clap();
        let output = sink.as_clap();
        let _audio_thread = self.active.then(AudioThreadScope::enter);
        unsafe { flush(self.plugin, &input, &output) };
        drop(_audio_thread);
        reported
    }

    /// The plugin's main-thread callbacks: `on_main_thread` when asked
    /// for, due timers and ready file descriptors.  CLAP allows these while
    /// the audio thread processes.
    pub(crate) fn service_callbacks(&mut self) {
        let host = Arc::clone(&self.core.host);
        if host.callback_requested.swap(false, Ordering::Relaxed) {
            if let Some(on_main_thread) = unsafe { (*self.plugin).on_main_thread } {
                unsafe { on_main_thread(self.plugin) };
            }
        }
        if let Some(on_timer) = unsafe { self.timer_ext.as_ref() }.and_then(|ext| ext.on_timer) {
            for id in host.due_timers() {
                unsafe { on_timer(self.plugin, id) };
            }
        }
        #[cfg(target_os = "linux")]
        if let Some(on_fd) = unsafe { self.fd_ext.as_ref() }.and_then(|ext| ext.on_fd) {
            for (fd, flags) in host.ready_fds() {
                unsafe { on_fd(self.plugin, fd, flags) };
            }
        }
    }

    /// Rescans the parameters if the plugin asked to.  Returns the new
    /// views, or `None` without a request.  A rescan that changes the ids
    /// or ranges the processor was built with cannot be followed: the
    /// processor, the views and every value sent use that one table.  The
    /// old list is kept and a reload is asked for: the insert stops
    /// processing and the path rebuilds it with the new controls.
    pub(crate) fn rescan_if_requested(&mut self) -> Option<Vec<PluginParamView>> {
        if !self.core.host.params_rescan.swap(false, Ordering::Relaxed) {
            return None;
        }
        let previous = std::mem::take(&mut self.cached_params);
        self.scan_parameters();
        let same = self.cached_params.len() == self.ranges.len()
            && self
                .cached_params
                .iter()
                .zip(self.ranges.iter())
                .all(|(param, range)| param.range() == *range);
        if !same {
            eprintln!("[plugins] a CLAP plugin changed its parameter list; reloading it");
            self.cached_params = previous;
            self.shared.want_reload();
        }
        Some(self.parameters())
    }

    /// Whether the plugin asked for `params.flush` (`request_flush`) and no
    /// block delivered it yet.
    pub(crate) fn flush_requested(&self) -> bool {
        self.core.host.flush_requested.load(Ordering::Relaxed)
    }

    /// Takes the flush request, for a flush the UI makes.
    pub(crate) fn take_flush_request(&self) -> bool {
        self.core
            .host
            .flush_requested
            .swap(false, Ordering::Relaxed)
    }

    /// Whether the latency changed outside a restart (which CLAP does not
    /// allow while active, but some plugins do anyway): re-read it.
    pub(crate) fn reread_latency_if_changed(&mut self) {
        if self.core.host.latency_changed.load(Ordering::Acquire) {
            self.query_latency();
        }
    }

    /// Saves the plugin's state through `clap_plugin_state.save`, a
    /// main-thread call CLAP allows while the audio thread processes.  Off
    /// the main thread the save is deferred.
    pub(crate) fn save_state(&mut self) -> StateSave {
        if !self.core.host.on_main_thread() {
            return StateSave::Deferred;
        }
        let Some(save) = (unsafe { self.state_ext.as_ref() }).and_then(|state| state.save) else {
            return StateSave::Unavailable;
        };
        // Cleared first, so a change the plugin marks during the save is
        // picked up by the next one; set again if the save fails.
        self.shared.take_state_dirty();
        let mut writer = StateWriter::default();
        let stream = writer.stream();
        let saved = unsafe { save(self.plugin, &stream) };
        if writer.overflowed {
            eprintln!(
                "[plugins] not saving a CLAP plugin's state: it is larger than {} MB",
                MAX_STATE_BYTES / (1024 * 1024)
            );
        }
        if saved && !writer.overflowed {
            StateSave::Saved(writer.bytes)
        } else {
            self.shared.mark_state_dirty();
            StateSave::Unavailable
        }
    }

    pub(super) fn scan_parameters(&mut self) {
        self.cached_params.clear();
        let Some(params) = (unsafe { self.params_ext.as_ref() }) else {
            return;
        };
        let (Some(count), Some(get_info)) = (params.count, params.get_info) else {
            return;
        };
        let count = unsafe { count(self.plugin) };
        let mut cached = Vec::new();
        for index in 0..count {
            let mut info: clap_param_info = unsafe { std::mem::zeroed() };
            if !unsafe { get_info(self.plugin, index, &mut info) } {
                continue;
            }
            if info.flags & CLAP_PARAM_IS_HIDDEN != 0 {
                continue;
            }
            let min = info.min_value;
            let max = info.max_value;
            let stepped = info.flags & (CLAP_PARAM_IS_STEPPED | CLAP_PARAM_IS_ENUM) != 0;
            let step_count = if stepped {
                (max - min).round().max(0.0) as i32
            } else {
                0
            };
            cached.push(ClapParam {
                id: info.id,
                name: fixed_string(&info.name),
                min,
                max,
                default: info.default_value,
                step_count,
                read_only: info.flags & CLAP_PARAM_IS_READONLY != 0,
            });
        }
        self.cached_params = cached;
    }

    pub(super) fn value_text(&self, id: u32, value: f64) -> String {
        let Some(params) = (unsafe { self.params_ext.as_ref() }) else {
            return format!("{value:.2}");
        };
        let Some(format) = params.value_to_text else {
            return format!("{value:.2}");
        };
        let mut buffer = [0 as c_char; CLAP_NAME_SIZE];
        let ok = unsafe {
            format(
                self.plugin,
                id,
                value,
                buffer.as_mut_ptr(),
                buffer.len() as u32,
            )
        };
        if ok {
            fixed_string(&buffer)
        } else {
            format!("{value:.2}")
        }
    }

    /// Reads every control from the plugin (`params.get_value`).
    pub(crate) fn parameters(&self) -> Vec<PluginParamView> {
        let Some(params) = (unsafe { self.params_ext.as_ref() }) else {
            return Vec::new();
        };
        self.cached_params
            .iter()
            .map(|param| {
                let native = params
                    .get_value
                    .and_then(|get| {
                        let mut value = param.default;
                        unsafe { get(self.plugin, param.id, &mut value) }.then_some(value)
                    })
                    .unwrap_or(param.default);
                let range = param.range();
                PluginParamView {
                    id: param.id,
                    name: param.name.clone(),
                    value: range.normalise(native),
                    default: range.normalise(param.default),
                    step_count: param.step_count,
                    unit: String::new(),
                    display: self.value_text(param.id, native),
                    read_only: param.read_only,
                }
            })
            .collect()
    }

    /// The plugin's text for a normalized value of `id`.
    pub(crate) fn display_for(&self, id: u32, value: f64) -> String {
        let Some(param) = self.cached_params.iter().find(|param| param.id == id) else {
            return String::new();
        };
        self.value_text(id, param.range().denormalise(value))
    }

    /// The views every control has at its default.
    pub(crate) fn default_views(&self) -> Vec<PluginParamView> {
        self.cached_params
            .iter()
            .map(|param| {
                let range = param.range();
                PluginParamView {
                    id: param.id,
                    name: param.name.clone(),
                    value: range.normalise(param.default),
                    default: range.normalise(param.default),
                    step_count: param.step_count,
                    unit: String::new(),
                    display: self.value_text(param.id, param.default),
                    read_only: param.read_only,
                }
            })
            .collect()
    }

    /// The writable controls' defaults, `(index, normalized)` in the
    /// processor's table.
    pub(crate) fn default_values(&self) -> Vec<(usize, f64)> {
        self.cached_params
            .iter()
            .filter(|param| !param.read_only)
            .filter_map(|param| {
                let index = self.ids.index_of(param.id)?;
                Some((index, param.range().normalise(param.default)))
            })
            .collect()
    }

    /// Whether `id` is a control the plugin does not let the host set.
    pub(crate) fn is_read_only(&self, id: u32) -> bool {
        self.cached_params
            .iter()
            .any(|param| param.id == id && param.read_only)
    }

    /// The native value of a normalized one at `index` of the table.
    pub(crate) fn native(&self, index: usize, value: f64) -> Option<(u32, f64)> {
        let range = self.ranges.get(index)?;
        Some((range.id, range.denormalise(value)))
    }
}

impl Drop for ClapInstance {
    fn drop(&mut self) {
        // The editor first, then the activation: this is the last handle
        // (an editor window drops its own view before it), and
        // `LoadedPlugin` already stopped processing.
        self.close_editor();
        self.deactivate();
        // `core` goes with the fields: the plugin is destroyed once the
        // processor is gone too.
    }
}

pub(super) fn make_host(state: &Arc<HostState>) -> clap_host {
    clap_host {
        clap_version: CLAP_VERSION,
        host_data: Arc::as_ptr(state) as *mut c_void,
        name: HOST_NAME.as_ptr() as *const c_char,
        vendor: HOST_VENDOR.as_ptr() as *const c_char,
        url: HOST_URL.as_ptr() as *const c_char,
        version: HOST_VERSION.as_ptr() as *const c_char,
        get_extension: Some(host_get_extension),
        request_restart: Some(host_request_restart),
        request_process: Some(host_request_process),
        request_callback: Some(host_request_callback),
    }
}

pub(super) fn extension<T>(plugin: *const clap_plugin, id: &CStr) -> *const T {
    let Some(get) = (unsafe { (*plugin).get_extension }) else {
        return ptr::null();
    };
    unsafe { get(plugin, id.as_ptr()).cast::<T>() }
}
