//! CLAP host state, main-thread helpers, and host extension callbacks.
//!
//! The GUI callbacks may arrive on any thread, so they only set flags that
//! the open editor window follows on the UI thread.

use std::cell::Cell;
use std::ffi::{c_char, c_void, CStr};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

use clap_sys::ext::audio_ports::{clap_host_audio_ports, CLAP_EXT_AUDIO_PORTS};
use clap_sys::ext::gui::{clap_host_gui, CLAP_EXT_GUI};
use clap_sys::ext::latency::{clap_host_latency, CLAP_EXT_LATENCY};
use clap_sys::ext::log::{clap_host_log, clap_log_severity, CLAP_EXT_LOG};
use clap_sys::ext::params::{clap_host_params, CLAP_EXT_PARAMS};
use clap_sys::ext::posix_fd_support::clap_plugin_posix_fd_support;
#[cfg(target_os = "linux")]
use clap_sys::ext::posix_fd_support::{
    clap_host_posix_fd_support, clap_posix_fd_flags, CLAP_EXT_POSIX_FD_SUPPORT,
    CLAP_POSIX_FD_ERROR, CLAP_POSIX_FD_READ, CLAP_POSIX_FD_WRITE,
};
use clap_sys::ext::state::{clap_host_state, clap_plugin_state, CLAP_EXT_STATE};
use clap_sys::ext::thread_check::{clap_host_thread_check, CLAP_EXT_THREAD_CHECK};
use clap_sys::ext::timer_support::{
    clap_host_timer_support, clap_plugin_timer_support, CLAP_EXT_TIMER_SUPPORT,
};
use clap_sys::host::clap_host;
use clap_sys::plugin::clap_plugin;

use super::cstring;
use super::ports::{AUDIO_PORTS_RESCAN_ALL, AUDIO_PORTS_RESCAN_STOPS};
use super::state::{StateWriter, MAX_STATE_BYTES};

thread_local! {
    /// True while the current thread is inside a CLAP `process` call.  The
    /// thread-check callback answers from here, so the audio callback never
    /// takes a host-state lock just to report its identity.
    pub(super) static ON_AUDIO_THREAD: Cell<bool> = const { Cell::new(false) };
}

/// Marks the current thread as the CLAP audio thread for as long as it lives.
pub(super) struct AudioThreadScope;

impl AudioThreadScope {
    pub(super) fn enter() -> Self {
        ON_AUDIO_THREAD.with(|flag| flag.set(true));
        Self
    }
}

impl Drop for AudioThreadScope {
    fn drop(&mut self) {
        ON_AUDIO_THREAD.with(|flag| flag.set(false));
    }
}

pub(super) const HOST_NAME: &[u8] = b"RigWerk\0";
pub(super) const HOST_VENDOR: &[u8] = b"RigWerk\0";
pub(super) const HOST_URL: &[u8] = b"https://rigwerk.local\0";
pub(super) const HOST_VERSION: &[u8] = b"0.3.0\0";

pub(super) struct TimerEntry {
    pub(super) id: u32,
    pub(super) period: Duration,
    pub(super) next: Instant,
}

/// The UI thread, recorded by [`mark_main_thread`] before any plugin loads.
/// [`HostState`] copies the id when a plugin is created.  The audio callback
/// uses that copy; it does not lock this.
pub(super) static MAIN_THREAD: RwLock<Option<std::thread::ThreadId>> = RwLock::new(None);

