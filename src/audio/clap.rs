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
use clap_sys::ext::state::{clap_host_state, clap_plugin_state, CLAP_EXT_STATE};
use clap_sys::ext::thread_check::{clap_host_thread_check, CLAP_EXT_THREAD_CHECK};
use clap_sys::ext::timer_support::{
    clap_host_timer_support, clap_plugin_timer_support, CLAP_EXT_TIMER_SUPPORT,
};
use clap_sys::factory::plugin_factory::{clap_plugin_factory, CLAP_PLUGIN_FACTORY_ID};
use clap_sys::host::clap_host;
use clap_sys::plugin::{clap_plugin, clap_plugin_descriptor};
use clap_sys::process::{clap_process, CLAP_PROCESS_ERROR};
use clap_sys::stream::{clap_istream, clap_ostream};
use clap_sys::string_sizes::CLAP_NAME_SIZE;
use clap_sys::version::CLAP_VERSION;

use crate::audio::plugins::{PluginParamView, Restore};

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
    state_ext: *const clap_plugin_state,
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
    /// Set by `clap_host_state.mark_dirty`.
    state_dirty: AtomicBool,
    /// Mirrors whether the instance holds parameter values the plugin has
    /// not received yet, so a state save can be deferred without taking the
    /// instance lock.
    params_pending: AtomicBool,
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
                state_ext: ptr::null(),
            }),
            restart_requested: AtomicBool::new(false),
            process_requested: AtomicBool::new(false),
            callback_requested: AtomicBool::new(false),
            params_rescan: AtomicBool::new(false),
            flush_requested: AtomicBool::new(false),
            gui_closed: AtomicBool::new(false),
            gui_resize: AtomicU64::new(0),
            state_dirty: AtomicBool::new(false),
            params_pending: AtomicBool::new(false),
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

    /// Whether the plugin reported state changes since the last save.
    pub(crate) fn state_dirty(&self) -> bool {
        self.state_dirty.load(Ordering::Relaxed)
    }

    /// Saves the plugin's state through `clap_plugin_state.save`.  That is a
    /// main-thread call which CLAP allows while the audio thread processes,
    /// so it runs under the target's read lock instead of the instance mutex
    /// (the audio callback keeps the insert).
    ///
    /// While parameter values are still queued for the next block the
    /// plugin's state would hold the old values, so the save is deferred.
    pub(crate) fn save_state(&self) -> StateSave {
        if self.params_pending.load(Ordering::Acquire) {
            return StateSave::Deferred;
        }
        if std::thread::current().id() != self.main_thread {
            return StateSave::Deferred;
        }
        let target = self
            .target
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if target.plugin.is_null() {
            return StateSave::Unavailable;
        }
        let Some(save) = (unsafe { target.state_ext.as_ref() }).and_then(|state| state.save) else {
            return StateSave::Unavailable;
        };
        // Cleared first, so a change the plugin marks during the save is
        // picked up by the next one; set again if the save fails.
        self.state_dirty.store(false, Ordering::Relaxed);
        let mut writer = StateWriter::default();
        let stream = writer.stream();
        let saved = unsafe { save(target.plugin, &stream) };
        if writer.overflowed {
            eprintln!(
                "[plugins] not saving a CLAP plugin's state: it is larger than {} MB",
                MAX_STATE_BYTES / (1024 * 1024)
            );
        }
        if saved && !writer.overflowed {
            StateSave::Saved(writer.bytes)
        } else {
            self.state_dirty.store(true, Ordering::Relaxed);
            StateSave::Unavailable
        }
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

/// The outcome of [`HostState::save_state`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StateSave {
    Saved(Vec<u8>),
    /// No state extension, or the save failed.
    Unavailable,
    /// Not now (values still queued, or not on the main thread); try again
    /// at the next save.
    Deferred,
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

/// One successful `clap_plugin_entry.init`.  Dropping it calls `deinit`
/// exactly once, so every path out of a load or scan that got past `init`
/// undoes it.  It must be dropped after every plugin created through the entry
/// is destroyed and before the library is unloaded.
///
/// This is the one place the module's init/deinit pairing lives, so a
/// per-library registry that shares a single init between instances and
/// scans can hand out references to it instead.
struct EntryInit {
    entry: *const clap_plugin_entry,
}

impl EntryInit {
    /// Calls the entry's `init` (an entry without one counts as initialised).
    /// `None` when the module refused; nothing needs undoing then.
    ///
    /// # Safety
    /// `entry` must point to a valid CLAP entry whose library stays loaded
    /// until the returned guard is dropped.
    unsafe fn init(entry: *const clap_plugin_entry, path: &CStr) -> Option<Self> {
        let initialised = match unsafe { (*entry).init } {
            Some(init) => unsafe { init(path.as_ptr()) },
            None => true,
        };
        // Not `then_some`: that would build (and drop, so `deinit`) the guard
        // even when `init` failed.
        if initialised {
            Some(Self { entry })
        } else {
            None
        }
    }

    fn entry(&self) -> &clap_plugin_entry {
        unsafe { &*self.entry }
    }

    /// The entry's plugin factory, or null.
    fn plugin_factory(&self) -> *const clap_plugin_factory {
        self.entry()
            .get_factory
            .map(|get| unsafe { get(CLAP_PLUGIN_FACTORY_ID.as_ptr()) })
            .unwrap_or(ptr::null())
            .cast::<clap_plugin_factory>()
    }
}

impl Drop for EntryInit {
    fn drop(&mut self) {
        if let Some(deinit) = self.entry().deinit {
            unsafe { deinit() };
        }
    }
}

/// A plugin instance `create_plugin` returned.  Dropping it deactivates the
/// plugin if it was activated and then destroys it, exactly once, whether or
/// not `init` ran or succeeded (CLAP allows `destroy` after a failed `init`).
struct PluginInstance {
    plugin: *const clap_plugin,
    active: bool,
}

impl PluginInstance {
    /// # Safety
    /// `plugin` must be a non-null instance from `create_plugin` that nothing
    /// else destroys.
    unsafe fn new(plugin: *const clap_plugin) -> Self {
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

/// A loaded CLAP effect.  The plugin pointer and its library are kept together
/// so the plugin can never outlive the code it came from.
pub(crate) struct ClapPlugin {
    // Fields drop in declaration order, which is the teardown order CLAP
    // needs: destroy the instance, `deinit` the entry, free the host the
    // plugin pointed at, and only then unload the library.  These five must
    // stay first and in this order.
    instance: PluginInstance,
    _entry: EntryInit,
    _host: Box<clap_host>,
    host_state: Arc<HostState>,
    _library: libloading::Library,
    /// `instance.plugin`, copied for brevity; owned by `instance`.
    plugin: *const clap_plugin,
    params_ext: *const clap_plugin_params,
    gui_ext: *const clap_plugin_gui,
    timer_ext: *const clap_plugin_timer_support,
    state_ext: *const clap_plugin_state,
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
        restore: &Restore<'_>,
    ) -> Result<(Self, bool), String> {
        let binary = bundle_binary_path(path);
        let library = unsafe { libloading::Library::new(&binary) }
            .map_err(|error| format!("CLAP load failed: {error}"))?;
        let entry_symbol: libloading::Symbol<'_, *const clap_plugin_entry> = unsafe {
            library
                .get(b"clap_entry\0")
                .map_err(|error| format!("Missing clap_entry symbol: {error}"))?
        };
        let entry = *entry_symbol;
        let init_path = CString::new(
            entry_init_path(path, &binary)
                .to_string_lossy()
                .into_owned(),
        )
        .map_err(|_| "The CLAP path contains a NUL byte.".to_owned())?;
        // SAFETY: `entry` comes from `library`, which `instantiate` keeps
        // loaded for as long as anything it creates from the entry.
        unsafe {
            Self::instantiate(
                library,
                entry,
                &init_path,
                plugin_id,
                sample_rate,
                max_block,
                restore,
            )
        }
    }

    /// Initialises the entry and creates, sets up and activates one plugin.
    /// Every step after `init` may fail; the guards then tear down whatever
    /// was set up, in order, before `library` is dropped.
    ///
    /// # Safety
    /// `entry` must be a valid CLAP entry inside `library`.
    unsafe fn instantiate(
        library: libloading::Library,
        entry: *const clap_plugin_entry,
        init_path: &CStr,
        plugin_id: &str,
        sample_rate: u32,
        max_block: usize,
        restore: &Restore<'_>,
    ) -> Result<(Self, bool), String> {
        if !clap_sys::version::clap_version_is_compatible(unsafe { (*entry).clap_version }) {
            return Err("This CLAP plugin uses an incompatible CLAP version.".to_owned());
        }
        // The host is created before the guards, so an early return destroys
        // the plugin, then `deinit`s, then frees the host (locals drop in
        // reverse order; `library`, a parameter, goes last).
        let host_state = Arc::new(HostState::new());
        let host = Box::new(make_host(&host_state));
        let Some(entry_init) = (unsafe { EntryInit::init(entry, init_path) }) else {
            return Err("The CLAP plugin could not be initialised.".to_owned());
        };
        let factory = entry_init.plugin_factory();
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

        let (input_channels, output_channels) = audio_channel_counts(plugin, audio_ports_ext)?;
        let rate = if sample_rate == 0 {
            48_000.0
        } else {
            sample_rate as f64
        };
        let block = max_block.max(1).min(MAX_BLOCK);

        let mut plugin = Self {
            instance,
            _entry: entry_init,
            _host: host,
            host_state,
            _library: library,
            plugin,
            params_ext,
            gui_ext,
            timer_ext,
            state_ext,
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
        // restored values below.  They run on whichever thread loads the
        // plugin, like `init` and `activate` (see REVIEW.md H8).
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
        plugin.activate()?;
        // `start_processing` is an audio-thread call in CLAP; the first
        // `process` makes it (see `process`).
        plugin.refresh_parameters();
        Ok((plugin, state_loaded && values.is_empty()))
    }

    /// Hands saved state bytes to `clap_plugin_state.load`.
    fn load_state(&mut self, bytes: &[u8]) -> bool {
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
    fn restore_parameters(&mut self, values: &[(u32, Option<f64>)]) {
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
    fn note_pending(&self) {
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

    fn activate(&mut self) -> Result<(), String> {
        let Some(activate) = (unsafe { (*self.plugin).activate }) else {
            return Err("The CLAP plugin cannot be activated.".to_owned());
        };
        if !unsafe { activate(self.plugin, self.sample_rate, 1, self.max_block as u32) } {
            return Err("The CLAP plugin refused to activate.".to_owned());
        }
        self.instance.active = true;
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
        self.note_pending();
    }

    pub(crate) fn process(
        &mut self,
        left: &mut [f32],
        right: &mut [f32],
        frames: usize,
        mix: f32,
    ) -> Result<(), String> {
        if !self.instance.active {
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
            // Only now has the plugin received the queued values.
            self.note_pending();

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
        if self.started {
            if let Some(stop) = unsafe { (*self.plugin).stop_processing } {
                unsafe { stop(self.plugin) };
            }
        }
        // The fields finish the teardown: `instance` deactivates and destroys
        // the plugin, `_entry` deinits the module, then the host and the
        // library go (see the field order on the struct).
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

unsafe extern "C" fn host_state_mark_dirty(host: *const clap_host) {
    if let Some(state) = unsafe { state(host) } {
        state.state_dirty.store(true, Ordering::Relaxed);
    }
}

/// The most state bytes accepted from one plugin, so a runaway `save` cannot
/// exhaust memory.
const MAX_STATE_BYTES: usize = 8 * 1024 * 1024;

/// Collects what a plugin writes in `clap_plugin_state.save`.
#[derive(Default)]
struct StateWriter {
    bytes: Vec<u8>,
    overflowed: bool,
}

impl StateWriter {
    /// The stream handed to the plugin.  It points at `self`, so `self` must
    /// stay in place while the plugin uses it.
    fn stream(&mut self) -> clap_ostream {
        clap_ostream {
            ctx: self as *mut Self as *mut c_void,
            write: Some(state_write),
        }
    }
}

unsafe extern "C" fn state_write(
    stream: *const clap_ostream,
    buffer: *const c_void,
    size: u64,
) -> i64 {
    let Some(stream) = (unsafe { stream.as_ref() }) else {
        return -1;
    };
    let Some(writer) = (unsafe { (stream.ctx as *mut StateWriter).as_mut() }) else {
        return -1;
    };
    if size == 0 {
        return 0;
    }
    if buffer.is_null() {
        return -1;
    }
    let size = match usize::try_from(size) {
        Ok(size) if size <= MAX_STATE_BYTES - writer.bytes.len().min(MAX_STATE_BYTES) => size,
        _ => {
            writer.overflowed = true;
            return -1;
        }
    };
    let data = unsafe { std::slice::from_raw_parts(buffer as *const u8, size) };
    writer.bytes.extend_from_slice(data);
    size as i64
}

/// Feeds saved bytes to `clap_plugin_state.load`.
struct StateReader<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> StateReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    /// The stream handed to the plugin.  It points at `self`, so `self` must
    /// stay in place while the plugin uses it.
    fn stream(&mut self) -> clap_istream {
        clap_istream {
            ctx: self as *mut Self as *mut c_void,
            read: Some(state_read),
        }
    }
}

/// Copies up to `size` bytes; returns how many, 0 at the end, -1 on error.
unsafe extern "C" fn state_read(
    stream: *const clap_istream,
    buffer: *mut c_void,
    size: u64,
) -> i64 {
    let Some(stream) = (unsafe { stream.as_ref() }) else {
        return -1;
    };
    let Some(reader) = (unsafe { (stream.ctx as *mut StateReader<'_>).as_mut() }) else {
        return -1;
    };
    let remaining = &reader.data[reader.position..];
    let count = usize::try_from(size)
        .unwrap_or(usize::MAX)
        .min(remaining.len())
        .min(i64::MAX as usize);
    if count == 0 {
        return 0;
    }
    if buffer.is_null() {
        return -1;
    }
    unsafe { ptr::copy_nonoverlapping(remaining.as_ptr(), buffer as *mut u8, count) };
    reader.position += count;
    count as i64
}

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
    // SAFETY: `library` outlives the guard (it is declared first, so it is
    // dropped last).
    let Some(entry_init) = (unsafe { EntryInit::init(entry, &init_path) }) else {
        return Err("init failed".to_owned());
    };
    let result: Result<Vec<ClapCandidate>, String> = (|| {
        let factory = entry_init.plugin_factory();
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
    drop(entry_init);
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
    use super::{HostState, StateReader, StateSave, StateWriter};
    use std::ffi::c_void;
    use std::sync::atomic::Ordering;

    #[test]
    fn state_save_waits_while_values_are_queued() {
        let mut host = HostState::new();
        // On the main thread with no plugin, a save would report
        // `Unavailable`; only the queued values make it wait.
        host.main_thread = std::thread::current().id();
        assert_eq!(host.save_state(), StateSave::Unavailable);
        host.params_pending.store(true, Ordering::Release);
        assert_eq!(host.save_state(), StateSave::Deferred);
    }

    #[test]
    fn state_writer_collects_every_chunk() {
        let mut writer = StateWriter::default();
        let stream = writer.stream();
        let write = stream.write.expect("write callback");
        let first = [1u8, 2, 3];
        let second = [4u8; 5];
        unsafe {
            assert_eq!(write(&stream, first.as_ptr() as *const c_void, 3), 3);
            assert_eq!(write(&stream, second.as_ptr() as *const c_void, 5), 5);
            assert_eq!(write(&stream, std::ptr::null(), 0), 0);
            assert_eq!(write(&stream, std::ptr::null(), 4), -1);
            assert_eq!(
                write(std::ptr::null(), first.as_ptr() as *const c_void, 3),
                -1
            );
        }
        assert!(!writer.overflowed);
        assert_eq!(writer.bytes, vec![1, 2, 3, 4, 4, 4, 4, 4]);
    }

    #[test]
    fn state_writer_refuses_runaway_sizes() {
        let mut writer = StateWriter::default();
        let stream = writer.stream();
        let byte = [0u8];
        let result = unsafe {
            (stream.write.expect("write"))(&stream, byte.as_ptr() as *const c_void, u64::MAX)
        };
        assert_eq!(result, -1);
        assert!(writer.overflowed);
        assert!(writer.bytes.is_empty());
    }

    #[test]
    fn state_reader_hands_out_the_bytes_in_pieces() {
        let data: Vec<u8> = (0..10).collect();
        let mut reader = StateReader::new(&data);
        let stream = reader.stream();
        let read = stream.read.expect("read callback");
        let mut out = Vec::new();
        let mut chunk = [0u8; 4];
        loop {
            let count = unsafe { read(&stream, chunk.as_mut_ptr() as *mut c_void, 4) };
            assert!(count >= 0);
            if count == 0 {
                break;
            }
            out.extend_from_slice(&chunk[..count as usize]);
        }
        assert_eq!(out, data);
        // At the end it keeps reporting end of stream.
        assert_eq!(
            unsafe { read(&stream, chunk.as_mut_ptr() as *mut c_void, 4) },
            0
        );
        assert_eq!(
            unsafe { read(std::ptr::null(), chunk.as_mut_ptr() as *mut c_void, 4) },
            -1
        );
    }

    #[test]
    fn state_round_trips_through_both_streams() {
        let mut writer = StateWriter::default();
        let ostream = writer.stream();
        let payload = b"plugin state";
        let written = unsafe {
            (ostream.write.expect("write"))(&ostream, payload.as_ptr() as *const c_void, 12)
        };
        assert_eq!(written, 12);
        let saved = std::mem::take(&mut writer.bytes);
        let mut reader = StateReader::new(&saved);
        let istream = reader.stream();
        let mut back = [0u8; 64];
        let count = unsafe {
            (istream.read.expect("read"))(&istream, back.as_mut_ptr() as *mut c_void, 64)
        };
        assert_eq!(&back[..count as usize], payload);
    }

    /// A fake CLAP module driven from Rust: every entry and plugin callback
    /// logs itself, and `Fail` picks the step that goes wrong.  The callbacks
    /// all run on the test thread, so the log and the setting are
    /// thread-local and the tests can run in parallel.
    mod fake {
        use super::super::*;
        use std::cell::RefCell;

        #[derive(Clone, Copy, PartialEq, Debug)]
        pub(super) enum Fail {
            Nothing,
            EntryInit,
            NoFactory,
            NoCreate,
            CreateNull,
            NoPluginInit,
            PluginInit,
            TooManyChannels,
            Activate,
        }

        thread_local! {
            static FAIL: Cell<Fail> = const { Cell::new(Fail::Nothing) };
            static LOG: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
        }

        fn log(event: &'static str) {
            LOG.with(|log| log.borrow_mut().push(event));
        }

        fn failing(step: Fail) -> bool {
            FAIL.with(Cell::get) == step
        }

        pub(super) fn reset(step: Fail) {
            FAIL.with(|fail| fail.set(step));
            LOG.with(|log| log.borrow_mut().clear());
        }

        pub(super) fn events() -> Vec<&'static str> {
            LOG.with(|log| log.borrow().clone())
        }

        pub(super) fn count(event: &str) -> usize {
            events().iter().filter(|logged| **logged == event).count()
        }

        unsafe extern "C" fn entry_init(_path: *const c_char) -> bool {
            log("entry.init");
            !failing(Fail::EntryInit)
        }

        unsafe extern "C" fn entry_deinit() {
            log("entry.deinit");
        }

        unsafe extern "C" fn entry_get_factory(_id: *const c_char) -> *const c_void {
            match FAIL.with(Cell::get) {
                Fail::NoFactory => ptr::null(),
                Fail::NoCreate => (&FACTORY_WITHOUT_CREATE as *const clap_plugin_factory).cast(),
                _ => (&FACTORY as *const clap_plugin_factory).cast(),
            }
        }

        unsafe extern "C" fn factory_create(
            _factory: *const clap_plugin_factory,
            host: *const clap_host,
            _id: *const c_char,
        ) -> *const clap_plugin {
            log("create");
            assert!(!host.is_null());
            if failing(Fail::CreateNull) {
                return ptr::null();
            }
            let init = if failing(Fail::NoPluginInit) {
                None
            } else {
                Some(plugin_init as unsafe extern "C" fn(*const clap_plugin) -> bool)
            };
            Box::into_raw(Box::new(clap_plugin {
                desc: ptr::null(),
                plugin_data: ptr::null_mut(),
                init,
                destroy: Some(plugin_destroy),
                activate: Some(plugin_activate),
                deactivate: Some(plugin_deactivate),
                start_processing: None,
                stop_processing: None,
                reset: None,
                process: None,
                get_extension: Some(plugin_get_extension),
                on_main_thread: None,
            }))
        }

        unsafe extern "C" fn plugin_init(_plugin: *const clap_plugin) -> bool {
            log("plugin.init");
            !failing(Fail::PluginInit)
        }

        unsafe extern "C" fn plugin_destroy(plugin: *const clap_plugin) {
            log("destroy");
            drop(unsafe { Box::from_raw(plugin as *mut clap_plugin) });
        }

        unsafe extern "C" fn plugin_activate(
            _plugin: *const clap_plugin,
            _rate: f64,
            _min: u32,
            _max: u32,
        ) -> bool {
            log("activate");
            !failing(Fail::Activate)
        }

        unsafe extern "C" fn plugin_deactivate(_plugin: *const clap_plugin) {
            log("deactivate");
        }

        unsafe extern "C" fn plugin_get_extension(
            _plugin: *const clap_plugin,
            id: *const c_char,
        ) -> *const c_void {
            let id = unsafe { CStr::from_ptr(id) };
            if id == CLAP_EXT_AUDIO_PORTS && failing(Fail::TooManyChannels) {
                return (&PORTS as *const clap_plugin_audio_ports).cast();
            }
            ptr::null()
        }

        unsafe extern "C" fn ports_count(_plugin: *const clap_plugin, _is_input: bool) -> u32 {
            1
        }

        unsafe extern "C" fn ports_get(
            _plugin: *const clap_plugin,
            _index: u32,
            _is_input: bool,
            info: *mut clap_audio_port_info,
        ) -> bool {
            unsafe {
                (*info).channel_count = 6;
                (*info).flags = CLAP_AUDIO_PORT_IS_MAIN;
            }
            true
        }

        static PORTS: clap_plugin_audio_ports = clap_plugin_audio_ports {
            count: Some(ports_count),
            get: Some(ports_get),
        };

        static FACTORY: clap_plugin_factory = clap_plugin_factory {
            get_plugin_count: None,
            get_plugin_descriptor: None,
            create_plugin: Some(factory_create),
        };

        static FACTORY_WITHOUT_CREATE: clap_plugin_factory = clap_plugin_factory {
            get_plugin_count: None,
            get_plugin_descriptor: None,
            create_plugin: None,
        };

        pub(super) static ENTRY: clap_plugin_entry = clap_plugin_entry {
            clap_version: CLAP_VERSION,
            init: Some(entry_init),
            deinit: Some(entry_deinit),
            get_factory: Some(entry_get_factory),
        };

        /// A handle on the test executable itself, standing in for the
        /// plugin library (unloading it only drops a reference).
        pub(super) fn library() -> libloading::Library {
            #[cfg(windows)]
            {
                libloading::os::windows::Library::this()
                    .expect("open the test executable")
                    .into()
            }
            #[cfg(unix)]
            {
                libloading::os::unix::Library::this().into()
            }
        }

        /// Runs the real load sequence against the fake module.
        pub(super) fn load(plugin_id: &str) -> Result<ClapPlugin, String> {
            let pref = crate::audio::PluginPref::default();
            let restore = Restore::from_pref(&pref);
            unsafe {
                ClapPlugin::instantiate(
                    library(),
                    &ENTRY,
                    c"fake.clap",
                    plugin_id,
                    48_000,
                    256,
                    &restore,
                )
            }
            .map(|(plugin, _)| plugin)
        }
    }

    #[test]
    fn a_refused_entry_init_is_not_deinitialised() {
        fake::reset(fake::Fail::EntryInit);
        assert!(fake::load("fake").is_err());
        assert_eq!(fake::events(), ["entry.init"]);
    }

    #[test]
    fn failures_before_create_deinit_the_entry_once() {
        for step in [fake::Fail::NoFactory, fake::Fail::NoCreate] {
            fake::reset(step);
            assert!(fake::load("fake").is_err(), "{step:?}");
            assert_eq!(fake::events(), ["entry.init", "entry.deinit"], "{step:?}");
        }
        // A plugin id with a NUL byte fails after `init` too.
        fake::reset(fake::Fail::Nothing);
        assert!(fake::load("fa\0ke").is_err());
        assert_eq!(fake::events(), ["entry.init", "entry.deinit"]);
        // So does a factory that creates nothing.
        fake::reset(fake::Fail::CreateNull);
        assert!(fake::load("fake").is_err());
        assert_eq!(fake::events(), ["entry.init", "create", "entry.deinit"]);
    }

    #[test]
    fn failures_after_create_destroy_then_deinit_once() {
        let cases: [(fake::Fail, &[&str]); 4] = [
            (
                fake::Fail::NoPluginInit,
                &["entry.init", "create", "destroy", "entry.deinit"],
            ),
            (
                fake::Fail::PluginInit,
                &[
                    "entry.init",
                    "create",
                    "plugin.init",
                    "destroy",
                    "entry.deinit",
                ],
            ),
            (
                fake::Fail::TooManyChannels,
                &[
                    "entry.init",
                    "create",
                    "plugin.init",
                    "destroy",
                    "entry.deinit",
                ],
            ),
            (
                fake::Fail::Activate,
                &[
                    "entry.init",
                    "create",
                    "plugin.init",
                    "activate",
                    "destroy",
                    "entry.deinit",
                ],
            ),
        ];
        for (step, expected) in cases {
            fake::reset(step);
            assert!(fake::load("fake").is_err(), "{step:?}");
            assert_eq!(fake::events(), expected, "{step:?}");
        }
    }

    #[test]
    fn a_loaded_plugin_tears_down_once_in_order() {
        fake::reset(fake::Fail::Nothing);
        let plugin = fake::load("fake").expect("load the fake plugin");
        assert_eq!(fake::count("destroy"), 0);
        assert_eq!(fake::count("entry.deinit"), 0);
        drop(plugin);
        assert_eq!(
            fake::events(),
            [
                "entry.init",
                "create",
                "plugin.init",
                "activate",
                "deactivate",
                "destroy",
                "entry.deinit",
            ]
        );
    }

    #[test]
    fn the_entry_guard_deinits_exactly_once() {
        fake::reset(fake::Fail::Nothing);
        let guard = unsafe { super::EntryInit::init(&fake::ENTRY, c"fake.clap") }
            .expect("the fake entry initialises");
        assert!(!guard.plugin_factory().is_null());
        assert_eq!(fake::count("entry.deinit"), 0);
        drop(guard);
        assert_eq!(fake::events(), ["entry.init", "entry.deinit"]);
    }

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
        let mut plugin = ClapPlugin::load_fresh(&path, &candidate.plugin_id, 48_000, 512)
            .expect("load the fixture");
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
