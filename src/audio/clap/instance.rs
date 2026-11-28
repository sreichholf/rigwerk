//! Main-thread half, behind the UI-only `PluginHandle` lock.  The
//! audio-thread half is `processor.rs`.  Audio-thread calls
//! (`stop_processing`, an active `params.flush`) run only while no block
//! is inside the plugin.
//!
//! Both halves share one [`ClapCore`], which `destroy`s the plugin when
//! the last drops.  This half deactivates on drop: processing was already
//! stopped, and the editor is closed first.

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
#[cfg(target_os = "linux")]
use clap_sys::ext::posix_fd_support::{clap_plugin_posix_fd_support, CLAP_EXT_POSIX_FD_SUPPORT};
use clap_sys::ext::state::{clap_plugin_state, CLAP_EXT_STATE};
use clap_sys::ext::timer_support::{clap_plugin_timer_support, CLAP_EXT_TIMER_SUPPORT};
use clap_sys::host::clap_host;
use clap_sys::plugin::clap_plugin;
use clap_sys::string_sizes::CLAP_NAME_SIZE;
use clap_sys::version::CLAP_VERSION;

use crate::audio::plugins::host::{
    apply_reported, drain_reported, drive_pause_or_fail, finish_capture, resume_or_log, HostIo,
    PluginHostUi,
};
use crate::audio::plugins::{
    param_transfer, AudioThreadScope, Capture, ClapFailure, ParamIds, ParamOut, ParamReceiver,
    PauseStep, PluginParamView, PluginShared, ProcessFailure, Restore, Retry, StateSave,
    RESTART_ACTIVATION,
};

use super::gui::{choose_editor_api, session_candidates, EditorApi};
use super::host::{
    host_get_extension, host_request_callback, host_request_process, host_request_restart,
    HostState, HOST_NAME, HOST_URL, HOST_VENDOR, HOST_VERSION,
};
use super::module::{acquire_module, ModuleHandle};
use super::ports::{port_layout, read_audio_ports, PortBuffers};
use super::processor::{stop_processing, ClapProcessor, ClapRange, EventBuffer, OutputSink};
use super::scan::fixed_string;
use super::state::{StateReader, StateWriter, MAX_STATE_BYTES};

/// The largest block the chain accepts, equal to the VST3 side.
/// Plugins are set up at this size, so it must stay one fixed-buffer
/// effects can handle.  Longer bridge blocks are split.
pub(super) const MAX_BLOCK: usize = 512;

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

/// `destroy` runs once on drop, even after a failed `init` (CLAP allows
/// that).  It does not deactivate: the owner of the activation does that
/// first.
pub(super) struct PluginPointer(pub(super) *const clap_plugin);

impl Drop for PluginPointer {
    fn drop(&mut self) {
        if let Some(destroy) = unsafe { (*self.0).destroy } {
            unsafe { destroy(self.0) };
        }
    }
}

/// Fields drop in declaration order: destroy the instance, free the host
/// it pointed at, then release the module (deinit and unload if last).
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

pub(crate) struct ClapInstance {
    pub(super) plugin: *const clap_plugin,
    pub(super) params_ext: *const clap_plugin_params,
    pub(super) gui_ext: *const clap_plugin_gui,
    pub(super) timer_ext: *const clap_plugin_timer_support,
    pub(super) state_ext: *const clap_plugin_state,
    pub(super) latency_ext: *const clap_plugin_latency,
    /// `posix-fd-support` is Linux-only.
    #[cfg(target_os = "linux")]
    pub(super) fd_ext: *const clap_plugin_posix_fd_support,
    /// The panel's list, as last scanned.
    pub(super) cached_params: Vec<ClapParam>,
    /// The table the processor was built with (index → id and range).  A
    /// rescan that changes it asks for a reload instead.
    pub(super) ranges: Arc<[ClapRange]>,
    pub(super) ids: Arc<ParamIds>,
    pub(super) active: bool,
    pub(super) sample_rate: f64,
    pub(super) max_block: usize,
    pub(super) editor_open: bool,
    /// `None` when this session cannot show an editor.
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
    /// The ids of the restored values still queued for the processor.
    pub(crate) undelivered: Vec<u32>,
}

impl ClapInstance {
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

    /// Main thread.  A failed step tears down what was set up before the
    /// module is released.  Processing is not started: `start_processing`
    /// is an audio-thread call, made by the first block.
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
            #[cfg(target_os = "linux")]
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

        // While still inactive: `state.load` is main-thread, and so is
        // `params.flush` until activation, so both land before the first
        // block.
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

        // Last, before activation: the restored state may have changed
        // the ports, and they cannot change while the plugin is active.
        // A rescan from here on means the buffers are stale.
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
        // Consumed here: the views of the new effect publish it.
        shared.take_latency_change();

