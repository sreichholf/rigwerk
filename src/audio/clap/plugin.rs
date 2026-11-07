//! Loaded CLAP plugin instances and process/parameter/GUI methods.

use std::ffi::{c_char, c_void, CStr, CString};
use std::path::Path;
use std::ptr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap_sys::events::{
    clap_event_header, clap_event_param_value, clap_input_events, clap_output_events,
    CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_PARAM_VALUE,
};
use clap_sys::ext::audio_ports::{clap_plugin_audio_ports, CLAP_EXT_AUDIO_PORTS};
use clap_sys::ext::gui::{
    clap_plugin_gui, clap_window, clap_window_handle, CLAP_EXT_GUI, CLAP_WINDOW_API_WIN32,
};
use clap_sys::ext::latency::{clap_plugin_latency, CLAP_EXT_LATENCY};
use clap_sys::ext::params::{
    clap_param_info, clap_plugin_params, CLAP_EXT_PARAMS, CLAP_PARAM_IS_ENUM, CLAP_PARAM_IS_HIDDEN,
    CLAP_PARAM_IS_READONLY, CLAP_PARAM_IS_STEPPED,
};
use clap_sys::ext::state::{clap_plugin_state, CLAP_EXT_STATE};
use clap_sys::ext::timer_support::{clap_plugin_timer_support, CLAP_EXT_TIMER_SUPPORT};
use clap_sys::host::clap_host;
use clap_sys::plugin::clap_plugin;
use clap_sys::process::{clap_process, CLAP_PROCESS_ERROR};
use clap_sys::string_sizes::CLAP_NAME_SIZE;
use clap_sys::version::CLAP_VERSION;

use crate::audio::plugins::{clamp_latency, PluginParamView, Restore};

use super::host::{
    host_get_extension, host_request_callback, host_request_process, host_request_restart,
    AudioThreadScope, HostState, MainThreadTarget, HOST_NAME, HOST_URL, HOST_VENDOR, HOST_VERSION,
};
use super::module::{acquire_module, ModuleHandle};
use super::ports::{port_layout, read_audio_ports, PortBuffers, AUDIO_PORTS_CHANGED};
use super::scan::fixed_string;
use super::state::StateReader;

/// The largest block the insert chain accepts.  Kept in step with the VST3
/// side: plugins get set up with this size, so it must stay a block size that
/// fixed-buffer effects can handle, and longer bridge blocks are split.
pub(super) const MAX_BLOCK: usize = 512;

/// How long a restart waits for an audio callback before the main thread
/// takes the stream for stopped and restarts the plugin itself.
pub(super) const RESTART_STALL: Duration = Duration::from_millis(200);

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

/// The event list passed to `process` and `params.flush`.  It is kept on the
/// instance so parameter changes do not allocate on the audio thread.
#[derive(Default)]
pub(super) struct EventBuffer {
    pub(super) events: Vec<clap_event_param_value>,
    pub(super) pointers: Vec<*const clap_event_header>,
}

impl EventBuffer {
    pub(super) fn clear(&mut self) {
        self.events.clear();
        self.pointers.clear();
    }

    pub(super) fn push_value(&mut self, id: u32, value: f64) {
        self.events.push(clap_event_param_value {
            header: clap_event_header {
                size: std::mem::size_of::<clap_event_param_value>() as u32,
                time: 0,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: CLAP_EVENT_PARAM_VALUE,
                flags: 0,
            },
            param_id: id,
            cookie: ptr::null_mut(),
            note_id: -1,
            port_index: -1,
            channel: -1,
            key: -1,
            value,
        });
    }

    pub(super) fn finish(&mut self) {
        self.pointers.clear();
        for event in &self.events {
            self.pointers
                .push(&event.header as *const clap_event_header);
        }
    }

    pub(super) fn as_clap(&self) -> clap_input_events {
        clap_input_events {
            ctx: self as *const _ as *mut c_void,
            size: Some(input_events_size),
            get: Some(input_events_get),
        }
    }
}

pub(super) unsafe extern "C" fn input_events_size(list: *const clap_input_events) -> u32 {
    let Some(list) = (unsafe { list.as_ref() }) else {
        return 0;
    };
    let Some(buffer) = (unsafe { (list.ctx as *const EventBuffer).as_ref() }) else {
        return 0;
    };
    buffer.pointers.len() as u32
}

