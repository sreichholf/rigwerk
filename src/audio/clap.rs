//! A small CLAP effect host.
//!
//! CLAP is a plain C ABI.  The host opens the `.clap` bundle, hands the plugin
//! a `clap_host` with the callbacks and extensions it implements, and then uses
//! the plugin's parameter, audio-port and GUI extensions.  Everything is
//! serialised through the same mutex the VST3 host already uses, so a plugin
//! instance is only ever called from one thread at a time.

use std::cell::Cell;
use std::ffi::{c_char, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use clap_sys::audio_buffer::clap_audio_buffer;
use clap_sys::entry::clap_plugin_entry;
use clap_sys::events::{
    clap_event_header, clap_event_param_value, clap_input_events, clap_output_events,
    CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_PARAM_VALUE,
};
use clap_sys::ext::audio_ports::{
    clap_audio_port_info, clap_host_audio_ports, clap_plugin_audio_ports, CLAP_AUDIO_PORT_IS_MAIN,
    CLAP_EXT_AUDIO_PORTS,
};
use clap_sys::ext::gui::{
    clap_host_gui, clap_plugin_gui, clap_window, clap_window_handle, CLAP_EXT_GUI,
    CLAP_WINDOW_API_WIN32,
};
use clap_sys::ext::latency::{clap_host_latency, CLAP_EXT_LATENCY};
use clap_sys::ext::log::{clap_host_log, clap_log_severity, CLAP_EXT_LOG};
use clap_sys::ext::params::{
    clap_host_params, clap_param_info, clap_plugin_params, CLAP_EXT_PARAMS, CLAP_PARAM_IS_ENUM,
    CLAP_PARAM_IS_HIDDEN, CLAP_PARAM_IS_READONLY, CLAP_PARAM_IS_STEPPED,
};
use clap_sys::ext::state::{clap_host_state, CLAP_EXT_STATE};
use clap_sys::ext::thread_check::{clap_host_thread_check, CLAP_EXT_THREAD_CHECK};
use clap_sys::ext::timer_support::{
    clap_host_timer_support, clap_plugin_timer_support, CLAP_EXT_TIMER_SUPPORT,
};
use clap_sys::factory::plugin_factory::{clap_plugin_factory, CLAP_PLUGIN_FACTORY_ID};
use clap_sys::host::clap_host;
use clap_sys::plugin::{clap_plugin, clap_plugin_descriptor};
use clap_sys::process::{clap_process, CLAP_PROCESS_ERROR};
use clap_sys::string_sizes::CLAP_NAME_SIZE;
use clap_sys::version::CLAP_VERSION;

use crate::audio::plugins::PluginParamView;

/// The largest block the insert chain accepts.  Kept in step with the VST3
/// side: plugins get set up with this size, so it must stay a block size that
/// fixed-buffer effects can handle, and longer bridge blocks are split.
const MAX_BLOCK: usize = 512;

thread_local! {
    /// True while the current thread is inside a CLAP `process` call.  The
    /// thread-check callback answers from here, so the audio callback never
    /// takes a host-state lock just to report its identity.
    static ON_AUDIO_THREAD: Cell<bool> = const { Cell::new(false) };
}

/// Marks the current thread as the CLAP audio thread for as long as it lives.
struct AudioThreadScope;

impl AudioThreadScope {
    fn enter() -> Self {
        ON_AUDIO_THREAD.with(|flag| flag.set(true));
        Self
    }
}

impl Drop for AudioThreadScope {
    fn drop(&mut self) {
        ON_AUDIO_THREAD.with(|flag| flag.set(false));
    }
}

const HOST_NAME: &[u8] = b"RigWerk\0";
const HOST_VENDOR: &[u8] = b"RigWerk\0";
const HOST_URL: &[u8] = b"https://rigwerk.local\0";
const HOST_VERSION: &[u8] = b"0.2.0\0";

/// One CLAP parameter, cached while the plugin is loaded.
#[derive(Clone, Debug)]
struct ClapParam {
    id: u32,
    name: String,
    min: f64,
    max: f64,
    default: f64,
    step_count: i32,
    read_only: bool,
}

struct TimerEntry {
    id: u32,
    period: Duration,
    next: Instant,
}

/// The thread CLAP plugins treat as their main thread: the UI thread, which
/// runs `on_main_thread`, timers and editors.  Recorded once at startup so a
/// plugin that happens to be loaded elsewhere still sees the right answer from
/// `is_main_thread`.
static MAIN_THREAD: OnceLock<std::thread::ThreadId> = OnceLock::new();

/// Records the calling thread as the CLAP main thread.  Call it from the UI
/// thread before any plugin is loaded.
pub(crate) fn mark_main_thread() {
    let _ = MAIN_THREAD.set(std::thread::current().id());
}

/// What main-thread servicing needs to call into the plugin without taking the
/// instance mutex.  The pointers are cleared (under the write lock) before the
/// plugin is destroyed, so a reader holding the read lock always sees either a
/// live plugin or null.
struct MainThreadTarget {
    plugin: *const clap_plugin,
    timer_ext: *const clap_plugin_timer_support,
}

// Safety: the pointers are only dereferenced on the main thread while the
// read lock is held, and cleared under the write lock before destruction.
unsafe impl Send for MainThreadTarget {}
unsafe impl Sync for MainThreadTarget {}

/// Callback state shared with the plugin.  The plugin may call into this from
/// its main, audio or helper threads, so everything here is atomics or a mutex
/// that callbacks only try to take.
pub(crate) struct HostState {
    main_thread: std::thread::ThreadId,
    target: RwLock<MainThreadTarget>,
    restart_requested: AtomicBool,
    process_requested: AtomicBool,
    callback_requested: AtomicBool,
    params_rescan: AtomicBool,
    flush_requested: AtomicBool,
    gui_closed: AtomicBool,
    gui_resize: AtomicU64,
    timers: Mutex<Vec<TimerEntry>>,
    next_timer: AtomicU32,
    last_log: Mutex<Option<String>>,
}

impl HostState {
    fn new() -> Self {
        Self {
            main_thread: *MAIN_THREAD.get_or_init(|| std::thread::current().id()),
            target: RwLock::new(MainThreadTarget {
                plugin: ptr::null(),
                timer_ext: ptr::null(),
            }),
            restart_requested: AtomicBool::new(false),
            process_requested: AtomicBool::new(false),
            callback_requested: AtomicBool::new(false),
            params_rescan: AtomicBool::new(false),
            flush_requested: AtomicBool::new(false),
            gui_closed: AtomicBool::new(false),
            gui_resize: AtomicU64::new(0),
            timers: Mutex::new(Vec::new()),
            next_timer: AtomicU32::new(1),
            last_log: Mutex::new(None),
        }
    }

    pub(crate) fn take_gui_resize(&self) -> Option<(u32, u32)> {
        let packed = self.gui_resize.swap(0, Ordering::Relaxed);
        if packed == 0 {
            None
        } else {
            Some(((packed >> 32) as u32, packed as u32))
        }
    }

    pub(crate) fn take_gui_closed(&self) -> bool {
        self.gui_closed.swap(false, Ordering::Relaxed)
    }

    /// Runs `on_main_thread` and due timers.  CLAP allows these main-thread
    /// calls while the audio thread is inside `process`, so this does not take
    /// the instance mutex: an editor repainting from its timer can no longer
    /// make the audio callback skip the insert.  Returns true when a parameter
    /// flush or rescan is waiting; those touch the instance and must run
    /// through [`ClapPlugin::service_pending`] under its lock.
    pub(crate) fn service_main_thread(&self) -> bool {
        if let Ok(target) = self.target.read() {
            if !target.plugin.is_null() {
                if self.callback_requested.swap(false, Ordering::Relaxed) {
                    if let Some(on_main_thread) = unsafe { (*target.plugin).on_main_thread } {
                        unsafe { on_main_thread(target.plugin) };
                    }
                }
                self.service_timers(&target);
            }
        }
        self.flush_requested.load(Ordering::Relaxed) || self.params_rescan.load(Ordering::Relaxed)
    }

    fn service_timers(&self, target: &MainThreadTarget) {
        let Some(timers) = (unsafe { target.timer_ext.as_ref() }) else {
            return;
        };
        let Some(on_timer) = timers.on_timer else {
            return;
        };
        let due = {
            let Ok(mut timers) = self.timers.lock() else {
                return;
            };
            let now = Instant::now();
            let mut due = Vec::new();
            for timer in timers.iter_mut() {
                if timer.next <= now {
                    due.push(timer.id);
                    timer.next = now + timer.period;
                }
            }
            due
        };
        for id in due {
            unsafe { on_timer(target.plugin, id) };
        }
    }
}

/// The event list passed to `process` and `params.flush`.  It is kept on the
/// instance so parameter changes do not allocate on the audio thread.
#[derive(Default)]
struct EventBuffer {
    events: Vec<clap_event_param_value>,
    pointers: Vec<*const clap_event_header>,
}

impl EventBuffer {
    fn clear(&mut self) {
        self.events.clear();
        self.pointers.clear();
    }

    fn push_value(&mut self, id: u32, value: f64) {
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

    fn finish(&mut self) {
        self.pointers.clear();
        for event in &self.events {
            self.pointers
                .push(&event.header as *const clap_event_header);
        }
    }

    fn as_clap(&self) -> clap_input_events {
        clap_input_events {
            ctx: self as *const _ as *mut c_void,
            size: Some(input_events_size),
            get: Some(input_events_get),
        }
    }
}

unsafe extern "C" fn input_events_size(list: *const clap_input_events) -> u32 {
    let Some(list) = (unsafe { list.as_ref() }) else {
        return 0;
    };
    let Some(buffer) = (unsafe { (list.ctx as *const EventBuffer).as_ref() }) else {
        return 0;
    };
    buffer.pointers.len() as u32
}

unsafe extern "C" fn input_events_get(
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

unsafe extern "C" fn discard_output_event(
    _list: *const clap_output_events,
    _event: *const clap_event_header,
) -> bool {
    false
}

fn output_events() -> clap_output_events {
    clap_output_events {
        ctx: ptr::null_mut(),
        try_push: Some(discard_output_event),
    }
}

/// A loaded CLAP effect.  The plugin pointer and its library are kept together
/// so the plugin can never outlive the code it came from.
pub(crate) struct ClapPlugin {
    plugin: *const clap_plugin,
    entry: *const clap_plugin_entry,
    library: libloading::Library,
    _host: Box<clap_host>,
    host_state: Arc<HostState>,
    params_ext: *const clap_plugin_params,
    gui_ext: *const clap_plugin_gui,
    timer_ext: *const clap_plugin_timer_support,
    cached_params: Vec<ClapParam>,
    input_buffers: Vec<Vec<f32>>,
    output_buffers: Vec<Vec<f32>>,
    input_pointers: Vec<*mut f32>,
    output_pointers: Vec<*mut f32>,
    dry_l: Vec<f32>,
    dry_r: Vec<f32>,
    events: EventBuffer,
    pending_params: Vec<(u32, f64)>,
    sample_rate: f64,
    max_block: usize,
    steady_time: i64,
    started: bool,
    active: bool,
    editor_open: bool,
    has_editor: bool,
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
    ) -> Result<Self, String> {
        let binary = bundle_binary_path(path);
        let library = unsafe { libloading::Library::new(&binary) }
            .map_err(|error| format!("CLAP load failed: {error}"))?;
        let entry_symbol: libloading::Symbol<'_, *const clap_plugin_entry> = unsafe {
            library
                .get(b"clap_entry\0")
                .map_err(|error| format!("Missing clap_entry symbol: {error}"))?
        };
        let entry = *entry_symbol;
        if !clap_sys::version::clap_version_is_compatible(unsafe { (*entry).clap_version }) {
            return Err("This CLAP plugin uses an incompatible CLAP version.".to_owned());
        }
        let init_path = CString::new(
            entry_init_path(path, &binary)
                .to_string_lossy()
                .into_owned(),
        )
        .map_err(|_| "The CLAP path contains a NUL byte.".to_owned())?;
        let initialised = match unsafe { (*entry).init } {
            Some(init) => unsafe { init(init_path.as_ptr()) },
            None => true,
        };
        if !initialised {
            return Err("The CLAP plugin could not be initialised.".to_owned());
        }

        let host_state = Arc::new(HostState::new());
        let host = Box::new(make_host(&host_state));
        let factory = unsafe {
            (*entry)
                .get_factory
                .map(|get| get(CLAP_PLUGIN_FACTORY_ID.as_ptr()))
                .unwrap_or(ptr::null())
                .cast::<clap_plugin_factory>()
        };
        if factory.is_null() {
            unsafe { (*entry).deinit.map(|deinit| deinit()) };
            return Err("This CLAP bundle has no plugin factory.".to_owned());
        }
        let Some(create) = (unsafe { (*factory).create_plugin }) else {
            unsafe { (*entry).deinit.map(|deinit| deinit()) };
            return Err("The CLAP factory cannot create plugins.".to_owned());
        };
        let id = CString::new(plugin_id)
            .map_err(|_| "The CLAP plugin id contains a NUL byte.".to_owned())?;
        let plugin = unsafe { create(factory, &*host, id.as_ptr()) };
        if plugin.is_null() {
            unsafe { (*entry).deinit.map(|deinit| deinit()) };
            return Err("The CLAP plugin could not be created.".to_owned());
        }
        let Some(init) = (unsafe { (*plugin).init }) else {
            if let Some(destroy) = unsafe { (*plugin).destroy } {
                unsafe { destroy(plugin) };
            }
            unsafe { (*entry).deinit.map(|deinit| deinit()) };
            return Err("The CLAP plugin has no init function.".to_owned());
        };
        if !unsafe { init(plugin) } {
            if let Some(destroy) = unsafe { (*plugin).destroy } {
                unsafe { destroy(plugin) };
            }
            unsafe { (*entry).deinit.map(|deinit| deinit()) };
            return Err("The CLAP plugin refused to start.".to_owned());
        }

        let params_ext = extension::<clap_plugin_params>(plugin, CLAP_EXT_PARAMS);
        let gui_ext = extension::<clap_plugin_gui>(plugin, CLAP_EXT_GUI);
        let timer_ext = extension::<clap_plugin_timer_support>(plugin, CLAP_EXT_TIMER_SUPPORT);
        let audio_ports_ext = extension::<clap_plugin_audio_ports>(plugin, CLAP_EXT_AUDIO_PORTS);

        let (input_channels, output_channels) = audio_channel_counts(plugin, audio_ports_ext)?;
        let rate = if sample_rate == 0 {
            48_000.0
        } else {
            sample_rate as f64
        };
        let block = max_block.max(1).min(MAX_BLOCK);

        let mut plugin = Self {
            plugin,
            entry,
            library,
            _host: host,
            host_state,
            params_ext,
            gui_ext,
            timer_ext,
            cached_params: Vec::new(),
            input_buffers: vec![vec![0.0; block]; input_channels],
            output_buffers: vec![vec![0.0; block]; output_channels],
            input_pointers: vec![ptr::null_mut(); input_channels],
            output_pointers: vec![ptr::null_mut(); output_channels],
            dry_l: vec![0.0; block],
            dry_r: vec![0.0; block],
            events: EventBuffer::default(),
            pending_params: Vec::new(),
            sample_rate: rate,
            max_block: block,
            steady_time: 0,
            started: false,
            active: false,
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
        };
        plugin.activate()?;
        // `start_processing` is an audio-thread call in CLAP; the first
        // `process` makes it (see `process`).
        plugin.refresh_parameters();
        Ok(plugin)
    }

    /// The callback state the main-thread servicing works from.
    pub(crate) fn host(&self) -> Arc<HostState> {
        Arc::clone(&self.host_state)
    }

    pub(crate) fn has_editor(&self) -> bool {
        self.has_editor
    }

    fn activate(&mut self) -> Result<(), String> {
        let Some(activate) = (unsafe { (*self.plugin).activate }) else {
            return Err("The CLAP plugin cannot be activated.".to_owned());
        };
        if !unsafe { activate(self.plugin, self.sample_rate, 1, self.max_block as u32) } {
            return Err("The CLAP plugin refused to activate.".to_owned());
        }
        self.active = true;
        Ok(())
    }

    /// Starts processing.  CLAP marks `start_processing` as an audio-thread
    /// call, so `process` makes it on the first block, inside the audio-thread
    /// scope.
    fn start_processing(&mut self) -> Result<(), String> {
        if self.started {
            return Ok(());
        }
        if let Some(start) = unsafe { (*self.plugin).start_processing } {
            if !unsafe { start(self.plugin) } {
                return Err("The CLAP plugin could not start processing.".to_owned());
            }
        }
        self.started = true;
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

    fn scan_parameters(&mut self) {
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

    fn normalise(&self, param: &ClapParam, value: f64) -> f64 {
        if param.max > param.min {
            ((value - param.min) / (param.max - param.min)).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }

    fn denormalise(&self, param: &ClapParam, value: f64) -> f64 {
        let value = value.clamp(0.0, 1.0);
        let native = param.min + value * (param.max - param.min);
        if param.step_count > 0 {
            native.round().clamp(param.min, param.max)
        } else {
            native
        }
    }

    fn value_text(&self, id: u32, value: f64) -> String {
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
                        if unsafe { get(self.plugin, param.id, &mut value) } {
                            Some(value)
                        } else {
                            None
                        }
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

    fn default_views(&self) -> Vec<PluginParamView> {
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

    fn flush_parameters(&mut self, keep_pending: bool) {
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
    }

    pub(crate) fn process(
        &mut self,
        left: &mut [f32],
        right: &mut [f32],
        frames: usize,
        mix: f32,
    ) -> Result<(), String> {
        if !self.active {
            return Err("The CLAP plugin is not active.".to_owned());
        }
        let _audio_thread = AudioThreadScope::enter();
        if !self.started {
            self.start_processing()?;
        }
        let mix = mix.clamp(0.0, 1.0);
        let frames = frames.min(left.len()).min(right.len());
        if frames == 0 {
            return Ok(());
        }

        let mut offset = 0;
        while offset < frames {
            let count = (frames - offset).min(self.max_block);
            if self.input_buffers.len() == 1 {
                for sample in 0..count {
                    self.input_buffers[0][sample] =
                        0.5 * (left[offset + sample] + right[offset + sample]);
                }
            } else {
                self.input_buffers[0][..count].copy_from_slice(&left[offset..offset + count]);
                self.input_buffers[1][..count].copy_from_slice(&right[offset..offset + count]);
            }
            for channel in &mut self.output_buffers {
                channel[..count].fill(0.0);
            }
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

            for (index, buffer) in self.input_buffers.iter_mut().enumerate() {
                self.input_pointers[index] = buffer.as_mut_ptr();
            }
            for (index, buffer) in self.output_buffers.iter_mut().enumerate() {
                self.output_pointers[index] = buffer.as_mut_ptr();
            }

            let input_audio = clap_audio_buffer {
                data32: self.input_pointers.as_mut_ptr(),
                data64: ptr::null_mut(),
                channel_count: self.input_buffers.len() as u32,
                latency: 0,
                constant_mask: 0,
            };
            let mut output_audio = clap_audio_buffer {
                data32: self.output_pointers.as_mut_ptr(),
                data64: ptr::null_mut(),
                channel_count: self.output_buffers.len() as u32,
                latency: 0,
                constant_mask: 0,
            };
            let input = events.as_clap();
            let output = output_events();
            let process = clap_process {
                steady_time: self.steady_time,
                frames_count: count as u32,
                transport: ptr::null(),
                audio_inputs: &input_audio,
                audio_outputs: &mut output_audio,
                audio_inputs_count: 1,
                audio_outputs_count: 1,
                in_events: &input,
                out_events: &output,
            };
            let status = match unsafe { (*self.plugin).process } {
                Some(process_fn) => unsafe { process_fn(self.plugin, &process) },
                None => CLAP_PROCESS_ERROR,
            };
            self.steady_time = self.steady_time.saturating_add(count as i64);
            self.events = events;

            if status == CLAP_PROCESS_ERROR {
                return Err("The CLAP plugin reported a processing error.".to_owned());
            }

            if self.output_buffers.len() == 1 {
                for sample in 0..count {
                    let wet = self.output_buffers[0][sample];
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
                    let wet_l = self.output_buffers[0][sample];
                    let wet_r = self.output_buffers[1][sample];
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
        Ok(())
    }

    /// Creates the plugin's GUI resources and asks how large its view is.
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

    /// The parameter flush and rescan the plugin asked for.  These read and
    /// write the instance's cached parameters and event list.
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
        unsafe {
            if self.started {
                if let Some(stop) = (*self.plugin).stop_processing {
                    stop(self.plugin);
                }
            }
            if self.active {
                if let Some(deactivate) = (*self.plugin).deactivate {
                    deactivate(self.plugin);
                }
            }
            if let Some(destroy) = (*self.plugin).destroy {
                destroy(self.plugin);
            }
            if let Some(deinit) = (*self.entry).deinit {
                deinit();
            }
        }
        let _ = &self.library;
    }
}

fn make_host(state: &Arc<HostState>) -> clap_host {
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

unsafe fn state<'a>(host: *const clap_host) -> Option<&'a HostState> {
    let host = unsafe { host.as_ref() }?;
    unsafe { (host.host_data as *const HostState).as_ref() }
}

unsafe extern "C" fn host_get_extension(
    host: *const clap_host,
    extension_id: *const c_char,
) -> *const c_void {
    if extension_id.is_null() {
        return ptr::null();
    }
    let id = unsafe { CStr::from_ptr(extension_id) };
    let _ = host;
    if id == CLAP_EXT_THREAD_CHECK {
        &HOST_THREAD_CHECK as *const _ as *const c_void
    } else if id == CLAP_EXT_LOG {
        &HOST_LOG as *const _ as *const c_void
    } else if id == CLAP_EXT_PARAMS {
        &HOST_PARAMS as *const _ as *const c_void
    } else if id == CLAP_EXT_GUI {
        &HOST_GUI as *const _ as *const c_void
    } else if id == CLAP_EXT_AUDIO_PORTS {
        &HOST_AUDIO_PORTS as *const _ as *const c_void
    } else if id == CLAP_EXT_TIMER_SUPPORT {
        &HOST_TIMER_SUPPORT as *const _ as *const c_void
    } else if id == CLAP_EXT_STATE {
        &HOST_STATE as *const _ as *const c_void
    } else if id == CLAP_EXT_LATENCY {
        &HOST_LATENCY as *const _ as *const c_void
    } else {
        ptr::null()
    }
}

unsafe extern "C" fn host_request_restart(host: *const clap_host) {
    if let Some(state) = unsafe { state(host) } {
        state.restart_requested.store(true, Ordering::Relaxed);
    }
}

unsafe extern "C" fn host_request_process(host: *const clap_host) {
    if let Some(state) = unsafe { state(host) } {
        state.process_requested.store(true, Ordering::Relaxed);
    }
}

unsafe extern "C" fn host_request_callback(host: *const clap_host) {
    if let Some(state) = unsafe { state(host) } {
        state.callback_requested.store(true, Ordering::Relaxed);
    }
}

static HOST_THREAD_CHECK: clap_host_thread_check = clap_host_thread_check {
    is_main_thread: Some(host_is_main_thread),
    is_audio_thread: Some(host_is_audio_thread),
};

unsafe extern "C" fn host_is_main_thread(host: *const clap_host) -> bool {
    unsafe { state(host) }
        .map(|state| state.main_thread == std::thread::current().id())
        .unwrap_or(false)
}

unsafe extern "C" fn host_is_audio_thread(_host: *const clap_host) -> bool {
    ON_AUDIO_THREAD.with(|flag| flag.get())
}

static HOST_LOG: clap_host_log = clap_host_log {
    log: Some(host_log),
};

unsafe extern "C" fn host_log(
    host: *const clap_host,
    severity: clap_log_severity,
    message: *const c_char,
) {
    if !(2..=6).contains(&severity) {
        return;
    }
    let Some(state) = (unsafe { state(host) }) else {
        return;
    };
    let text = unsafe { cstring(message) };
    // A plugin may log from its process callback; never wait behind the UI.
    if let Ok(mut last) = state.last_log.try_lock() {
        *last = Some(text);
    }
}

static HOST_PARAMS: clap_host_params = clap_host_params {
    rescan: Some(host_params_rescan),
    clear: Some(host_params_clear),
    request_flush: Some(host_params_request_flush),
};

unsafe extern "C" fn host_params_rescan(host: *const clap_host, _flags: u32) {
    if let Some(state) = unsafe { state(host) } {
        state.params_rescan.store(true, Ordering::Relaxed);
    }
}

unsafe extern "C" fn host_params_clear(host: *const clap_host, _id: u32, _flags: u32) {
    if let Some(state) = unsafe { state(host) } {
        state.params_rescan.store(true, Ordering::Relaxed);
    }
}

unsafe extern "C" fn host_params_request_flush(host: *const clap_host) {
    if let Some(state) = unsafe { state(host) } {
        state.flush_requested.store(true, Ordering::Relaxed);
    }
}

static HOST_GUI: clap_host_gui = clap_host_gui {
    resize_hints_changed: Some(host_gui_resize_hints_changed),
    request_resize: Some(host_gui_request_resize),
    request_show: Some(host_gui_request_show),
    request_hide: Some(host_gui_request_hide),
    closed: Some(host_gui_closed),
};

unsafe extern "C" fn host_gui_resize_hints_changed(_host: *const clap_host) {}

unsafe extern "C" fn host_gui_request_resize(
    host: *const clap_host,
    width: u32,
    height: u32,
) -> bool {
    let Some(state) = (unsafe { state(host) }) else {
        return false;
    };
    let packed = ((width as u64) << 32) | height as u64;
    state.gui_resize.store(packed, Ordering::Relaxed);
    true
}

unsafe extern "C" fn host_gui_request_show(_host: *const clap_host) -> bool {
    true
}

unsafe extern "C" fn host_gui_request_hide(_host: *const clap_host) -> bool {
    true
}

unsafe extern "C" fn host_gui_closed(host: *const clap_host, _was_destroyed: bool) {
    if let Some(state) = unsafe { state(host) } {
        state.gui_closed.store(true, Ordering::Relaxed);
    }
}

static HOST_AUDIO_PORTS: clap_host_audio_ports = clap_host_audio_ports {
    is_rescan_flag_supported: Some(host_audio_ports_flag_supported),
    rescan: Some(host_audio_ports_rescan),
};

unsafe extern "C" fn host_audio_ports_flag_supported(_host: *const clap_host, _flag: u32) -> bool {
    false
}

unsafe extern "C" fn host_audio_ports_rescan(_host: *const clap_host, _flags: u32) {}

static HOST_TIMER_SUPPORT: clap_host_timer_support = clap_host_timer_support {
    register_timer: Some(host_register_timer),
    unregister_timer: Some(host_unregister_timer),
};

unsafe extern "C" fn host_register_timer(
    host: *const clap_host,
    period_ms: u32,
    timer_id: *mut u32,
) -> bool {
    let Some(state) = (unsafe { state(host) }) else {
        return false;
    };
    let Some(timer_id) = (unsafe { timer_id.as_mut() }) else {
        return false;
    };
    let id = state.next_timer.fetch_add(1, Ordering::Relaxed);
    if id == 0 {
        return false;
    }
    let period = Duration::from_millis(period_ms.max(1) as u64);
    // Timer callbacks can run on the audio thread, so never wait for the UI's
    // timer service; a failed registration is a valid CLAP answer.
    if let Ok(mut timers) = state.timers.try_lock() {
        timers.push(TimerEntry {
            id,
            period,
            next: Instant::now() + period,
        });
        *timer_id = id;
        true
    } else {
        false
    }
}

unsafe extern "C" fn host_unregister_timer(host: *const clap_host, timer_id: u32) -> bool {
    let Some(state) = (unsafe { state(host) }) else {
        return false;
    };
    if let Ok(mut timers) = state.timers.try_lock() {
        timers.retain(|timer| timer.id != timer_id);
        true
    } else {
        false
    }
}

static HOST_STATE: clap_host_state = clap_host_state {
    mark_dirty: Some(host_state_mark_dirty),
};

unsafe extern "C" fn host_state_mark_dirty(_host: *const clap_host) {}

static HOST_LATENCY: clap_host_latency = clap_host_latency {
    changed: Some(host_latency_changed),
};

unsafe extern "C" fn host_latency_changed(_host: *const clap_host) {}

fn extension<T>(plugin: *const clap_plugin, id: &CStr) -> *const T {
    let Some(get) = (unsafe { (*plugin).get_extension }) else {
        return ptr::null();
    };
    unsafe { get(plugin, id.as_ptr()).cast::<T>() }
}

fn entry_init_path(bundle: &Path, binary: &Path) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        let _ = binary;
        bundle.to_path_buf()
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = bundle;
        binary.to_path_buf()
    }
}

fn bundle_binary_path(bundle: &Path) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        if bundle.is_dir() {
            let stem = bundle.file_stem().unwrap_or_default();
            return bundle.join("Contents/MacOS").join(stem);
        }
    }
    bundle.to_path_buf()
}

fn audio_channel_counts(
    plugin: *const clap_plugin,
    ports: *const clap_plugin_audio_ports,
) -> Result<(usize, usize), String> {
    let Some(ports) = (unsafe { ports.as_ref() }) else {
        return Ok((2, 2));
    };
    let input = main_port_channels(plugin, ports, true);
    let output = main_port_channels(plugin, ports, false);
    match (input, output) {
        (Some(input), Some(output)) if input >= 1 && output >= 1 && input <= 2 && output <= 2 => {
            Ok((input, output))
        }
        (Some(_), Some(_)) => {
            Err("This CLAP plugin wants more than a stereo input and output.".to_owned())
        }
        _ => Err("This CLAP plugin has no main audio input or output.".to_owned()),
    }
}

fn main_port_channels(
    plugin: *const clap_plugin,
    ports: &clap_plugin_audio_ports,
    is_input: bool,
) -> Option<usize> {
    let count = ports.count?;
    let get = ports.get?;
    let count = unsafe { count(plugin, is_input) };
    let mut fallback = None;
    for index in 0..count {
        let mut info: clap_audio_port_info = unsafe { std::mem::zeroed() };
        if !unsafe { get(plugin, index, is_input, &mut info) } {
            continue;
        }
        let channels = info.channel_count as usize;
        if info.flags & CLAP_AUDIO_PORT_IS_MAIN != 0 {
            return Some(channels);
        }
        fallback.get_or_insert(channels);
    }
    fallback
}

fn gui_supports_embedding(plugin: *const clap_plugin, gui: *const clap_plugin_gui) -> bool {
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

pub(crate) fn fixed_string(buffer: &[c_char]) -> String {
    let bytes: Vec<u8> = buffer
        .iter()
        .take_while(|byte| **byte != 0)
        .map(|byte| *byte as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

unsafe fn cstring(pointer: *const c_char) -> String {
    if pointer.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(pointer) }
            .to_string_lossy()
            .into_owned()
    }
}

/// One discovered CLAP plugin.
#[derive(Clone, Debug)]
pub(crate) struct ClapCandidate {
    pub(crate) path: PathBuf,
    pub(crate) plugin_id: String,
    pub(crate) name: String,
    pub(crate) vendor: String,
    pub(crate) category: String,
}

/// Standard CLAP locations, plus `RIGWERK_CLAP_PATH` for a custom folder.
pub(crate) fn scan_library() -> Vec<ClapCandidate> {
    let mut roots = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        let mut folder = PathBuf::from(home);
        #[cfg(target_os = "macos")]
        folder.push("Library/Audio/Plug-Ins/CLAP");
        #[cfg(target_os = "linux")]
        folder.push(".clap");
        #[cfg(target_os = "windows")]
        folder.push("AppData/Local/Programs/Common/CLAP");
        roots.push(folder);
    }
    #[cfg(target_os = "macos")]
    roots.push(PathBuf::from("/Library/Audio/Plug-Ins/CLAP"));
    #[cfg(target_os = "linux")]
    roots.push(PathBuf::from("/usr/lib/clap"));
    #[cfg(target_os = "windows")]
    {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            roots.push(PathBuf::from(local).join("Programs/Common/CLAP"));
        }
        if let Some(common) = std::env::var_os("CommonProgramFiles") {
            roots.push(PathBuf::from(common).join("CLAP"));
        }
    }
    if let Ok(extra) = std::env::var("CLAP_PATH") {
        roots.extend(std::env::split_paths(&extra));
    }
    if let Ok(extra) = std::env::var("RIGWERK_CLAP_PATH") {
        roots.extend(std::env::split_paths(&extra));
    }

    let mut bundles = Vec::new();
    for root in roots {
        collect_bundles(&root, 0, &mut bundles);
    }
    bundles.sort();
    bundles.dedup();

    let mut found = Vec::new();
    for bundle in bundles {
        match scan_bundle(&bundle) {
            Ok(mut candidates) => found.append(&mut candidates),
            Err(error) => eprintln!("[audio-bridge] skipping {}: {error}", bundle.display()),
        }
    }
    found.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    found.dedup_by(|a, b| {
        a.path == b.path && a.plugin_id == b.plugin_id && a.name.eq_ignore_ascii_case(&b.name)
    });
    found
}

pub(crate) fn scan_bundle(bundle: &Path) -> Result<Vec<ClapCandidate>, String> {
    let binary = bundle_binary_path(bundle);
    let library = unsafe { libloading::Library::new(&binary) }
        .map_err(|error| format!("CLAP load failed: {error}"))?;
    let entry_symbol: libloading::Symbol<'_, *const clap_plugin_entry> = unsafe {
        library
            .get(b"clap_entry\0")
            .map_err(|error| format!("Missing clap_entry symbol: {error}"))?
    };
    let entry = *entry_symbol;
    if !clap_sys::version::clap_version_is_compatible(unsafe { (*entry).clap_version }) {
        return Err("incompatible CLAP version".to_owned());
    }
    let init_path = CString::new(
        entry_init_path(bundle, &binary)
            .to_string_lossy()
            .into_owned(),
    )
    .map_err(|_| "path contains a NUL byte".to_owned())?;
    let initialised = match unsafe { (*entry).init } {
        Some(init) => unsafe { init(init_path.as_ptr()) },
        None => true,
    };
    if !initialised {
        return Err("init failed".to_owned());
    }
    let result: Result<Vec<ClapCandidate>, String> = (|| {
        let factory = unsafe {
            (*entry)
                .get_factory
                .map(|get| get(CLAP_PLUGIN_FACTORY_ID.as_ptr()))
                .unwrap_or(ptr::null())
                .cast::<clap_plugin_factory>()
        };
        if factory.is_null() {
            return Ok(Vec::new());
        }
        let count = unsafe { (*factory).get_plugin_count }
            .map(|get| unsafe { get(factory) })
            .unwrap_or(0);
        let mut found = Vec::new();
        for index in 0..count {
            let Some(get) = (unsafe { (*factory).get_plugin_descriptor }) else {
                break;
            };
            let descriptor = unsafe { get(factory, index) };
            let Some(descriptor) = (unsafe { descriptor.as_ref() }) else {
                continue;
            };
            let Some(candidate) = candidate_from_descriptor(bundle, descriptor) else {
                continue;
            };
            found.push(candidate);
        }
        Ok(found)
    })();
    unsafe { (*entry).deinit.map(|deinit| deinit()) };
    result
}

fn candidate_from_descriptor(
    bundle: &Path,
    descriptor: &clap_plugin_descriptor,
) -> Option<ClapCandidate> {
    let features = descriptor_features(descriptor);
    if features.iter().any(|feature| {
        feature.eq_ignore_ascii_case("instrument") || feature.eq_ignore_ascii_case("note-effect")
    }) {
        return None;
    }
    let id = unsafe { cstring(descriptor.id) };
    if id.trim().is_empty() {
        return None;
    }
    let name = unsafe { cstring(descriptor.name) };
    let name = if name.trim().is_empty() {
        bundle
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("CLAP plugin")
            .to_owned()
    } else {
        name
    };
    let vendor = unsafe { cstring(descriptor.vendor) };
    let category = if features.is_empty() {
        "CLAP effect".to_owned()
    } else {
        features.join(" · ")
    };
    Some(ClapCandidate {
        path: bundle.to_path_buf(),
        plugin_id: id,
        name,
        vendor,
        category,
    })
}

fn descriptor_features(descriptor: &clap_plugin_descriptor) -> Vec<String> {
    let mut features = Vec::new();
    if descriptor.features.is_null() {
        return features;
    }
    for index in 0..64 {
        let feature = unsafe { *descriptor.features.add(index) };
        if feature.is_null() {
            break;
        }
        features.push(unsafe { cstring(feature) });
    }
    features
}

fn collect_bundles(folder: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 4 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(folder) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_clap = path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("clap"));
        if is_clap {
            out.push(path);
        } else if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            collect_bundles(&path, depth + 1, out);
        }
    }
}

