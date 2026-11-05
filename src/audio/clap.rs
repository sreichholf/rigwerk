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
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use clap_sys::audio_buffer::clap_audio_buffer;
use clap_sys::entry::clap_plugin_entry;
use clap_sys::events::{
    clap_event_header, clap_event_param_value, clap_input_events, clap_output_events,
    CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_PARAM_VALUE,
};
use clap_sys::ext::audio_ports::{
    clap_audio_port_info, clap_host_audio_ports, clap_plugin_audio_ports,
    CLAP_AUDIO_PORTS_RESCAN_CHANNEL_COUNT, CLAP_AUDIO_PORTS_RESCAN_FLAGS,
    CLAP_AUDIO_PORTS_RESCAN_IN_PLACE_PAIR, CLAP_AUDIO_PORTS_RESCAN_LIST,
    CLAP_AUDIO_PORTS_RESCAN_NAMES, CLAP_AUDIO_PORTS_RESCAN_PORT_TYPE, CLAP_AUDIO_PORT_IS_MAIN,
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

/// The UI thread, recorded by [`mark_main_thread`] before any plugin loads.
/// [`HostState`] copies the id when a plugin is created.  The audio callback
/// uses that copy; it does not lock this.
static MAIN_THREAD: RwLock<Option<std::thread::ThreadId>> = RwLock::new(None);

fn recorded_main_thread() -> Option<std::thread::ThreadId> {
    match MAIN_THREAD.read() {
        Ok(slot) => *slot,
        Err(poison) => *poison.into_inner(),
    }
}

/// Records the calling thread as the CLAP main thread.  Call it from the UI
/// thread before any plugin is loaded.  A later call does not move it.
pub(crate) fn mark_main_thread() {
    let mut slot = MAIN_THREAD
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if slot.is_none() {
        *slot = Some(std::thread::current().id());
    }
}

/// Whether this thread is the recorded UI/CLAP main thread.
/// False until [`mark_main_thread`] has run.
pub(crate) fn is_main_thread() -> bool {
    recorded_main_thread().is_some_and(|id| id == std::thread::current().id())
}

/// Whether plugin instances may be destroyed on this thread.
/// The recorded UI thread may.  Before any thread is recorded, the caller
/// may too, so startup and tests do not leak.  Any other thread must hand
/// the instances to the deferred queue and let the UI thread drop them.
pub(crate) fn may_destroy_plugins_here() -> bool {
    match recorded_main_thread() {
        Some(id) => id == std::thread::current().id(),
        None => true,
    }
}

/// Points the recorded main thread at the caller for the guard's lifetime.
/// Production records the UI thread once; tests use this so a case can run
/// on its own thread without stealing that id from a later test.
#[cfg(test)]
pub(crate) struct MainThreadClaim {
    previous: Option<std::thread::ThreadId>,
}

#[cfg(test)]
pub(crate) fn claim_main_thread_for_test() -> MainThreadClaim {
    let mut slot = MAIN_THREAD
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let previous = *slot;
    *slot = Some(std::thread::current().id());
    MainThreadClaim { previous }
}

#[cfg(test)]
impl Drop for MainThreadClaim {
    fn drop(&mut self) {
        let mut slot = MAIN_THREAD
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *slot = self.previous;
    }
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
    /// Set by `clap_host_audio_ports.rescan` once the plugin is set up: its
    /// ports no longer match the buffers, so it needs loading again.
    ports_changed: AtomicBool,
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
            // Prefer the thread `mark_main_thread` recorded.  Do not publish
            // the loader's thread: a plugin opened off the UI thread must
            // still answer `is_main_thread` for the UI thread.
            main_thread: recorded_main_thread().unwrap_or_else(|| std::thread::current().id()),
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
            ports_changed: AtomicBool::new(false),
            gui_closed: AtomicBool::new(false),
            gui_resize: AtomicU64::new(0),
            state_dirty: AtomicBool::new(false),
            params_pending: AtomicBool::new(false),
            timers: Mutex::new(Vec::new()),
            next_timer: AtomicU32::new(1),
            last_log: Mutex::new(None),
        }
    }

    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(crate) fn take_gui_resize(&self) -> Option<(u32, u32)> {
        let packed = self.gui_resize.swap(0, Ordering::Relaxed);
        if packed == 0 {
            None
        } else {
            Some(((packed >> 32) as u32, packed as u32))
        }
    }

    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(crate) fn take_gui_closed(&self) -> bool {
        self.gui_closed.swap(false, Ordering::Relaxed)
    }

    /// Whether the plugin reported state changes since the last save.
    pub(crate) fn state_dirty(&self) -> bool {
        self.state_dirty.load(Ordering::Relaxed)
    }

    /// Whether the plugin asked for an audio-port rescan that the fixed
    /// buffers cannot follow.  The insert is stopped until it is reloaded.
    pub(crate) fn ports_changed(&self) -> bool {
        self.ports_changed.load(Ordering::Relaxed)
    }

    /// A state for tests that only exercise slot bookkeeping.
    #[cfg(test)]
    pub(crate) fn for_test(ports_changed: bool) -> Self {
        let mut state = Self::new();
        // Bookkeeping stand-ins stay on the caller even if another test has
        // claimed the process-wide UI thread.
        state.main_thread = std::thread::current().id();
        state.ports_changed.store(ports_changed, Ordering::Relaxed);
        state
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
/// exactly once.  It must be dropped after every plugin created through the
/// entry is destroyed and before the library is unloaded.
///
/// Outside the tests only a [`ClapModule`] owns one, so the module registry
/// decides when `init` and `deinit` run.
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

/// A freshly opened CLAP library, before its entry is initialised.
struct OpenedModule {
    library: libloading::Library,
    entry: *const clap_plugin_entry,
    /// The path handed to `entry.init`.
    init_path: CString,
}

/// One loaded CLAP library with its one successful `entry.init`.  Every
/// instance and every scan of the library shares it through the
/// [`ModuleRegistry`], so `init` runs once per load of the library and
/// `deinit` only after the last user is gone.
struct ClapModule {
    // `init` deinits the entry before the library is unloaded.
    init: EntryInit,
    _library: libloading::Library,
}

// SAFETY: the entry is immutable data inside the library, which the module
// keeps loaded.  `init` and `deinit` only run under the registry lock, and
// CLAP makes `get_factory` and the plugin factory callable from any thread.
unsafe impl Send for ClapModule {}
unsafe impl Sync for ClapModule {}

impl ClapModule {
    fn plugin_factory(&self) -> *const clap_plugin_factory {
        self.init.plugin_factory()
    }
}

/// The process-wide table of loaded CLAP modules, keyed by the canonical path
/// of the binary.  CLAP 1.0/1.1 modules do not tolerate a second `init`, a
/// `deinit` under a live instance, or `init`/`deinit` racing each other, and
/// the OS shares one copy of a library between every `LoadLibrary`/`dlopen`
/// of it, so a per-instance or per-scan init is not safe.  The registry
/// therefore holds one library and one `init` per module, and its lock is
/// held for the whole of every `init` and every `deinit` (and the library
/// load and unload around them), so those never overlap.
///
/// One lock covers every module, so a slow library load or `init` of one
/// module delays loads and scans of others until it finishes; loads are
/// rare and happen off the audio thread, so this is accepted for the
/// simpler guarantee.
///
/// The lock is only ever taken on the UI, bridge and scan threads: a module is
/// released when the last [`ModuleHandle`] drops, and the audio callback never
/// drops the last reference to a plugin (see `PluginChain::process`).
struct ModuleRegistry {
    modules: Mutex<std::collections::BTreeMap<PathBuf, Arc<ClapModule>>>,
    /// Test hook: how long a [`ModuleHandle`] drop lingers after releasing
    /// the lock, to widen any window in which it still held a reference.
    #[cfg(test)]
    release_pause_ms: AtomicU64,
}

/// The registry every load and scan goes through.
static MODULES: ModuleRegistry = ModuleRegistry::new();

impl ModuleRegistry {
    const fn new() -> Self {
        Self {
            modules: Mutex::new(std::collections::BTreeMap::new()),
            #[cfg(test)]
            release_pause_ms: AtomicU64::new(0),
        }
    }

    #[cfg(test)]
    fn pause_after_release(&self) {
        let pause = self.release_pause_ms.load(Ordering::SeqCst);
        if pause > 0 {
            std::thread::sleep(Duration::from_millis(pause));
        }
    }

    fn lock(
        &self,
    ) -> std::sync::MutexGuard<'_, std::collections::BTreeMap<PathBuf, Arc<ClapModule>>> {
        self.modules
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Hands out the module stored under `key`, opening and initialising it
    /// with `open` first when no one is using it.  A module whose `open`,
    /// version check or `init` fails is not stored, so the next acquire tries
    /// again from scratch.
    fn acquire(
        &'static self,
        key: PathBuf,
        open: impl FnOnce() -> Result<OpenedModule, String>,
    ) -> Result<ModuleHandle, String> {
        let mut modules = self.lock();
        if let Some(module) = modules.get(&key) {
            let module = Arc::clone(module);
            return Ok(ModuleHandle {
                registry: self,
                key,
                module: Some(module),
            });
        }
        let opened = open()?;
        if !clap_sys::version::clap_version_is_compatible(unsafe { (*opened.entry).clap_version }) {
            return Err("This CLAP plugin uses an incompatible CLAP version.".to_owned());
        }
        // SAFETY: `opened.entry` comes from `opened.library`, which the module
        // keeps loaded until after the guard is dropped.
        let Some(init) = (unsafe { EntryInit::init(opened.entry, &opened.init_path) }) else {
            return Err("The CLAP plugin could not be initialised.".to_owned());
        };
        let module = Arc::new(ClapModule {
            init,
            _library: opened.library,
        });
        modules.insert(key.clone(), Arc::clone(&module));
        Ok(ModuleHandle {
            registry: self,
            key,
            module: Some(module),
        })
    }

    #[cfg(test)]
    fn holds(&self, key: &Path) -> bool {
        self.lock().contains_key(key)
    }
}

/// One user's share of a [`ClapModule`].  Handles are never cloned; each one
/// comes from [`ModuleRegistry::acquire`].  Dropping the last one deinits the
/// module and unloads its library, under the registry lock.
struct ModuleHandle {
    registry: &'static ModuleRegistry,
    key: PathBuf,
    /// Always `Some` until `drop`.
    module: Option<Arc<ClapModule>>,
}

impl std::ops::Deref for ModuleHandle {
    type Target = ClapModule;

    fn deref(&self) -> &ClapModule {
        self.module.as_deref().expect("a live module handle")
    }
}

impl Drop for ModuleHandle {
    fn drop(&mut self) {
        let Some(module) = self.module.take() else {
            return;
        };
        let mut modules = self.registry.lock();
        // Every reference to a module is created (`acquire`) and released
        // (here) under the lock, so the count is stable while it is held:
        // two references (the table's and this one) mean this is the last
        // user.  This handle's reference is released before the lock is, on
        // every path, so a concurrent drop never counts it.
        let last = Arc::strong_count(&module) == 2;
        let stored = if last {
            let stored = modules.remove(&self.key);
            debug_assert!(stored
                .as_ref()
                .is_some_and(|stored| Arc::ptr_eq(stored, &module)));
            stored
        } else {
            None
        };
        drop(module);
        // On the last release `deinit` and the unload run here, still under
        // the lock.
        drop(stored);
        drop(modules);
        #[cfg(test)]
        self.registry.pause_after_release();
    }
}

/// The registry key for a CLAP binary: its canonical path, so different
/// spellings of one file share one module.  When the path cannot be
/// canonicalised (a filesystem that cannot report the final path), the
/// absolute path is used instead; the library load then decides whether the
/// file can be opened at all.
fn module_key(binary: &Path) -> PathBuf {
    std::fs::canonicalize(binary)
        .or_else(|_| std::path::absolute(binary))
        .unwrap_or_else(|_| binary.to_path_buf())
}

/// Opens a CLAP library and finds its entry, without initialising it.
fn open_module(bundle: &Path, binary: &Path) -> Result<OpenedModule, String> {
    let init_path = CString::new(
        entry_init_path(bundle, binary)
            .to_string_lossy()
            .into_owned(),
    )
    .map_err(|_| "The CLAP path contains a NUL byte.".to_owned())?;
    let library = unsafe { libloading::Library::new(binary) }
        .map_err(|error| format!("CLAP load failed: {error}"))?;
    let entry = {
        let symbol: libloading::Symbol<'_, *const clap_plugin_entry> = unsafe {
            library
                .get(b"clap_entry\0")
                .map_err(|error| format!("Missing clap_entry symbol: {error}"))?
        };
        *symbol
    };
    if entry.is_null() {
        return Err("The clap_entry symbol is null.".to_owned());
    }
    Ok(OpenedModule {
        library,
        entry,
        init_path,
    })
}