pub(super) unsafe extern "C" fn input_events_get(
    list: *const clap_input_events,
    index: u32,
) -> *const clap_event_header {
    let Some(list) = (unsafe { list.as_ref() }) else {
        return ptr::null();
    };
    let Some(buffer) = (unsafe { (list.ctx as *const EventBuffer).as_ref() }) else {
        return ptr::null();
    };
    buffer
        .pointers
        .get(index as usize)
        .copied()
        .unwrap_or(ptr::null())
}

pub(super) unsafe extern "C" fn discard_output_event(
    _list: *const clap_output_events,
    _event: *const clap_event_header,
) -> bool {
    false
}

pub(super) fn output_events() -> clap_output_events {
    clap_output_events {
        ctx: ptr::null_mut(),
        try_push: Some(discard_output_event),
    }
}

/// A plugin instance `create_plugin` returned.  Dropping it deactivates the
/// plugin if it was activated and then destroys it, exactly once, whether or
/// not `init` ran or succeeded (CLAP allows `destroy` after a failed `init`).
pub(super) struct PluginInstance {
    pub(super) plugin: *const clap_plugin,
    pub(super) active: bool,
}

impl PluginInstance {
    /// # Safety
    /// `plugin` must be a non-null instance from `create_plugin` that nothing
    /// else destroys.
    pub(super) unsafe fn new(plugin: *const clap_plugin) -> Self {
        Self {
            plugin,
            active: false,
        }
    }
}

impl Drop for PluginInstance {
    fn drop(&mut self) {
        unsafe {
            if self.active {
                if let Some(deactivate) = (*self.plugin).deactivate {
                    deactivate(self.plugin);
                }
            }
            if let Some(destroy) = (*self.plugin).destroy {
                destroy(self.plugin);
            }
        }
    }
}

/// A loaded CLAP effect.  The plugin pointer and its module are kept together
/// so the plugin can never outlive the code it came from.
pub(crate) struct ClapPlugin {
    // Fields drop in declaration order, which is the teardown order CLAP
    // needs: destroy the instance, free the host the plugin pointed at, and
    // only then release the shared module (which deinits the entry and
    // unloads the library if this was its last user).  These four must stay
    // first and in this order.
    pub(super) instance: PluginInstance,
    pub(super) _host: Box<clap_host>,
    pub(super) host_state: Arc<HostState>,
    pub(super) _module: ModuleHandle,
    /// `instance.plugin`, copied for brevity; owned by `instance`.
    pub(super) plugin: *const clap_plugin,
    pub(super) params_ext: *const clap_plugin_params,
    pub(super) gui_ext: *const clap_plugin_gui,
    pub(super) timer_ext: *const clap_plugin_timer_support,
    pub(super) state_ext: *const clap_plugin_state,
    pub(super) latency_ext: *const clap_plugin_latency,
    pub(super) cached_params: Vec<ClapParam>,
    /// One buffer per declared audio port, set up just before activation.
    pub(super) inputs: PortBuffers,
    pub(super) outputs: PortBuffers,
    pub(super) dry_l: Vec<f32>,
    pub(super) dry_r: Vec<f32>,
    pub(super) events: EventBuffer,
    pub(super) pending_params: Vec<(u32, f64)>,
    pub(super) sample_rate: f64,
    pub(super) max_block: usize,
    pub(super) steady_time: i64,
    pub(super) started: bool,
    /// While a restart waits for the audio thread to stop processing: the
    /// block count and the time the main thread first saw it waiting.
    pub(super) restart_wait: Option<(u64, Instant)>,
    pub(super) editor_open: bool,
    pub(super) has_editor: bool,
}

// SAFETY: every access to the raw CLAP pointers is serialised through the
// `Arc<Mutex<ClapPlugin>>` the insert chain owns.  The plugin is never called
// from two threads at once, and `CLAP` explicitly allows moving a stopped
// instance between threads.
unsafe impl Send for ClapPlugin {}

impl ClapPlugin {
    pub(crate) fn load(
        path: &Path,
        plugin_id: &str,
        sample_rate: u32,
        max_block: usize,
        restore: &Restore<'_>,
    ) -> Result<(Self, bool), String> {
        let module = acquire_module(path)?;
        Self::instantiate(module, plugin_id, sample_rate, max_block, restore)
    }