#[cfg(test)]
mod tests {
    /// Optional smoke test used while developing the host: point
    /// `RIGWERK_CLAP_FIXTURE` at a `.clap` file, then run `cargo test`.
    #[test]
    fn loads_a_real_clap_effect() {
        use super::*;
        let Ok(path) = std::env::var("RIGWERK_CLAP_FIXTURE") else {
            return;
        };
        let path = PathBuf::from(path);
        let candidates = scan_bundle(&path).expect("scan the fixture");
        assert!(!candidates.is_empty(), "fixture exposed no effects");
        let candidate = &candidates[0];
        let mut plugin =
            ClapPlugin::load(&path, &candidate.plugin_id, 48_000, 512).expect("load the fixture");
        assert!(!plugin.parameters().is_empty());
        if plugin.has_editor() {
            let (width, height) = plugin
                .prepare_editor()
                .expect("create the fixture interface");
            assert!(width > 0 && height > 0);
            plugin.close_editor();
        }
        plugin.set_parameter(1, 0.5).expect("set the fixture gain");
        let mut left = vec![0.25f32; 128];
        let mut right = vec![-0.25f32; 128];
        plugin
            .process(&mut left, &mut right, 128, 1.0)
            .expect("process a block");
        assert!(left.iter().any(|sample| (*sample - 0.125).abs() < 0.001));
    }
}