/// The shared module for a `.clap` bundle (or bare binary), loading and
/// initialising it if no one else is using it.
fn acquire_module(bundle: &Path) -> Result<ModuleHandle, String> {
    let binary = bundle_binary_path(bundle);
    MODULES.acquire(module_key(&binary), || open_module(bundle, &binary))
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

/// A loaded CLAP effect.  The plugin pointer and its module are kept together
/// so the plugin can never outlive the code it came from.
pub(crate) struct ClapPlugin {
    // Fields drop in declaration order, which is the teardown order CLAP
    // needs: destroy the instance, free the host the plugin pointed at, and
    // only then release the shared module (which deinits the entry and
    // unloads the library if this was its last user).  These four must stay
    // first and in this order.
    instance: PluginInstance,
    _host: Box<clap_host>,
    host_state: Arc<HostState>,
    _module: ModuleHandle,
    /// `instance.plugin`, copied for brevity; owned by `instance`.
    plugin: *const clap_plugin,
    params_ext: *const clap_plugin_params,
    gui_ext: *const clap_plugin_gui,
    timer_ext: *const clap_plugin_timer_support,
    state_ext: *const clap_plugin_state,
    cached_params: Vec<ClapParam>,
    /// One buffer per declared audio port, set up just before activation.
    inputs: PortBuffers,
    outputs: PortBuffers,
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
        let module = acquire_module(path)?;
        Self::instantiate(module, plugin_id, sample_rate, max_block, restore)
    }

    /// Creates, sets up and activates one plugin from an initialised module.
    /// Every step may fail; the guards then tear down whatever was set up, in
    /// order, before the module is released.
    fn instantiate(
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
    /// scope.  The error is static so the audio path does not allocate.
    fn start_processing(&mut self) -> Result<(), &'static str> {
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
    ) -> Result<(), &'static str> {
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
            return Ok(());
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
        Ok(())
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
        // the plugin, then the host goes and the module is released (see the
        // field order on the struct).
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

/// What a slot shows once the plugin has declared audio ports that no longer
/// match the buffers it was loaded with.
pub(crate) const AUDIO_PORTS_CHANGED: &str =
    "The CLAP plugin changed its audio ports; remove it and add it again.";

/// Rescan flags that cannot invalidate the buffers: port names and metadata,
/// and the in-place pairing, which this host never uses because every port has
/// its own buffers.
const AUDIO_PORTS_RESCAN_HARMLESS: u32 = CLAP_AUDIO_PORTS_RESCAN_NAMES
    | CLAP_AUDIO_PORTS_RESCAN_PORT_TYPE
    | CLAP_AUDIO_PORTS_RESCAN_IN_PLACE_PAIR;

/// Rescan flags whose change would leave the buffers wrongly sized or wired:
/// a different port list, channel count, or port flags needs another load.
const AUDIO_PORTS_RESCAN_STOPS: u32 = CLAP_AUDIO_PORTS_RESCAN_LIST
    | CLAP_AUDIO_PORTS_RESCAN_CHANNEL_COUNT
    | CLAP_AUDIO_PORTS_RESCAN_FLAGS;

const AUDIO_PORTS_RESCAN_ALL: u32 = AUDIO_PORTS_RESCAN_HARMLESS | AUDIO_PORTS_RESCAN_STOPS;

/// Every rescan flag is "supported" in the only way RigWerk can: the buffers
/// are fixed once the plugin is active.  A request that changes the port
/// layout or channel counts stops the insert until it is loaded again (see
/// `process`); requests about names, port type, or in-place pairing are
/// absorbed without touching the buffers.
unsafe extern "C" fn host_audio_ports_flag_supported(_host: *const clap_host, flag: u32) -> bool {
    flag != 0 && flag & !AUDIO_PORTS_RESCAN_ALL == 0
}

/// Marks the port buffers stale when the change would make them wrong.  Only
/// a flag is set, so nothing is reallocated under the audio thread.
unsafe extern "C" fn host_audio_ports_rescan(host: *const clap_host, flags: u32) {
    // An unknown flag means the plugin is asking for something this host did
    // not agree to handle; stop rather than risk stale buffers.
    if flags & (AUDIO_PORTS_RESCAN_STOPS | !AUDIO_PORTS_RESCAN_ALL) == 0 {
        return;
    }
    if let Some(state) = unsafe { state(host) } {
        state.ports_changed.store(true, Ordering::Relaxed);
    }
}

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

/// The most audio ports per direction, and channels per port, RigWerk sets up
/// buffers for.  Anything larger is a broken description, not an effect.
const MAX_AUDIO_PORTS: usize = 32;
const MAX_PORT_CHANNELS: usize = 64;

/// What [`port_layout`] needs to know about one declared audio port.  The
/// channel count is authoritative (the port type only names a common
/// layout), and every port gets its own buffers, so `in_place_pair` never
/// matters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PortInfo {
    channel_count: u32,
    flags: u32,
}