    /// Creates, sets up and activates one plugin from an initialised module.
    /// Every step may fail; the guards then tear down whatever was set up, in
    /// order, before the module is released.
    pub(super) fn instantiate(
        module: ModuleHandle,
        plugin_id: &str,
        sample_rate: u32,
        max_block: usize,
        restore: &Restore<'_>,
    ) -> Result<(Self, bool), String> {
        // The host is created before the instance guard, so an early return
        // destroys the plugin and then frees the host (locals drop in reverse
        // order; `module`, a parameter, is released last).
        let host_state = Arc::new(HostState::new());
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
        let instance = unsafe { PluginInstance::new(plugin) };
        let Some(init) = (unsafe { (*plugin).init }) else {
            return Err("The CLAP plugin has no init function.".to_owned());
        };
        if !unsafe { init(plugin) } {
            return Err("The CLAP plugin refused to start.".to_owned());
        }

        let params_ext = extension::<clap_plugin_params>(plugin, CLAP_EXT_PARAMS);
        let gui_ext = extension::<clap_plugin_gui>(plugin, CLAP_EXT_GUI);
        let timer_ext = extension::<clap_plugin_timer_support>(plugin, CLAP_EXT_TIMER_SUPPORT);
        let audio_ports_ext = extension::<clap_plugin_audio_ports>(plugin, CLAP_EXT_AUDIO_PORTS);
        let state_ext = extension::<clap_plugin_state>(plugin, CLAP_EXT_STATE);
        let latency_ext = extension::<clap_plugin_latency>(plugin, CLAP_EXT_LATENCY);

        let rate = if sample_rate == 0 {
            48_000.0
        } else {
            sample_rate as f64
        };
        let block = max_block.clamp(1, MAX_BLOCK);

        let mut plugin = Self {
            instance,
            _host: host,
            host_state,
            _module: module,
            plugin,
            params_ext,
            gui_ext,
            timer_ext,
            state_ext,
            latency_ext,
            cached_params: Vec::new(),
            inputs: PortBuffers::default(),
            outputs: PortBuffers::default(),
            dry_l: vec![0.0; block],
            dry_r: vec![0.0; block],
            events: EventBuffer::default(),
            pending_params: Vec::new(),
            sample_rate: rate,
            max_block: block,
            steady_time: 0,
            started: false,
            restart_wait: None,
            editor_open: false,
            has_editor: false,
        };
        plugin.has_editor = gui_supports_embedding(plugin.plugin, plugin.gui_ext);
        *plugin
            .host_state
            .target
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = MainThreadTarget {
            plugin: plugin.plugin,
            timer_ext: plugin.timer_ext,
            state_ext: plugin.state_ext,
        };
        // Restore the saved state and controls while the plugin is still
        // inactive: `state.load` is a main-thread call, and `params.flush` is
        // one too until activation (never concurrent with `process`), so both
        // take effect before the first audio block and the panel reads the
        // restored values below.  Load, activate and this restore run on the
        // UI thread, which is the CLAP main thread.
        plugin.scan_parameters();
        let state_loaded = restore
            .state
            .as_deref()
            .is_some_and(|bytes| plugin.load_state(bytes));
        if state_loaded {
            // The state may bring a different parameter set.
            plugin.scan_parameters();
        }
        let values = restore.params(state_loaded);
        plugin.restore_parameters(&values);
        // The ports are read last before activation: the restored state may
        // have changed them, and they cannot change while the plugin is
        // active.  A rescan request from here on means the buffers are stale.
        plugin
            .host_state
            .ports_changed
            .store(false, Ordering::Relaxed);
        let layout = port_layout(
            read_audio_ports(plugin.plugin, audio_ports_ext)?
                .as_ref()
                .map(|(inputs, outputs)| (inputs.as_slice(), outputs.as_slice())),
        )?;
        plugin.inputs = PortBuffers::new(&layout.inputs, layout.main_input, block);
        plugin.outputs = PortBuffers::new(&layout.outputs, layout.main_output, block);
        plugin.activate()?;
        plugin.query_latency();
        // The first value goes out with the views of the new insert.
        plugin
            .host_state
            .latency_unseen
            .store(false, Ordering::Relaxed);
        // `start_processing` is an audio-thread call in CLAP; the first
        // `process` makes it (see `process`).
        plugin.refresh_parameters();
        Ok((plugin, state_loaded && values.is_empty()))
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
    /// activation.  They go through `params.flush` right away; a plugin
    /// without `flush` gets them with the first `process` instead, and until
    /// then [`Self::has_pending_parameters`] keeps the editor sync from
    /// reading (and saving) the not-yet-applied defaults.
    pub(super) fn restore_parameters(&mut self, values: &[(u32, Option<f64>)]) {
        for (id, value) in values {
            let Some(param) = self.cached_params.iter().find(|param| param.id == *id) else {
                continue;
            };
            if param.read_only {
                continue;
            }
            let native = match value {
                Some(value) => self.denormalise(param, *value),
                None => param.default,
            };
            if let Some(pending) = self
                .pending_params
                .iter_mut()
                .find(|(pending_id, _)| *pending_id == *id)
            {
                pending.1 = native;
            } else {
                self.pending_params.push((*id, native));
            }
        }
        let can_flush =
            unsafe { self.params_ext.as_ref() }.is_some_and(|params| params.flush.is_some());
        if !self.instance.active && can_flush {
            self.flush_parameters(false);
        }
        self.note_pending();
    }

    /// Publishes whether values are queued, for [`HostState::save_state`].
    pub(super) fn note_pending(&self) {
        self.host_state
            .params_pending
            .store(!self.pending_params.is_empty(), Ordering::Release);
    }

    /// Loads with nothing to restore, for the fixture smoke tests.
    #[cfg(test)]
    pub(crate) fn load_fresh(
        path: &Path,
        plugin_id: &str,
        sample_rate: u32,
        max_block: usize,
    ) -> Result<Self, String> {
        let pref = crate::audio::PluginPref::default();
        let restore = Restore::from_pref(&pref);
        Self::load(path, plugin_id, sample_rate, max_block, &restore).map(|(plugin, _)| plugin)
    }

    /// The callback state the main-thread servicing works from.
    pub(crate) fn host(&self) -> Arc<HostState> {
        Arc::clone(&self.host_state)
    }

    pub(crate) fn has_editor(&self) -> bool {
        self.has_editor
    }

    pub(super) fn activate(&mut self) -> Result<(), String> {
        let Some(activate) = (unsafe { (*self.plugin).activate }) else {
            return Err("The CLAP plugin cannot be activated.".to_owned());
        };
        if !unsafe { activate(self.plugin, self.sample_rate, 1, self.max_block as u32) } {
            return Err("The CLAP plugin refused to activate.".to_owned());
        }
        self.instance.active = true;
        Ok(())
    }

    /// Reads `clap_plugin_latency.get` into [`HostState::latency`].  CLAP
    /// allows the call on the main thread while the plugin is active; an
    /// inactive plugin (or one without the extension) reports 0.
    pub(super) fn query_latency(&mut self) {
        // Cleared first, so a change reported during the call is read again.
        self.host_state
            .latency_changed
            .store(false, Ordering::Release);
        let latency = if self.instance.active {
            unsafe { self.latency_ext.as_ref() }
                .and_then(|latency| latency.get)
                .map_or(0, |get| unsafe { get(self.plugin) })
        } else {
            0
        };
        let latency = clamp_latency(latency);
        if self.host_state.latency.swap(latency, Ordering::AcqRel) != latency {
            self.host_state
                .latency_unseen
                .store(true, Ordering::Release);
        }
    }

    /// The latency in samples the plugin reported at its last activation.
    #[cfg(test)]
    pub(crate) fn latency_samples(&self) -> u32 {
        self.host_state.latency()
    }

    /// Deactivates and reactivates the plugin for `request_restart`, then
    /// reads its latency again.  Main thread only, and only once processing
    /// has stopped: `process` stops it on the audio thread when it sees the
    /// request.  A failed activation leaves the plugin inactive, so the next
    /// `process` reports it and the insert shows the error.
    pub(super) fn restart(&mut self) {
        self.restart_wait = None;
        self.host_state
            .restart_requested
            .store(false, Ordering::Release);
        if self.instance.active {
            if let Some(deactivate) = unsafe { (*self.plugin).deactivate } {
                unsafe { deactivate(self.plugin) };
            }
            self.instance.active = false;
        }
        // The plugin may change its latency (and call `latency.changed`)
        // inside `activate`; it is read once activation is done.
        let _ = self.activate();
        self.query_latency();
    }

    /// Calls `stop_processing` if processing was started.  It is an
    /// audio-thread call: the caller is either the audio thread, or holds
    /// the instance lock while no audio callback runs (the stream stopped,
    /// or the plugin is being destroyed), which makes it the audio thread
    /// for that call.
    pub(super) fn stop_processing(&mut self) {
        if !self.started {
            return;
        }
        let _audio_thread = AudioThreadScope::enter();
        if let Some(stop) = unsafe { (*self.plugin).stop_processing } {
            unsafe { stop(self.plugin) };
        }
        self.started = false;
    }

    /// Whether the audio callback stopped calling `process` while a restart
    /// waits for it.  The first call notes the block count; once it has not
    /// moved for [`RESTART_STALL`], the stream is taken to be stopped.
    pub(super) fn audio_stalled(&mut self) -> bool {
        let blocks = self.host_state.blocks.load(Ordering::Relaxed);
        let now = Instant::now();
        match self.restart_wait {
            Some((seen, since)) if seen == blocks => now.duration_since(since) >= RESTART_STALL,
            _ => {
                self.restart_wait = Some((blocks, now));
                false
            }
        }
    }

    /// Starts processing.  CLAP marks `start_processing` as an audio-thread
    /// call, so `process` makes it on the first block, inside the audio-thread
    /// scope.  The error is static so the audio path does not allocate.
    pub(super) fn start_processing(&mut self) -> Result<(), &'static str> {
        if self.started {
            return Ok(());
        }
        if let Some(start) = unsafe { (*self.plugin).start_processing } {
            if !unsafe { start(self.plugin) } {
                return Err("The CLAP plugin could not start processing.");
            }
        }
        self.started = true;
        // A rescan during `start_processing` would otherwise only be noticed
        // before the second block.
        if self.host_state.ports_changed() {
            return Err(AUDIO_PORTS_CHANGED);
        }
        Ok(())
    }