pub(super) fn recorded_main_thread() -> Option<std::thread::ThreadId> {
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
    pub(super) previous: Option<std::thread::ThreadId>,
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
pub(super) struct MainThreadTarget {
    pub(super) plugin: *const clap_plugin,
    pub(super) timer_ext: *const clap_plugin_timer_support,
    pub(super) state_ext: *const clap_plugin_state,
    pub(super) fd_ext: *const clap_plugin_posix_fd_support,
}

// Safety: the pointers are only dereferenced on the main thread while the
// read lock is held, and cleared under the write lock before destruction.
unsafe impl Send for MainThreadTarget {}
unsafe impl Sync for MainThreadTarget {}

/// Callback state shared with the plugin.  The plugin may call into this from
/// its main, audio or helper threads, so everything here is atomics or a mutex
/// that callbacks only try to take.
pub(crate) struct HostState {
    pub(super) main_thread: std::thread::ThreadId,
    pub(super) target: RwLock<MainThreadTarget>,
    /// Set by `clap_host.request_restart`.  The audio thread stops
    /// processing (see `ClapPlugin::process`), then the main thread
    /// deactivates and reactivates the plugin in `ClapPlugin::service_pending`.
    pub(super) restart_requested: AtomicBool,
    pub(super) process_requested: AtomicBool,
    /// The plugin's latency in samples, read through `clap_plugin_latency.get`
    /// after every activation.  Readable from any thread without a lock.
    pub(super) latency: AtomicU32,
    /// Set by `clap_host_latency.changed`; the main thread re-reads the
    /// latency in `ClapPlugin::service_pending`.
    pub(super) latency_changed: AtomicBool,
    /// Set when a re-read moved [`Self::latency`], until the path's
    /// `service_plugins` takes it to republish the insert views.
    pub(super) latency_unseen: AtomicBool,
    /// Counts `ClapPlugin::process` calls, so the main thread can tell
    /// whether the audio callback still runs while a restart waits for it.
    pub(super) blocks: AtomicU64,
    pub(super) callback_requested: AtomicBool,
    pub(super) params_rescan: AtomicBool,
    pub(super) flush_requested: AtomicBool,
    /// Set by `clap_host_audio_ports.rescan` once the plugin is set up: its
    /// ports no longer match the buffers, so it needs loading again.
    pub(super) ports_changed: AtomicBool,
    pub(super) gui_closed: AtomicBool,
    pub(super) gui_resize: AtomicU64,
    /// The last `request_show` or `request_hide` the open editor window
    /// has not followed yet (see [`GuiVisibility`]); 0 for none.
    pub(super) gui_visibility: AtomicU8,
    /// Set by `resize_hints_changed`; the window re-reads the hints.
    pub(super) gui_hints_changed: AtomicBool,
    /// File descriptors the plugin registered through `posix-fd-support`,
    /// with the flags it watches.  Main-thread only, like the extension.
    #[cfg(target_os = "linux")]
    pub(super) fds: Mutex<Vec<(i32, clap_posix_fd_flags)>>,
    /// Set by `clap_host_state.mark_dirty`.
    pub(super) state_dirty: AtomicBool,
    /// Mirrors whether the instance holds parameter values the plugin has
    /// not received yet, so a state save can be deferred without taking the
    /// instance lock.
    pub(super) params_pending: AtomicBool,
    pub(super) timers: Mutex<Vec<TimerEntry>>,
    pub(super) next_timer: AtomicU32,
    pub(super) last_log: Mutex<Option<String>>,
}

impl HostState {
    pub(super) fn new() -> Self {
        Self {
            // Prefer the thread `mark_main_thread` recorded.  Do not publish
            // the loader's thread: a plugin opened off the UI thread must
            // still answer `is_main_thread` for the UI thread.
            main_thread: recorded_main_thread().unwrap_or_else(|| std::thread::current().id()),
            target: RwLock::new(MainThreadTarget {
                plugin: ptr::null(),
                timer_ext: ptr::null(),
                state_ext: ptr::null(),
                fd_ext: ptr::null(),
            }),
            restart_requested: AtomicBool::new(false),
            process_requested: AtomicBool::new(false),
            latency: AtomicU32::new(0),
            latency_changed: AtomicBool::new(false),
            latency_unseen: AtomicBool::new(false),
            blocks: AtomicU64::new(0),
            callback_requested: AtomicBool::new(false),
            params_rescan: AtomicBool::new(false),
            flush_requested: AtomicBool::new(false),
            ports_changed: AtomicBool::new(false),
            gui_closed: AtomicBool::new(false),
            gui_resize: AtomicU64::new(0),
            gui_visibility: AtomicU8::new(0),
            gui_hints_changed: AtomicBool::new(false),
            #[cfg(target_os = "linux")]
            fds: Mutex::new(Vec::new()),
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

    /// The show or hide the plugin asked for since the last call.
    pub(crate) fn take_gui_visibility(&self) -> Option<GuiVisibility> {
        match self.gui_visibility.swap(0, Ordering::Relaxed) {
            GUI_SHOW => Some(GuiVisibility::Show),
            GUI_HIDE => Some(GuiVisibility::Hide),
            _ => None,
        }
    }

    /// Whether the plugin's resize hints changed since the last call.
    pub(crate) fn take_gui_hints_changed(&self) -> bool {
        self.gui_hints_changed.swap(false, Ordering::Relaxed)
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

    /// The plugin's latency in samples as last read (0 without the latency
    /// extension).  Lock-free, so the audio thread may read it.
    pub(crate) fn latency(&self) -> u32 {
        self.latency.load(Ordering::Acquire)
    }

    /// Whether the latency changed since the last call, whoever serviced
    /// the plugin (the path, or an open editor window).
    pub(crate) fn take_latency_change(&self) -> bool {
        self.latency_unseen.swap(false, Ordering::AcqRel)
    }

    /// Whether a restart or latency change waits for the main thread.
    pub(super) fn lifecycle_pending(&self) -> bool {
        self.restart_requested.load(Ordering::Acquire)
            || self.latency_changed.load(Ordering::Acquire)
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
    /// flush or rescan, a restart, or a latency re-read is waiting; those
    /// touch the instance and must run through
    /// [`super::ClapPlugin::service_pending`] under its lock.
    pub(crate) fn service_main_thread(&self) -> bool {
        if let Ok(target) = self.target.read() {
            if !target.plugin.is_null() {
                if self.callback_requested.swap(false, Ordering::Relaxed) {
                    if let Some(on_main_thread) = unsafe { (*target.plugin).on_main_thread } {
                        unsafe { on_main_thread(target.plugin) };
                    }
                }
                self.service_timers(&target);
                #[cfg(target_os = "linux")]
                self.service_fds(&target);
            }
        }
        self.flush_requested.load(Ordering::Relaxed)
            || self.params_rescan.load(Ordering::Relaxed)
            || self.lifecycle_pending()
    }

    pub(super) fn service_timers(&self, target: &MainThreadTarget) {
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

impl HostState {
    /// Polls the registered file descriptors without waiting and hands the
    /// ready ones to `on_fd`.  Runs where the timers do, so a plugin that
    /// waits on its own display connection is serviced at the same rate.
    #[cfg(target_os = "linux")]
    pub(super) fn service_fds(&self, target: &MainThreadTarget) {
        let Some(on_fd) = (unsafe { target.fd_ext.as_ref() }).and_then(|ext| ext.on_fd) else {
            return;
        };
        let mut polled: Vec<libc::pollfd> = {
            let fds = self
                .fds
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            fds.iter()
                .map(|&(fd, flags)| libc::pollfd {
                    fd,
                    events: poll_events(flags),
                    revents: 0,
                })
                .collect()
        };
        if polled.is_empty() {
            return;
        }
        let ready = unsafe { libc::poll(polled.as_mut_ptr(), polled.len() as libc::nfds_t, 0) };
        if ready <= 0 {
            return;
        }
        for entry in polled {
            let flags = fd_flags(entry.revents);
            if flags != 0 {
                unsafe { on_fd(target.plugin, entry.fd, flags) };
            }
        }
    }
}

/// `poll` events for the CLAP flags a plugin registered.  Errors are always
/// reported by `poll`, so `CLAP_POSIX_FD_ERROR` needs no event bit.
#[cfg(target_os = "linux")]
pub(super) fn poll_events(flags: clap_posix_fd_flags) -> libc::c_short {
    let mut events = 0;
    if flags & CLAP_POSIX_FD_READ != 0 {
        events |= libc::POLLIN;
    }
    if flags & CLAP_POSIX_FD_WRITE != 0 {
        events |= libc::POLLOUT;
    }
    events
}

/// The CLAP flags for what `poll` returned.
#[cfg(target_os = "linux")]
pub(super) fn fd_flags(revents: libc::c_short) -> clap_posix_fd_flags {
    let mut flags = 0;
    if revents & libc::POLLIN != 0 {
        flags |= CLAP_POSIX_FD_READ;
    }
    if revents & libc::POLLOUT != 0 {
        flags |= CLAP_POSIX_FD_WRITE;
    }
    if revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
        flags |= CLAP_POSIX_FD_ERROR;
    }
    flags
}

const GUI_SHOW: u8 = 1;
const GUI_HIDE: u8 = 2;

/// A `request_show` or `request_hide` from the plugin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GuiVisibility {
    Show,
    Hide,
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

pub(super) unsafe fn state<'a>(host: *const clap_host) -> Option<&'a HostState> {
    let host = unsafe { host.as_ref() }?;
    unsafe { (host.host_data as *const HostState).as_ref() }
}

pub(super) unsafe extern "C" fn host_get_extension(
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
    } else if cfg!(target_os = "linux") {
        posix_fd_extension(id)
    } else {
        ptr::null()
    }
}

pub(super) unsafe extern "C" fn host_request_restart(host: *const clap_host) {
    if let Some(state) = unsafe { state(host) } {
        state.restart_requested.store(true, Ordering::Release);
    }
}

pub(super) unsafe extern "C" fn host_request_process(host: *const clap_host) {
    if let Some(state) = unsafe { state(host) } {
        state.process_requested.store(true, Ordering::Relaxed);
    }
}

pub(super) unsafe extern "C" fn host_request_callback(host: *const clap_host) {
    if let Some(state) = unsafe { state(host) } {
        state.callback_requested.store(true, Ordering::Relaxed);
    }
}

pub(super) static HOST_THREAD_CHECK: clap_host_thread_check = clap_host_thread_check {
    is_main_thread: Some(host_is_main_thread),
    is_audio_thread: Some(host_is_audio_thread),
};

pub(super) unsafe extern "C" fn host_is_main_thread(host: *const clap_host) -> bool {
    unsafe { state(host) }
        .map(|state| state.main_thread == std::thread::current().id())
        .unwrap_or(false)
}

pub(super) unsafe extern "C" fn host_is_audio_thread(_host: *const clap_host) -> bool {
    ON_AUDIO_THREAD.with(|flag| flag.get())
}

pub(super) static HOST_LOG: clap_host_log = clap_host_log {
    log: Some(host_log),
};

pub(super) unsafe extern "C" fn host_log(
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
    let text = cstring(message);
    // A plugin may log from its process callback; never wait behind the UI.
    if let Ok(mut last) = state.last_log.try_lock() {
        *last = Some(text);
    }
}

pub(super) static HOST_PARAMS: clap_host_params = clap_host_params {
    rescan: Some(host_params_rescan),
    clear: Some(host_params_clear),
    request_flush: Some(host_params_request_flush),
};

pub(super) unsafe extern "C" fn host_params_rescan(host: *const clap_host, _flags: u32) {
    if let Some(state) = unsafe { state(host) } {
        state.params_rescan.store(true, Ordering::Relaxed);
    }
}

pub(super) unsafe extern "C" fn host_params_clear(host: *const clap_host, _id: u32, _flags: u32) {
    if let Some(state) = unsafe { state(host) } {
        state.params_rescan.store(true, Ordering::Relaxed);
    }
}

pub(super) unsafe extern "C" fn host_params_request_flush(host: *const clap_host) {
    if let Some(state) = unsafe { state(host) } {
        state.flush_requested.store(true, Ordering::Relaxed);
    }
}

pub(super) static HOST_GUI: clap_host_gui = clap_host_gui {
    resize_hints_changed: Some(host_gui_resize_hints_changed),
    request_resize: Some(host_gui_request_resize),
    request_show: Some(host_gui_request_show),
    request_hide: Some(host_gui_request_hide),
    closed: Some(host_gui_closed),
};

/// Thread-safe in CLAP: only a flag is set, and the editor window re-reads
/// the hints on its next service.
pub(super) unsafe extern "C" fn host_gui_resize_hints_changed(host: *const clap_host) {
    if let Some(state) = unsafe { state(host) } {
        state.gui_hints_changed.store(true, Ordering::Relaxed);
    }
}

pub(super) unsafe extern "C" fn host_gui_request_resize(
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

/// Thread-safe in CLAP: the open editor window shows itself and the view on
/// its next service.  The later of a show and a hide wins.
pub(super) unsafe extern "C" fn host_gui_request_show(host: *const clap_host) -> bool {
    request_visibility(host, GUI_SHOW)
}

pub(super) unsafe extern "C" fn host_gui_request_hide(host: *const clap_host) -> bool {
    request_visibility(host, GUI_HIDE)
}

unsafe fn request_visibility(host: *const clap_host, request: u8) -> bool {
    let Some(state) = (unsafe { state(host) }) else {
        return false;
    };
    state.gui_visibility.store(request, Ordering::Relaxed);
    true
}

pub(super) unsafe extern "C" fn host_gui_closed(host: *const clap_host, _was_destroyed: bool) {
    if let Some(state) = unsafe { state(host) } {
        state.gui_closed.store(true, Ordering::Relaxed);
    }
}

pub(super) static HOST_AUDIO_PORTS: clap_host_audio_ports = clap_host_audio_ports {
    is_rescan_flag_supported: Some(host_audio_ports_flag_supported),
    rescan: Some(host_audio_ports_rescan),
};

/// Every rescan flag is "supported" in the only way RigWerk can: the buffers
/// are fixed once the plugin is active.  A request that changes the port
/// layout or channel counts stops the insert until it is loaded again (see
/// `process`); requests about names, port type, or in-place pairing are
/// absorbed without touching the buffers.
pub(super) unsafe extern "C" fn host_audio_ports_flag_supported(
    _host: *const clap_host,
    flag: u32,
) -> bool {
    flag != 0 && flag & !AUDIO_PORTS_RESCAN_ALL == 0
}

/// Marks the port buffers stale when the change would make them wrong.  Only
/// a flag is set, so nothing is reallocated under the audio thread.
pub(super) unsafe extern "C" fn host_audio_ports_rescan(host: *const clap_host, flags: u32) {
    // An unknown flag means the plugin is asking for something this host did
    // not agree to handle; stop rather than risk stale buffers.
    if flags & (AUDIO_PORTS_RESCAN_STOPS | !AUDIO_PORTS_RESCAN_ALL) == 0 {
        return;
    }
    if let Some(state) = unsafe { state(host) } {
        state.ports_changed.store(true, Ordering::Relaxed);
    }
}

pub(super) static HOST_TIMER_SUPPORT: clap_host_timer_support = clap_host_timer_support {
    register_timer: Some(host_register_timer),
    unregister_timer: Some(host_unregister_timer),
};

pub(super) unsafe extern "C" fn host_register_timer(
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

pub(super) unsafe extern "C" fn host_unregister_timer(
    host: *const clap_host,
    timer_id: u32,
) -> bool {
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

pub(super) static HOST_STATE: clap_host_state = clap_host_state {
    mark_dirty: Some(host_state_mark_dirty),
};

pub(super) unsafe extern "C" fn host_state_mark_dirty(host: *const clap_host) {
    if let Some(state) = unsafe { state(host) } {
        state.state_dirty.store(true, Ordering::Relaxed);
    }
}

pub(super) static HOST_LATENCY: clap_host_latency = clap_host_latency {
    changed: Some(host_latency_changed),
};

/// `clap_host_latency.changed` is a main-thread call, and CLAP only lets the
/// latency change while the plugin is inactive (inside `activate`) or through
/// `request_restart`.  Only a flag is set here: the main-thread service reads
/// the new value once the plugin is active again (it may be in the middle of
/// `activate` right now, where `latency.get` is not allowed yet).
pub(super) unsafe extern "C" fn host_latency_changed(host: *const clap_host) {
    if let Some(state) = unsafe { state(host) } {
        state.latency_changed.store(true, Ordering::Release);
    }
}

#[cfg(target_os = "linux")]
fn posix_fd_extension(id: &CStr) -> *const c_void {
    if id == CLAP_EXT_POSIX_FD_SUPPORT {
        &HOST_POSIX_FD as *const _ as *const c_void
    } else {
        ptr::null()
    }
}

#[cfg(not(target_os = "linux"))]
fn posix_fd_extension(_id: &CStr) -> *const c_void {
    ptr::null()
}

#[cfg(target_os = "linux")]
pub(super) static HOST_POSIX_FD: clap_host_posix_fd_support = clap_host_posix_fd_support {
    register_fd: Some(host_register_fd),
    modify_fd: Some(host_modify_fd),
    unregister_fd: Some(host_unregister_fd),
};

/// Registers `fd` once; a second registration of the same descriptor is
/// refused, as CLAP asks the plugin to use `modify_fd` for that.
#[cfg(target_os = "linux")]
pub(super) unsafe extern "C" fn host_register_fd(
    host: *const clap_host,
    fd: i32,
    flags: clap_posix_fd_flags,
) -> bool {
    let Some(state) = (unsafe { state(host) }) else {
        return false;
    };
    if fd < 0 {
        return false;
    }
    let mut fds = state
        .fds
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if fds.iter().any(|&(known, _)| known == fd) {
        return false;
    }
    fds.push((fd, flags));
    true
}

#[cfg(target_os = "linux")]
pub(super) unsafe extern "C" fn host_modify_fd(
    host: *const clap_host,
    fd: i32,
    flags: clap_posix_fd_flags,
) -> bool {
    let Some(state) = (unsafe { state(host) }) else {
        return false;
    };
    let mut fds = state
        .fds
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match fds.iter_mut().find(|(known, _)| *known == fd) {
        Some(entry) => {
            entry.1 = flags;
            true
        }
        None => false,
    }
}

#[cfg(target_os = "linux")]
pub(super) unsafe extern "C" fn host_unregister_fd(host: *const clap_host, fd: i32) -> bool {
    let Some(state) = (unsafe { state(host) }) else {
        return false;
    };
    let mut fds = state
        .fds
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let before = fds.len();
    fds.retain(|&(known, _)| known != fd);
    fds.len() != before
}