/// The ports `process` hands the plugin: one entry per declared port, in the
/// plugin's index order, with its channel count.
#[derive(Debug, PartialEq, Eq)]
struct PortLayout {
    inputs: Vec<usize>,
    outputs: Vec<usize>,
    /// The input that gets the stereo signal; every other input (a
    /// sidechain, say) gets silence.
    main_input: usize,
    /// The output that is read back; the others are ignored.
    main_output: usize,
}

/// Decides the buffers for the ports a plugin declares.  `None` means the
/// plugin has no audio-ports extension.  The CLAP spec says such a plugin
/// has no ports at all, but RigWerk has always given it one stereo input
/// and output, which is what simple effects without the extension expect.
///
/// The main port is the one flagged `CLAP_AUDIO_PORT_IS_MAIN` (the spec puts
/// it at index 0, but the flag wins wherever it is); with no flag, index 0
/// stands in.  A main port with one channel gets the mono sum and its output
/// goes to both sides; one with more than two gets left and right on its
/// first two channels and silence on the rest, and only its first two
/// output channels are read.
fn port_layout(ports: Option<(&[PortInfo], &[PortInfo])>) -> Result<PortLayout, String> {
    let Some((inputs, outputs)) = ports else {
        return Ok(PortLayout {
            inputs: vec![2],
            outputs: vec![2],
            main_input: 0,
            main_output: 0,
        });
    };
    let (inputs, main_input) = direction_layout(inputs, "input")?;
    let (outputs, main_output) = direction_layout(outputs, "output")?;
    Ok(PortLayout {
        inputs,
        outputs,
        main_input,
        main_output,
    })
}