    /// Reads the plugin's parameters into the cache used by the panel.
    pub(crate) fn refresh_parameters(&mut self) {
        self.scan_parameters();
        // One pending value per parameter at most, so this capacity means
        // `process` never grows the event list on the audio thread.
        let count = self.cached_params.len().max(16);
        self.pending_params
            .reserve(count.saturating_sub(self.pending_params.len()));
        self.events
            .events
            .reserve(count.saturating_sub(self.events.events.len()));
        self.events
            .pointers
            .reserve(count.saturating_sub(self.events.pointers.len()));
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

    pub(super) fn normalise(&self, param: &ClapParam, value: f64) -> f64 {
        if param.max > param.min {
            ((value - param.min) / (param.max - param.min)).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }

    pub(super) fn denormalise(&self, param: &ClapParam, value: f64) -> f64 {
        let value = value.clamp(0.0, 1.0);
        let native = param.min + value * (param.max - param.min);
        if param.step_count > 0 {
            native.round().clamp(param.min, param.max)
        } else {
            native
        }
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
                // A queued value is what the plugin will have from the next
                // block on, so the panel shows that rather than the old one.
                let pending = self
                    .pending_params
                    .iter()
                    .find(|(id, _)| *id == param.id)
                    .map(|(_, value)| *value);
                let native = pending
                    .or_else(|| {
                        params.get_value.and_then(|get| {
                            let mut value = param.default;
                            if unsafe { get(self.plugin, param.id, &mut value) } {
                                Some(value)
                            } else {
                                None
                            }
                        })
                    })
                    .unwrap_or(param.default);
                PluginParamView {
                    id: param.id,
                    name: param.name.clone(),
                    value: self.normalise(param, native),
                    default: self.normalise(param, param.default),
                    step_count: param.step_count,
                    unit: String::new(),
                    display: self.value_text(param.id, native),
                    read_only: param.read_only,
                }
            })
            .collect()
    }