        let mut views = instance.parameters();
        let undelivered = queued
            .iter()
            .filter_map(|&(index, _)| ids.id(index))
            .collect();
        // A value queued for the first block is what the plugin has, so
        // the panel shows that rather than the old one.
        for (index, value) in queued {
            if let Some(view) = views.get_mut(index) {
                view.value = value;
                view.display = instance.display_for(view.id, value);
            }
        }
        // Room for a full ring, every overflow, and every value a block
        // could not take.
        let events = EventBuffer::with_capacity(from_ui.capacity() + 2 * from_ui.params());
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
            retry: Retry::new(ids.len()),
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
            undelivered,
            instance,
        })
    }

    pub(crate) fn host(&self) -> Arc<HostState> {
        Arc::clone(&self.core.host)
    }

    pub(super) fn load_state(&mut self, bytes: &[u8]) -> bool {
        let Some(load) = (unsafe { self.state_ext.as_ref() }).and_then(|state| state.load) else {
            return false;
        };
        let mut reader = StateReader::new(bytes);
        let stream = reader.stream();
        unsafe { load(self.plugin, &stream) }
    }

    /// Before activation, through `params.flush` on the main thread.
    /// `None` is the default.  Without `flush` the values go out with
    /// the first block and are returned for the views.
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

    /// A failed reactivation leaves the plugin inactive for good.
    pub(crate) fn is_active(&self) -> bool {
        self.active
    }

    /// Main thread, with processing stopped.
    pub(crate) fn deactivate(&mut self) {
        if !self.active {
            return;
        }
        if let Some(deactivate) = unsafe { (*self.plugin).deactivate } {
            unsafe { deactivate(self.plugin) };
        }
        self.active = false;
    }

    /// CLAP allows this on the main thread while the plugin is active.
    /// An inactive plugin, or one without the extension, reports 0.
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

    /// For `request_restart`.  Main thread, paused, no block inside.
    /// False leaves the plugin inactive: it must not be resumed.
    pub(crate) fn restart(&mut self) -> bool {
        self.deactivate();
        // The plugin may change its latency (and call `latency.changed`)
        // inside `activate`; it is read once activation is done.
        let activated = self.activate().is_ok();
        self.query_latency();
        activated
    }

    /// As the audio thread.  The caller knows no block is inside.
    pub(crate) fn stop_processing(&mut self) {
        stop_processing(self.plugin);
    }

    pub(crate) fn can_flush(&self) -> bool {
        unsafe { self.params_ext.as_ref() }.is_some_and(|params| params.flush.is_some())
    }

    /// Returns reported output values, `(index, normalized)`.  `flush` is
    /// main-thread while inactive and audio-thread while active, never
    /// concurrent with `process`: an active plugin is paused with no
    /// block inside, and the call runs as its audio thread.
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

    /// `on_main_thread`, due timers, and ready fds.  CLAP allows these
    /// while the audio thread processes.
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

    /// `None` without a request.  A rescan that changes the ids or ranges
    /// the processor was built with cannot be followed: that table is
    /// what the processor, the views, and every value use.  The old list
    /// stays and a reload is asked for.
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

    /// `request_flush`, and no block has delivered it yet.
    pub(crate) fn flush_requested(&self) -> bool {
        self.core.host.flush_requested.load(Ordering::Relaxed)
    }

    /// For a flush the UI makes.
    pub(crate) fn take_flush_request(&self) -> bool {
        self.core
            .host
            .flush_requested
            .swap(false, Ordering::Relaxed)
    }

    /// CLAP does not allow a latency change while active, but some
    /// plugins do it anyway: re-read it.
    pub(crate) fn reread_latency_if_changed(&mut self) {
        if self.core.host.latency_changed.load(Ordering::Acquire) {
            self.query_latency();
        }
    }

    /// Main thread; CLAP allows it while the audio thread processes.
    /// Off the main thread the save is deferred.
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

    pub(crate) fn display_for(&self, id: u32, value: f64) -> String {
        let Some(param) = self.cached_params.iter().find(|param| param.id == id) else {
            return String::new();
        };
        self.value_text(id, param.range().denormalise(value))
    }

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

    pub(crate) fn is_read_only(&self, id: u32) -> bool {
        self.cached_params
            .iter()
            .any(|param| param.id == id && param.read_only)
    }

    pub(crate) fn native(&self, index: usize, value: f64) -> Option<(u32, f64)> {
        let range = self.ranges.get(index)?;
        Some((range.id, range.denormalise(value)))
    }

    /// While processing, values go through the transfer into the next
    /// block.  While idle, and nothing older is queued, the UI flushes
    /// them itself so a value with no audio reaches the plugin at once.
    /// Never waits: a pause that is not ready leaves them queued for
    /// [`PluginHostUi::service`].
    fn send_values(&mut self, io: &mut HostIo<'_>, values: &[(usize, f64)]) {
        let shared = io.shared;
        let native = |clap: &ClapInstance| -> Vec<(u32, f64)> {
            values
                .iter()
                .filter_map(|&(index, value)| clap.native(index, value))
                .collect()
        };
        if !self.is_active() {
            // A failed reactivation: the plugin stays paused and never
            // starts again, so `flush` is a plain main-thread call.
            if self.can_flush() {
                let reported = self.flush_values(&native(self));
                apply_reported(io.ids, io.views, &reported, |id, value| {
                    self.display_for(id, value)
                });
            }
            return;
        }
        // Only from a pause, or before a start no block has begun: a plugin
        // between the audio side's start and `started` is left to it.
        let direct = shared.pause.is_idle()
            && io.pause_wait.is_none()
            && !shared.restart_pending()
            && self.can_flush()
            && !io.out.has_queued();
        if direct && drive_pause_or_fail(io, || self.stop_processing()) == PauseStep::Ready {
            // What the last blocks reported is older than this flush.
            drain_reported(io.from_audio, io.ids, io.views, |id, value| {
                self.display_for(id, value)
            });
            let reported = self.flush_values(&native(self));
            apply_reported(io.ids, io.views, &reported, |id, value| {
                self.display_for(id, value)
            });
            // A failed effect stays paused: no new start per knob move.
            if shared.failure().is_none() {
                resume_or_log(&shared.pause);
            }
            return;
        }
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

impl PluginHostUi for ClapInstance {
    fn parameter_text(&self, id: u32, value: f64) -> Option<String> {
        Some(self.display_for(id, value))
    }

    fn is_read_only(&self, id: u32) -> bool {
        self.is_read_only(id)
    }

    fn write_parameter(&mut self, io: &mut HostIo<'_>, index: usize, value: f64) {
        self.send_values(io, &[(index, value)]);
    }

    fn read_parameters(&mut self, io: &mut HostIo<'_>) -> Option<Vec<PluginParamView>> {
        if io.out.has_queued() {
            return None;
        }
        Some(self.parameters())
    }

    fn sync_parameters(&mut self, io: &mut HostIo<'_>) -> Option<Vec<PluginParamView>> {
        if io.out.has_queued() {
            return None;
        }
        Some(self.parameters())
    }

    fn reset_parameters(&mut self, io: &mut HostIo<'_>) -> Vec<PluginParamView> {
        let defaults = self.default_values();
        let views = self.default_views();
        self.send_values(io, &defaults);
        views
    }

    fn capture_state(&mut self, io: &mut HostIo<'_>) -> Capture {
        io.undelivered.settle(io.out.applied());
        finish_capture(io, self.save_state(), self.state_holds_params())
    }

    fn service(&mut self, io: &mut HostIo<'_>) -> bool {
        self.service_callbacks();
        let mut changed = drain_reported(io.from_audio, io.ids, io.views, |id, value| {
            self.display_for(id, value)
        });
        if let Some(fresh) = self.rescan_if_requested() {
            *io.views = fresh;
            changed = true;
        }
        if !self.is_active() && self.take_flush_request() {
            // Inactive (a failed reactivation): a main-thread flush.
            let reported = self.flush_values(&[]);
            changed |= apply_reported(io.ids, io.views, &reported, |id, value| {
                self.display_for(id, value)
            });
        }
        let flush_wanted = self.flush_requested() && !io.shared.pause.processing();
        if io.shared.failure().is_none()
            && (io.shared.restart_pending() || io.pause_wait.is_some() || flush_wanted)
        {
            let step = drive_pause_or_fail(io, || self.stop_processing());
            // The failure text is new: republish so the effect shows it.
            changed |= step == PauseStep::Stranded;
            if step == PauseStep::Ready {
                if io.shared.take_restart() & RESTART_ACTIVATION != 0 && !self.restart() {
                    // Left paused: an inactive plugin must not start again.
                    io.shared.fail(ProcessFailure::Clap(ClapFailure::NotActive));
                    changed = true;
                }
                if self.is_active() && self.take_flush_request() {
                    // Quiet: what the last blocks reported is older.
                    changed |= drain_reported(io.from_audio, io.ids, io.views, |id, value| {
                        self.display_for(id, value)
                    });
                    let reported = self.flush_values(&[]);
                    changed |= apply_reported(io.ids, io.views, &reported, |id, value| {
                        self.display_for(id, value)
                    });
                }
                // Never resume an inactive plugin, or restart a failed one.
                if self.is_active() && io.shared.failure().is_none() {
                    resume_or_log(&io.shared.pause);
                }
            }
        }
        self.reread_latency_if_changed();
        changed
    }

    fn state_holds_params(&self) -> bool {
        true
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
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