fn direction_layout(ports: &[PortInfo], direction: &str) -> Result<(Vec<usize>, usize), String> {
    if ports.is_empty() {
        return Err(format!("This CLAP plugin has no audio {direction}."));
    }
    if ports.len() > MAX_AUDIO_PORTS {
        return Err(format!(
            "This CLAP plugin declares more than {MAX_AUDIO_PORTS} audio {direction}s."
        ));
    }
    let counts: Vec<usize> = ports
        .iter()
        .map(|port| port.channel_count as usize)
        .collect();
    if counts.iter().any(|count| *count > MAX_PORT_CHANNELS) {
        return Err(format!(
            "This CLAP plugin declares an audio {direction} with more than {MAX_PORT_CHANNELS} channels."
        ));
    }
    let main = ports
        .iter()
        .position(|port| port.flags & CLAP_AUDIO_PORT_IS_MAIN != 0)
        .unwrap_or(0);
    if counts[main] == 0 {
        return Err(format!(
            "This CLAP plugin's main audio {direction} has no channels."
        ));
    }
    Ok((counts, main))
}

/// Input and output ports as a plugin declares them.
type DeclaredPorts = (Vec<PortInfo>, Vec<PortInfo>);

/// Reads every audio port the plugin declares, for [`port_layout`].  A
/// main-thread call, made while the plugin is inactive.
fn read_audio_ports(
    plugin: *const clap_plugin,
    ext: *const clap_plugin_audio_ports,
) -> Result<Option<DeclaredPorts>, String> {
    let Some(ext) = (unsafe { ext.as_ref() }) else {
        return Ok(None);
    };
    let (Some(count), Some(get)) = (ext.count, ext.get) else {
        return Err("This CLAP plugin's audio ports cannot be read.".to_owned());
    };
    let read = |is_input: bool| -> Result<Vec<PortInfo>, String> {
        let total = unsafe { count(plugin, is_input) } as usize;
        // Checked before asking for each port, so a nonsense count cannot
        // make the host allocate or loop for long.
        if total > MAX_AUDIO_PORTS {
            return Err(format!(
                "This CLAP plugin declares more than {MAX_AUDIO_PORTS} audio ports."
            ));
        }
        let mut ports = Vec::with_capacity(total);
        for index in 0..total as u32 {
            let mut info: clap_audio_port_info = unsafe { std::mem::zeroed() };
            // A port that cannot be described still exists at its index, so
            // skipping it would shift every later buffer onto the wrong port.
            if !unsafe { get(plugin, index, is_input, &mut info) } {
                return Err(
                    "This CLAP plugin could not describe one of its audio ports.".to_owned(),
                );
            }
            ports.push(PortInfo {
                channel_count: info.channel_count,
                flags: info.flags,
            });
        }
        Ok(ports)
    };
    Ok(Some((read(true)?, read(false)?)))
}

/// The preallocated buffers for one direction: one `clap_audio_buffer` per
/// declared port, each pointing at that port's channels.  Nothing here
/// allocates after [`PortBuffers::new`].
struct PortBuffers {
    /// Every channel of every port, in port order, each `max_block` long.
    channels: Vec<Vec<f32>>,
    /// The channel pointers the `clap_audio_buffer`s point into.
    pointers: Vec<*mut f32>,
    ports: Vec<clap_audio_buffer>,
    /// Index into `channels` of each port's first channel.
    first: Vec<usize>,
    /// Each port's channel count.
    counts: Vec<usize>,
    /// The main port's index.
    main: usize,
}

impl Default for PortBuffers {
    fn default() -> Self {
        Self::new(&[], 0, 0)
    }
}

impl PortBuffers {
    fn new(counts: &[usize], main: usize, block: usize) -> Self {
        let total = counts.iter().sum();
        let mut first = Vec::with_capacity(counts.len());
        let mut next = 0;
        for count in counts {
            first.push(next);
            next += count;
        }
        let mut buffers = Self {
            channels: vec![vec![0.0; block]; total],
            pointers: vec![ptr::null_mut(); total],
            ports: counts
                .iter()
                .map(|count| clap_audio_buffer {
                    data32: ptr::null_mut(),
                    data64: ptr::null_mut(),
                    channel_count: *count as u32,
                    latency: 0,
                    constant_mask: 0,
                })
                .collect(),
            first,
            counts: counts.to_vec(),
            main,
        };
        buffers.refresh();
        buffers
    }

    /// The channels of the main port.  Empty when the buffers have not been
    /// set up (only [`PortBuffers::default`]), so a stray call cannot panic.
    fn main_channels(&mut self) -> &mut [Vec<f32>] {
        let (Some(first), Some(count)) = (self.first.get(self.main), self.counts.get(self.main))
        else {
            return &mut [];
        };
        &mut self.channels[*first..*first + *count]
    }

    /// Clears the first `frames` samples of every channel.
    fn silence(&mut self, frames: usize) {
        for channel in &mut self.channels {
            channel[..frames].fill(0.0);
        }
    }

    /// Points every port at its channels again.  The plugin gets mutable
    /// pointers and could have changed any of them (or set a constant
    /// mask), so this runs before each block.
    fn refresh(&mut self) {
        for (pointer, channel) in self.pointers.iter_mut().zip(&mut self.channels) {
            *pointer = channel.as_mut_ptr();
        }
        let base = self.pointers.as_mut_ptr();
        for (index, port) in self.ports.iter_mut().enumerate() {
            let count = self.counts[index];
            // A zero-channel port gets a null pointer rather than one past the
            // end of `pointers`; a plugin that checks the pointer instead of
            // the count then sees the empty port.
            port.data32 = if count == 0 {
                ptr::null_mut()
            } else {
                // In bounds: `first + count <= pointers.len()`.
                unsafe { base.add(self.first[index]) }
            };
            port.data64 = ptr::null_mut();
            port.channel_count = count as u32;
            port.latency = 0;
            port.constant_mask = 0;
        }
    }
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
    found.sort_by_cached_key(|candidate| candidate.name.to_lowercase());
    found.dedup_by(|a, b| {
        a.path == b.path && a.plugin_id == b.plugin_id && a.name.eq_ignore_ascii_case(&b.name)
    });
    found
}