    pub(crate) fn has_pending_parameters(&self) -> bool {
        !self.pending_params.is_empty()
    }

    pub(crate) fn display_for(&self, id: u32, value: f64) -> String {
        let Some(param) = self.cached_params.iter().find(|param| param.id == id) else {
            return String::new();
        };
        let native = self.denormalise(param, value);
        self.value_text(id, native)
    }

    pub(super) fn default_views(&self) -> Vec<PluginParamView> {
        self.cached_params
            .iter()
            .map(|param| PluginParamView {
                id: param.id,
                name: param.name.clone(),
                value: self.normalise(param, param.default),
                default: self.normalise(param, param.default),
                step_count: param.step_count,
                unit: String::new(),
                display: self.value_text(param.id, param.default),
                read_only: param.read_only,
            })
            .collect()
    }

    pub(crate) fn set_parameter(&mut self, id: u32, value: f64) -> Result<(), String> {
        let Some(param) = self
            .cached_params
            .iter()
            .find(|param| param.id == id)
            .cloned()
        else {
            return Err("That control is no longer available.".to_owned());
        };
        if param.read_only {
            return Ok(());
        }
        let native = self.denormalise(&param, value);
        if let Some(saved) = self
            .pending_params
            .iter_mut()
            .find(|(pending_id, _)| *pending_id == id)
        {
            saved.1 = native;
        } else {
            self.pending_params.push((id, native));
        }
        self.note_pending();
        Ok(())
    }

    pub(crate) fn reset_parameters(&mut self) -> Result<Vec<PluginParamView>, String> {
        let defaults: Vec<(u32, f64)> = self
            .cached_params
            .iter()
            .filter(|param| !param.read_only)
            .map(|param| (param.id, self.normalise(param, param.default)))
            .collect();
        for (id, value) in defaults {
            self.set_parameter(id, value)?;
        }
        Ok(self.default_views())
    }

    pub(super) fn flush_parameters(&mut self, keep_pending: bool) {
        if self.pending_params.is_empty() {
            return;
        }
        let Some(flush) = (unsafe { self.params_ext.as_ref() }).and_then(|params| params.flush)
        else {
            if !keep_pending {
                self.pending_params.clear();
            }
            return;
        };
        let mut events = std::mem::take(&mut self.events);
        events.clear();
        if keep_pending {
            for (id, value) in &self.pending_params {
                events.push_value(*id, *value);
            }
        } else {
            for (id, value) in self.pending_params.drain(..) {
                events.push_value(id, value);
            }
        }
        events.finish();
        let input = events.as_clap();
        let output = output_events();
        unsafe { flush(self.plugin, &input, &output) };
        self.events = events;
        self.note_pending();
    }

    /// Runs the plugin over one block in place, blended with the undelayed
    /// input by `mix` (the chain passes 1 and blends with a delayed dry side
    /// itself).  `Ok(false)`: the plugin asked for a restart and the block
    /// passed through unprocessed; the chain treats it as a skipped insert.
    pub(crate) fn process(
        &mut self,
        left: &mut [f32],
        right: &mut [f32],
        frames: usize,
        mix: f32,
    ) -> Result<bool, &'static str> {
        self.host_state.blocks.fetch_add(1, Ordering::Relaxed);
        if self.host_state.restart_requested.load(Ordering::Acquire) {
            // The plugin asked to be restarted.  `stop_processing` is an
            // audio-thread call, so it is made here; the main thread then
            // deactivates and reactivates the plugin (`service_pending`).
            // Until then the insert passes the block through unprocessed:
            // usually one UI frame, a few tens of ms.
            self.stop_processing();
            return Ok(false);
        }
        if !self.instance.active {
            return Err("The CLAP plugin is not active.");
        }
        if self.host_state.ports_changed() {
            // The buffers were built for the old ports; using them could
            // hand the plugin fewer ports or channels than it now expects.
            // A static message keeps this audio path allocation-free.
            return Err(AUDIO_PORTS_CHANGED);
        }
        let _audio_thread = AudioThreadScope::enter();
        if !self.started {
            self.start_processing()?;
        }
        let mix = mix.clamp(0.0, 1.0);
        let frames = frames.min(left.len()).min(right.len());
        if frames == 0 {
            return Ok(true);
        }