/// Lists the effects in one bundle.  The scan shares the module with any
/// loaded instance of it: a module already in use is neither initialised
/// again nor deinitialised when the scan finishes.
pub(crate) fn scan_bundle(bundle: &Path) -> Result<Vec<ClapCandidate>, String> {
    let module = acquire_module(bundle)?;
    Ok(scan_module(bundle, &module))
}

fn scan_module(bundle: &Path, module: &ClapModule) -> Vec<ClapCandidate> {
    let factory = module.plugin_factory();
    if factory.is_null() {
        return Vec::new();
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
    found
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
        use clap_sys::id::CLAP_INVALID_ID;
        use clap_sys::process::{clap_process_status, CLAP_PROCESS_CONTINUE};
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
            NoAudioPorts,
            Activate,
        }

        thread_local! {
            static FAIL: Cell<Fail> = const { Cell::new(Fail::Nothing) };
            static LOG: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
            /// The ports the fake declares; `None` leaves out the extension.
            static PORTS_DECLARED: RefCell<Option<DeclaredPorts>> =
                const { RefCell::new(None) };
            static SEEN: RefCell<Option<Seen>> = const { RefCell::new(None) };
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
            SEEN.with(|seen| seen.borrow_mut().take());
            // `NoAudioPorts` declares the extension with no ports at all.
            PORTS_DECLARED.with(|ports| {
                *ports.borrow_mut() =
                    (step == Fail::NoAudioPorts).then(|| (Vec::new(), Vec::new()));
            });
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
                process: Some(plugin_process),
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
            if id == CLAP_EXT_AUDIO_PORTS && PORTS_DECLARED.with(|ports| ports.borrow().is_some()) {
                return (&PORTS as *const clap_plugin_audio_ports).cast();
            }
            ptr::null()
        }

        fn declared(is_input: bool) -> Vec<PortInfo> {
            PORTS_DECLARED.with(|ports| {
                let ports = ports.borrow();
                let (inputs, outputs) = ports.as_ref().expect("ports declared");
                if is_input {
                    inputs.clone()
                } else {
                    outputs.clone()
                }
            })
        }

        unsafe extern "C" fn ports_count(_plugin: *const clap_plugin, is_input: bool) -> u32 {
            declared(is_input).len() as u32
        }

        unsafe extern "C" fn ports_get(
            _plugin: *const clap_plugin,
            index: u32,
            is_input: bool,
            info: *mut clap_audio_port_info,
        ) -> bool {
            let Some(port) = declared(is_input).get(index as usize).copied() else {
                return false;
            };
            unsafe {
                (*info).id = index;
                (*info).channel_count = port.channel_count;
                (*info).flags = port.flags;
                (*info).port_type = ptr::null();
                (*info).in_place_pair = CLAP_INVALID_ID;
            }
            true
        }

        /// Reads every channel of every input port the host passes (so a
        /// port pointing at the wrong memory shows up) and fills each output
        /// channel with `10 * port + channel + 1`.
        unsafe extern "C" fn plugin_process(
            _plugin: *const clap_plugin,
            process: *const clap_process,
        ) -> clap_process_status {
            let process = unsafe { &*process };
            let frames = process.frames_count as usize;
            let inputs = unsafe {
                std::slice::from_raw_parts(
                    process.audio_inputs,
                    process.audio_inputs_count as usize,
                )
            };
            let input_sums = inputs
                .iter()
                .map(|port| {
                    (0..port.channel_count as usize)
                        .map(|channel| {
                            let samples = unsafe {
                                std::slice::from_raw_parts(*port.data32.add(channel), frames)
                            };
                            samples.iter().sum::<f32>()
                        })
                        .collect()
                })
                .collect();
            let outputs = unsafe {
                std::slice::from_raw_parts_mut(
                    process.audio_outputs,
                    process.audio_outputs_count as usize,
                )
            };
            for (index, port) in outputs.iter().enumerate() {
                for channel in 0..port.channel_count as usize {
                    let samples = unsafe {
                        std::slice::from_raw_parts_mut(*port.data32.add(channel), frames)
                    };
                    samples.fill((10 * index + channel + 1) as f32);
                }
            }
            SEEN.with(|seen| {
                *seen.borrow_mut() = Some(Seen {
                    inputs_count: process.audio_inputs_count,
                    outputs_count: process.audio_outputs_count,
                    input_sums,
                    output_channels: outputs.iter().map(|port| port.channel_count).collect(),
                });
            });
            CLAP_PROCESS_CONTINUE
        }

        /// What the fake plugin saw in its last `process` call.
        #[derive(Debug, PartialEq)]
        pub(super) struct Seen {
            pub(super) inputs_count: u32,
            pub(super) outputs_count: u32,
            /// Per input port, per channel: the sum of the block.
            pub(super) input_sums: Vec<Vec<f32>>,
            pub(super) output_channels: Vec<u32>,
        }

        /// Makes the fake plugin offer the audio-ports extension with these
        /// `(channel_count, flags)` ports.
        pub(super) fn declare_ports(inputs: &[(u32, u32)], outputs: &[(u32, u32)]) {
            let ports = |list: &[(u32, u32)]| {
                list.iter()
                    .map(|&(channel_count, flags)| PortInfo {
                        channel_count,
                        flags,
                    })
                    .collect()
            };
            PORTS_DECLARED.with(|declared| {
                *declared.borrow_mut() = Some((ports(inputs), ports(outputs)));
            });
        }

        pub(super) fn seen() -> Option<Seen> {
            SEEN.with(|seen| seen.borrow_mut().take())
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

        /// The registry the fake module is loaded through.  Each test thread
        /// uses its own key (the log is per thread too), so parallel tests
        /// never share a fake module.
        pub(super) static REGISTRY: ModuleRegistry = ModuleRegistry::new();

        pub(super) fn key() -> PathBuf {
            PathBuf::from(format!("fake-{:?}.clap", std::thread::current().id()))
        }

        pub(super) fn opened(entry: &'static clap_plugin_entry) -> OpenedModule {
            OpenedModule {
                library: library(),
                entry,
                init_path: c"fake.clap".to_owned(),
            }
        }

        /// Acquires the fake module the way a load or scan does.
        pub(super) fn acquire() -> Result<ModuleHandle, String> {
            REGISTRY.acquire(key(), || Ok(opened(&ENTRY)))
        }

        /// Runs the real load sequence against the fake module.
        pub(super) fn load(plugin_id: &str) -> Result<ClapPlugin, String> {
            let pref = crate::audio::PluginPref::default();
            let restore = Restore::from_pref(&pref);
            let module = acquire()?;
            ClapPlugin::instantiate(module, plugin_id, 48_000, 256, &restore)
                .map(|(plugin, _)| plugin)
        }

        /// A second fake entry that counts `init`/`deinit` across threads
        /// and records whether two of those calls ever overlapped.
        pub(super) mod counted {
            use super::super::super::*;
            use std::sync::atomic::AtomicUsize;

            pub(in super::super) static INITS: AtomicUsize = AtomicUsize::new(0);
            pub(in super::super) static DEINITS: AtomicUsize = AtomicUsize::new(0);
            pub(in super::super) static OVERLAPPED: AtomicBool = AtomicBool::new(false);
            static INSIDE: AtomicBool = AtomicBool::new(false);

            fn enter_and_wait() {
                if INSIDE.swap(true, Ordering::SeqCst) {
                    OVERLAPPED.store(true, Ordering::SeqCst);
                }
                // Long enough for a racing caller to arrive.
                std::thread::sleep(Duration::from_millis(20));
                INSIDE.store(false, Ordering::SeqCst);
            }

            unsafe extern "C" fn init(_path: *const c_char) -> bool {
                INITS.fetch_add(1, Ordering::SeqCst);
                enter_and_wait();
                true
            }

            unsafe extern "C" fn deinit() {
                DEINITS.fetch_add(1, Ordering::SeqCst);
                enter_and_wait();
            }

            unsafe extern "C" fn get_factory(_id: *const c_char) -> *const c_void {
                ptr::null()
            }

            pub(in super::super) static ENTRY: clap_plugin_entry = clap_plugin_entry {
                clap_version: CLAP_VERSION,
                init: Some(init),
                deinit: Some(deinit),
                get_factory: Some(get_factory),
            };
        }
    }

    #[test]
    fn instances_and_scans_of_one_module_share_one_init() {
        fake::reset(fake::Fail::Nothing);
        let first = fake::load("fake").expect("load the first instance");
        let second = fake::load("fake").expect("load the second instance");
        // A rescan while both are loaded shares the module too.
        let scan = fake::acquire().expect("scan the module");
        assert!(super::scan_module(std::path::Path::new("fake.clap"), &scan).is_empty());
        drop(scan);
        assert_eq!(fake::count("entry.init"), 1);
        assert_eq!(fake::count("entry.deinit"), 0);
        drop(first);
        assert_eq!(fake::count("destroy"), 1);
        assert_eq!(fake::count("entry.deinit"), 0);
        assert!(fake::REGISTRY.holds(&fake::key()));
        drop(second);
        assert_eq!(fake::count("destroy"), 2);
        assert_eq!(fake::count("entry.deinit"), 1);
        assert!(!fake::REGISTRY.holds(&fake::key()));
        // The last release came after the last instance was destroyed.
        assert_eq!(fake::events().last(), Some(&"entry.deinit"));
        // A load after everything is released starts the module afresh.
        let third = fake::load("fake").expect("load again");
        assert_eq!(fake::count("entry.init"), 2);
        drop(third);
        assert_eq!(fake::count("entry.deinit"), 2);
    }

    #[test]
    fn a_failed_module_init_is_not_cached() {
        fake::reset(fake::Fail::EntryInit);
        assert!(fake::acquire().is_err());
        assert!(!fake::REGISTRY.holds(&fake::key()));
        fake::reset(fake::Fail::Nothing);
        let plugin = fake::load("fake").expect("the next load retries the init");
        assert_eq!(fake::count("entry.init"), 1);
        drop(plugin);
        assert_eq!(fake::count("entry.deinit"), 1);
    }

    #[test]
    fn a_failed_open_is_not_cached() {
        fake::reset(fake::Fail::Nothing);
        let key = fake::key();
        let error = fake::REGISTRY.acquire(key.clone(), || Err("missing".to_owned()));
        assert!(error.is_err());
        assert!(!fake::REGISTRY.holds(&key));
        let module = fake::acquire().expect("the next acquire opens the module");
        assert_eq!(fake::events(), ["entry.init"]);
        drop(module);
        assert_eq!(fake::events(), ["entry.init", "entry.deinit"]);
    }

    #[test]
    fn concurrent_acquires_init_the_module_once() {
        use fake::counted::{DEINITS, INITS, OVERLAPPED};
        static REGISTRY: super::ModuleRegistry = super::ModuleRegistry::new();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    let module = REGISTRY
                        .acquire("counted.clap".into(), || {
                            Ok(fake::opened(&fake::counted::ENTRY))
                        })
                        .expect("acquire the counted module");
                    // Both threads hold the module at once before either
                    // releases it.
                    barrier.wait();
                    drop(module);
                })
            })
            .collect();
        for thread in threads {
            thread.join().expect("acquire thread");
        }
        assert_eq!(INITS.load(Ordering::SeqCst), 1);
        assert_eq!(DEINITS.load(Ordering::SeqCst), 1);
        assert!(!OVERLAPPED.load(Ordering::SeqCst));
        assert!(!REGISTRY.holds(std::path::Path::new("counted.clap")));
    }

    #[test]
    fn concurrent_last_releases_still_deinit_the_module() {
        fake::reset(fake::Fail::Nothing);
        static REGISTRY: super::ModuleRegistry = super::ModuleRegistry::new();
        // Each drop lingers after unlocking; a drop that still held its
        // reference then would make every concurrent drop see a third
        // reference, and none would release the module.
        REGISTRY.release_pause_ms.store(20, Ordering::SeqCst);
        const THREADS: usize = 4;
        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                REGISTRY
                    .acquire("released.clap".into(), || Ok(fake::opened(&fake::ENTRY)))
                    .expect("acquire the fake module")
            })
            .collect();
        assert_eq!(fake::events(), ["entry.init"]);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(THREADS));
        std::thread::scope(|scope| {
            for handle in handles {
                let barrier = std::sync::Arc::clone(&barrier);
                scope.spawn(move || {
                    barrier.wait();
                    drop(handle);
                });
            }
        });
        assert!(!REGISTRY.holds(std::path::Path::new("released.clap")));
        // `deinit` ran on whichever thread released last, so this thread's
        // log cannot show it; the empty table is the proof it was released.
    }

    #[test]
    fn module_keys_resolve_different_spellings_of_one_file() {
        let dir = std::env::temp_dir().join(format!("rigwerk-clap-key-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create the test folder");
        let file = dir.join("module.clap");
        std::fs::write(&file, b"").expect("create the test file");
        let dotted = dir.join(".").join("module.clap");
        assert_eq!(super::module_key(&file), super::module_key(&dotted));
        // A path that cannot be canonicalised still gets an absolute key.
        let missing = std::path::Path::new("no-such-dir/missing.clap");
        assert!(super::module_key(missing).is_absolute());
        let _ = std::fs::remove_dir_all(&dir);
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
                fake::Fail::NoAudioPorts,
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

    fn port(channel_count: u32, flags: u32) -> super::PortInfo {
        super::PortInfo {
            channel_count,
            flags,
        }
    }

    fn layout(
        inputs: &[usize],
        outputs: &[usize],
        main_input: usize,
        main_output: usize,
    ) -> super::PortLayout {
        super::PortLayout {
            inputs: inputs.to_vec(),
            outputs: outputs.to_vec(),
            main_input,
            main_output,
        }
    }

    const MAIN: u32 = super::CLAP_AUDIO_PORT_IS_MAIN;

    /// `(channel_count, flags)` per input and per output port.
    type FakePorts<'a> = (&'a [(u32, u32)], &'a [(u32, u32)]);

    #[test]
    fn port_layout_follows_the_declared_ports() {
        use super::port_layout;
        let stereo = [port(2, MAIN)];
        // Main at index 0.
        assert_eq!(
            port_layout(Some((&stereo, &stereo))),
            Ok(layout(&[2], &[2], 0, 0))
        );
        // Main at index 1: the flag wins, every port keeps its buffer.
        let late = [port(1, 0), port(2, MAIN)];
        assert_eq!(
            port_layout(Some((&late, &late))),
            Ok(layout(&[1, 2], &[1, 2], 1, 1))
        );
        // No main flag: index 0 stands in.
        let unflagged = [port(2, 0), port(1, 0)];
        assert_eq!(
            port_layout(Some((&unflagged, &stereo))),
            Ok(layout(&[2, 1], &[2], 0, 0))
        );
        // A sidechain input gets its own buffer.
        let sidechain = [port(2, MAIN), port(2, 0)];
        assert_eq!(
            port_layout(Some((&sidechain, &stereo))),
            Ok(layout(&[2, 2], &[2], 0, 0))
        );
        // Mono main ports.
        let mono = [port(1, MAIN)];
        assert_eq!(
            port_layout(Some((&mono, &mono))),
            Ok(layout(&[1], &[1], 0, 0))
        );
        // More than two channels on the main port are set up in full.
        let surround = [port(6, MAIN)];
        assert_eq!(
            port_layout(Some((&surround, &surround))),
            Ok(layout(&[6], &[6], 0, 0))
        );
        // No extension: one stereo input and output, as before.
        assert_eq!(port_layout(None), Ok(layout(&[2], &[2], 0, 0)));
        // An auxiliary port without channels is fine.
        let empty_aux = [port(2, MAIN), port(0, 0)];
        assert_eq!(
            port_layout(Some((&empty_aux, &stereo))),
            Ok(layout(&[2, 0], &[2], 0, 0))
        );
    }

    #[test]
    fn port_layout_rejects_unusable_ports() {
        use super::port_layout;
        let stereo = [port(2, MAIN)];
        // Zero ports in either direction.
        assert!(port_layout(Some((&[], &stereo))).is_err());
        assert!(port_layout(Some((&stereo, &[]))).is_err());
        assert!(port_layout(Some((&[], &[]))).is_err());
        // A main port without channels.
        assert!(port_layout(Some((&[port(0, MAIN), port(2, 0)], &stereo))).is_err());
        // Nonsense sizes.
        let wide = [port(super::MAX_PORT_CHANNELS as u32 + 1, MAIN)];
        assert!(port_layout(Some((&wide, &stereo))).is_err());
        let many = vec![port(2, 0); super::MAX_AUDIO_PORTS + 1];
        assert!(port_layout(Some((&stereo, &many))).is_err());
    }

    /// Loads the fake with `(inputs, outputs)` ports (`None`: no extension)
    /// and runs one block of left 0.5, right -0.25 through it.
    fn process_fake(ports: Option<FakePorts<'_>>) -> (fake::Seen, Vec<f32>, Vec<f32>) {
        fake::reset(fake::Fail::Nothing);
        if let Some((inputs, outputs)) = ports {
            fake::declare_ports(inputs, outputs);
        }
        let mut plugin = fake::load("fake").expect("load the fake plugin");
        let mut left = vec![0.5f32; 64];
        let mut right = vec![-0.25f32; 64];
        plugin
            .process(&mut left, &mut right, 64, 1.0)
            .expect("process a block");
        let seen = fake::seen().expect("the plugin processed");
        (seen, left, right)
    }

    #[test]
    fn process_passes_every_declared_port() {
        // A sidechain compressor: main stereo input plus a stereo sidechain.
        let (seen, left, right) = process_fake(Some((&[(2, MAIN), (2, 0)], &[(2, MAIN)])));
        assert_eq!(seen.inputs_count, 2);
        assert_eq!(seen.outputs_count, 1);
        assert_eq!(seen.input_sums, [vec![32.0, -16.0], vec![0.0, 0.0]]);
        assert_eq!(seen.output_channels, [2]);
        assert!(left.iter().all(|sample| *sample == 1.0));
        assert!(right.iter().all(|sample| *sample == 2.0));
    }

    #[test]
    fn process_feeds_and_reads_the_flagged_main_port() {
        // Main ports at index 1: a 6-channel input after a mono aux, a mono
        // output after a stereo aux.
        let (seen, left, right) = process_fake(Some((&[(1, 0), (6, MAIN)], &[(2, 0), (1, MAIN)])));
        assert_eq!(seen.inputs_count, 2);
        assert_eq!(seen.outputs_count, 2);
        assert_eq!(
            seen.input_sums,
            [vec![0.0], vec![32.0, -16.0, 0.0, 0.0, 0.0, 0.0]]
        );
        assert_eq!(seen.output_channels, [2, 1]);
        // Output port 1, channel 0, on both sides.
        assert!(left.iter().all(|sample| *sample == 11.0));
        assert!(right.iter().all(|sample| *sample == 11.0));
    }

    #[test]
    fn process_mixes_a_mono_main_port_down_and_back_up() {
        let (seen, left, right) = process_fake(Some((&[(1, MAIN)], &[(1, MAIN)])));
        assert_eq!(seen.input_sums, [vec![8.0]]);
        assert!(left.iter().chain(&right).all(|sample| *sample == 1.0));
    }

    #[test]
    fn process_without_the_extension_uses_one_stereo_pair() {
        let (seen, left, right) = process_fake(None);
        assert_eq!(seen.inputs_count, 1);
        assert_eq!(seen.outputs_count, 1);
        assert_eq!(seen.input_sums, [vec![32.0, -16.0]]);
        assert_eq!(seen.output_channels, [2]);
        assert!(left.iter().all(|sample| *sample == 1.0));
        assert!(right.iter().all(|sample| *sample == 2.0));
    }

    #[test]
    fn a_zero_channel_port_gets_a_null_pointer() {
        use super::PortBuffers;
        let buffers = PortBuffers::new(&[2, 0], 0, 8);
        assert_eq!(buffers.ports[0].channel_count, 2);
        assert!(!buffers.ports[0].data32.is_null());
        assert_eq!(buffers.ports[1].channel_count, 0);
        assert!(buffers.ports[1].data32.is_null());
        // The empty default is only a placeholder, but asking for its main
        // port must not panic.
        let mut empty = PortBuffers::default();
        assert!(empty.main_channels().is_empty());
    }

    #[test]
    fn process_passes_a_zero_channel_aux_port_as_empty() {
        let (seen, left, right) = process_fake(Some((&[(2, MAIN), (0, 0)], &[(2, MAIN), (0, 0)])));
        assert_eq!(seen.inputs_count, 2);
        assert_eq!(seen.input_sums, [vec![32.0, -16.0], vec![]]);
        assert_eq!(seen.outputs_count, 2);
        assert_eq!(seen.output_channels, [2, 0]);
        assert!(left.iter().all(|sample| *sample == 1.0));
        assert!(right.iter().all(|sample| *sample == 2.0));
    }

    #[test]
    fn process_restores_pointers_a_plugin_changed() {
        fake::reset(fake::Fail::Nothing);
        fake::declare_ports(&[(2, MAIN)], &[(2, MAIN)]);
        let mut plugin = fake::load("fake").expect("load the fake plugin");
        let mut left = vec![0.5f32; 32];
        let mut right = vec![-0.25f32; 32];
        plugin
            .process(&mut left, &mut right, 32, 1.0)
            .expect("process the first block");
        // A misbehaving plugin can overwrite the buffer descriptions it was
        // handed.  `refresh` must restore everything before the next call.
        for port in &mut plugin.inputs.ports {
            port.data32 = std::ptr::null_mut();
            port.data64 = std::ptr::null_mut();
            port.constant_mask = u64::MAX;
        }
        for port in &mut plugin.outputs.ports {
            port.data32 = std::ptr::null_mut();
            port.channel_count = 0;
            port.constant_mask = u64::MAX;
        }
        left.fill(0.5);
        right.fill(-0.25);
        plugin
            .process(&mut left, &mut right, 32, 1.0)
            .expect("process the second block");
        let seen = fake::seen().expect("the plugin processed");
        assert_eq!(seen.inputs_count, 1);
        assert_eq!(seen.input_sums, [vec![16.0, -8.0]]);
        assert_eq!(seen.output_channels, [2]);
        assert!(left.iter().all(|sample| *sample == 1.0));
        assert!(right.iter().all(|sample| *sample == 2.0));
        for port in plugin.inputs.ports.iter().chain(&plugin.outputs.ports) {
            assert!(port.data64.is_null());
            assert_eq!(port.constant_mask, 0);
        }
        assert_eq!(plugin.outputs.ports[0].channel_count, 2);
    }

    #[test]
    fn process_refreshes_every_sub_block_of_a_long_buffer() {
        fake::reset(fake::Fail::Nothing);
        fake::declare_ports(&[(2, MAIN), (2, 0)], &[(2, MAIN)]);
        let mut plugin = fake::load("fake").expect("load the fake plugin");
        // 600 frames are three calls (256, 256, 88) at the fake's block size.
        let mut left = vec![0.5f32; 600];
        let mut right = vec![-0.25f32; 600];
        plugin
            .process(&mut left, &mut right, 600, 1.0)
            .expect("process a long buffer");
        let seen = fake::seen().expect("the plugin processed");
        assert_eq!(seen.inputs_count, 2);
        assert_eq!(seen.input_sums, [vec![44.0, -22.0], vec![0.0, 0.0]]);
        assert_eq!(seen.output_channels, [2]);
        assert!(left.iter().all(|sample| *sample == 1.0));
        assert!(right.iter().all(|sample| *sample == 2.0));
    }

    #[test]
    fn a_port_rescan_stops_processing_until_reloaded() {
        use super::*;
        fake::reset(fake::Fail::Nothing);
        fake::declare_ports(&[(2, MAIN)], &[(2, MAIN)]);
        let mut plugin = fake::load("fake").expect("load the fake plugin");
        let host: *const clap_host = &*plugin._host;
        let mut left = vec![0.5f32; 16];
        let mut right = vec![0.5f32; 16];
        unsafe {
            assert!(host_audio_ports_flag_supported(
                host,
                CLAP_AUDIO_PORTS_RESCAN_LIST
            ));
            assert!(!host_audio_ports_flag_supported(host, 1 << 31));
            // Names and the two metadata flags do not touch the buffers.
            host_audio_ports_rescan(host, CLAP_AUDIO_PORTS_RESCAN_NAMES);
            assert!(!plugin.host_state.ports_changed());
            assert!(plugin.process(&mut left, &mut right, 16, 1.0).is_ok());
            host_audio_ports_rescan(
                host,
                CLAP_AUDIO_PORTS_RESCAN_PORT_TYPE | CLAP_AUDIO_PORTS_RESCAN_IN_PLACE_PAIR,
            );
            assert!(!plugin.host_state.ports_changed());
            assert!(plugin.process(&mut left, &mut right, 16, 1.0).is_ok());
        }
        assert!(fake::seen().is_some());
        unsafe { host_audio_ports_rescan(host, CLAP_AUDIO_PORTS_RESCAN_CHANNEL_COUNT) };
        assert!(plugin.host_state.ports_changed());
        assert!(plugin.process(&mut left, &mut right, 16, 1.0).is_err());
        assert!(fake::seen().is_none(), "the plugin was not called");
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