        let mut offset = 0;
        while offset < frames {
            let count = (frames - offset).min(self.max_block);
            // Auxiliary inputs and main channels past the second stay
            // silent; the plugin may have written into any of them.
            self.inputs.silence(count);
            let main = self.inputs.main_channels();
            if let [mono] = main {
                for sample in 0..count {
                    mono[sample] = 0.5 * (left[offset + sample] + right[offset + sample]);
                }
            } else {
                main[0][..count].copy_from_slice(&left[offset..offset + count]);
                main[1][..count].copy_from_slice(&right[offset..offset + count]);
            }
            self.outputs.silence(count);
            if mix < 1.0 {
                self.dry_l[..count].copy_from_slice(&left[offset..offset + count]);
                self.dry_r[..count].copy_from_slice(&right[offset..offset + count]);
            }

            let mut events = std::mem::take(&mut self.events);
            events.clear();
            for (id, value) in self.pending_params.drain(..) {
                events.push_value(id, value);
            }
            events.finish();

            self.inputs.refresh();
            self.outputs.refresh();
            let input = events.as_clap();
            let output = output_events();
            // One buffer per declared port, in the plugin's order, so the
            // counts match what it reported through `clap_plugin_audio_ports`.
            let process = clap_process {
                steady_time: self.steady_time,
                frames_count: count as u32,
                transport: ptr::null(),
                audio_inputs: self.inputs.ports.as_ptr(),
                audio_outputs: self.outputs.ports.as_mut_ptr(),
                audio_inputs_count: self.inputs.ports.len() as u32,
                audio_outputs_count: self.outputs.ports.len() as u32,
                in_events: &input,
                out_events: &output,
            };
            let status = match unsafe { (*self.plugin).process } {
                Some(process_fn) => unsafe { process_fn(self.plugin, &process) },
                None => CLAP_PROCESS_ERROR,
            };
            self.steady_time = self.steady_time.saturating_add(count as i64);
            self.events = events;
            // Only now has the plugin received the queued values.
            self.note_pending();

            if status == CLAP_PROCESS_ERROR {
                return Err("The CLAP plugin reported a processing error.");
            }

            let main = self.outputs.main_channels();
            if let [mono] = main {
                for sample in 0..count {
                    let wet = mono[sample];
                    if mix < 1.0 {
                        let dry = 1.0 - mix;
                        left[offset + sample] = self.dry_l[sample] * dry + wet * mix;
                        right[offset + sample] = self.dry_r[sample] * dry + wet * mix;
                    } else {
                        left[offset + sample] = wet;
                        right[offset + sample] = wet;
                    }
                }
            } else {
                for sample in 0..count {
                    let wet_l = main[0][sample];
                    let wet_r = main[1][sample];
                    if mix < 1.0 {
                        let dry = 1.0 - mix;
                        left[offset + sample] = self.dry_l[sample] * dry + wet_l * mix;
                        right[offset + sample] = self.dry_r[sample] * dry + wet_r * mix;
                    } else {
                        left[offset + sample] = wet_l;
                        right[offset + sample] = wet_r;
                    }
                }
            }

            offset += count;
        }
        Ok(true)
    }

    /// Creates the plugin's GUI resources and asks how large its view is.
    /// Win32 embedding only; Linux still uses the in-app parameter panel.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(crate) fn prepare_editor(&mut self) -> Result<(u32, u32), String> {
        if self.editor_open {
            return Ok(self.editor_size().unwrap_or((640, 480)));
        }
        if !self.has_editor {
            return Err("This plugin has no interface that RigWerk can embed.".to_owned());
        }
        let gui = unsafe { self.gui_ext.as_ref() }
            .ok_or_else(|| "This plugin has no GUI extension.".to_owned())?;
        let create = gui
            .create
            .ok_or_else(|| "The plugin cannot create its interface.".to_owned())?;
        if !unsafe { create(self.plugin, CLAP_WINDOW_API_WIN32.as_ptr(), false) } {
            return Err("The plugin refused to create its interface.".to_owned());
        }
        self.editor_open = true;
        let size = self.editor_size().unwrap_or((640, 480));
        Ok(size)
    }

    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(crate) fn editor_size(&self) -> Option<(u32, u32)> {
        let gui = unsafe { self.gui_ext.as_ref() }?;
        let get_size = gui.get_size?;
        let mut width = 640;
        let mut height = 480;
        if unsafe { get_size(self.plugin, &mut width, &mut height) } {
            Some((width.max(80), height.max(80)))
        } else {
            None
        }
    }

    /// Attaches the prepared view to a native window and shows it.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(crate) fn attach_editor(&mut self, hwnd: *mut c_void, scale: f64) -> Result<(), String> {
        let gui = unsafe { self.gui_ext.as_ref() }
            .ok_or_else(|| "This plugin has no GUI extension.".to_owned())?;
        if let Some(set_scale) = gui.set_scale {
            let _ = unsafe { set_scale(self.plugin, scale) };
        }
        let window = clap_window {
            api: CLAP_WINDOW_API_WIN32.as_ptr(),
            specific: clap_window_handle { win32: hwnd },
        };
        if let Some(set_parent) = gui.set_parent {
            if !unsafe { set_parent(self.plugin, &window) } {
                return Err("The plugin could not attach its interface.".to_owned());
            }
        }
        if let Some(show) = gui.show {
            let _ = unsafe { show(self.plugin) };
        }
        self.editor_open = true;
        Ok(())
    }

    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(crate) fn resize_editor(&mut self, width: u32, height: u32) {
        let Some(gui) = (unsafe { self.gui_ext.as_ref() }) else {
            return;
        };
        if let Some(can_resize) = gui.can_resize {
            if !unsafe { can_resize(self.plugin) } {
                return;
            }
        }
        if let Some(set_size) = gui.set_size {
            let _ = unsafe { set_size(self.plugin, width, height) };
        }
    }

    pub(crate) fn close_editor(&mut self) {
        if !self.editor_open {
            return;
        }
        if let Some(gui) = unsafe { self.gui_ext.as_ref() } {
            if let Some(hide) = gui.hide {
                let _ = unsafe { hide(self.plugin) };
            }
            if let Some(destroy) = gui.destroy {
                unsafe { destroy(self.plugin) };
            }
        }
        self.editor_open = false;
    }

    /// The parameter flush and rescan, restart and latency re-read the
    /// plugin asked for.  These read and write the instance's cached
    /// parameters and event list, or change its activation, so they run
    /// under the instance lock on the main thread.
    ///
    /// A restart waits until the audio thread has stopped processing (see
    /// `process`), which passes blocks through dry until the next UI frame
    /// services it.  A plugin that has not processed yet restarts right
    /// away.  If the stream stopped while the plugin was started, no audio
    /// callback will stop it: once no block has run for [`RESTART_STALL`]
    /// the main thread stops it itself (under the instance lock) and
    /// restarts, so the request neither hangs nor keeps the main thread
    /// try-locking the instance every frame.
    ///
    /// `latency.changed` outside a restart (which CLAP does not allow while
    /// active, but some plugins do anyway) just re-reads the value.
    pub(crate) fn service_pending(&mut self) {
        if self
            .host_state
            .flush_requested
            .swap(false, Ordering::Relaxed)
        {
            self.flush_parameters(true);
        }
        if self.host_state.params_rescan.swap(false, Ordering::Relaxed) {
            self.refresh_parameters();
        }
        if self.host_state.restart_requested.load(Ordering::Acquire)
            && (!self.started || self.audio_stalled())
        {
            self.stop_processing();
            self.restart();
        }
        if self.host_state.latency_changed.load(Ordering::Acquire) {
            self.query_latency();
        }
    }
}

impl Drop for ClapPlugin {
    fn drop(&mut self) {
        // Wait for any main-thread service still inside the plugin, then make
        // sure none can start once it is destroyed.
        let mut target = self
            .host_state
            .target
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        target.plugin = ptr::null();
        target.timer_ext = ptr::null();
        drop(target);
        self.close_editor();
        // No audio callback can be inside the plugin: it holds the last
        // reference, and the instance lock is gone with it.
        self.stop_processing();
        // The fields finish the teardown: `instance` deactivates and destroys
        // the plugin, then the host goes and the module is released (see the
        // field order on the struct).
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

pub(super) fn gui_supports_embedding(
    plugin: *const clap_plugin,
    gui: *const clap_plugin_gui,
) -> bool {
    #[cfg(target_os = "windows")]
    {
        let Some(gui) = (unsafe { gui.as_ref() }) else {
            return false;
        };
        let Some(supported) = gui.is_api_supported else {
            return false;
        };
        unsafe { supported(plugin, CLAP_WINDOW_API_WIN32.as_ptr(), false) }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (plugin, gui);
        false
    }
}
