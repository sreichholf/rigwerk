//! CLAP host state, main-thread helpers, and host extension callbacks.
//!
//! A plugin may call any host callback from any thread (its audio thread
//! included), so every callback here only sets an atomic, or try-locks a
//! list the UI services.  `request_restart`, `request_callback`,
//! `latency.changed` and the rest leave the work to the main-thread
//! service (`ClapInstance` behind the plugin's UI lock); none of them
//! calls into the plugin or blocks.

use std::cell::Cell;
use std::ffi::{c_char, c_void, CStr};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use clap_sys::ext::audio_ports::{clap_host_audio_ports, CLAP_EXT_AUDIO_PORTS};
use clap_sys::ext::gui::{clap_host_gui, CLAP_EXT_GUI};
use clap_sys::ext::latency::{clap_host_latency, CLAP_EXT_LATENCY};
use clap_sys::ext::log::{clap_host_log, clap_log_severity, CLAP_EXT_LOG};
use clap_sys::ext::params::{clap_host_params, CLAP_EXT_PARAMS};
#[cfg(target_os = "linux")]
use clap_sys::ext::posix_fd_support::{
    clap_host_posix_fd_support, clap_posix_fd_flags, CLAP_EXT_POSIX_FD_SUPPORT,
    CLAP_POSIX_FD_ERROR, CLAP_POSIX_FD_READ, CLAP_POSIX_FD_WRITE,
};
use clap_sys::ext::state::{clap_host_state, CLAP_EXT_STATE};
use clap_sys::ext::thread_check::{clap_host_thread_check, CLAP_EXT_THREAD_CHECK};
use clap_sys::ext::timer_support::{clap_host_timer_support, CLAP_EXT_TIMER_SUPPORT};
use clap_sys::host::clap_host;

use super::cstring;
use super::ports::{AUDIO_PORTS_RESCAN_ALL, AUDIO_PORTS_RESCAN_STOPS};
use crate::audio::plugins::{PluginShared, RESTART_ACTIVATION};

thread_local! {
    /// True while the current thread makes CLAP audio-thread calls
    /// (`start_processing`, `process`, `stop_processing`, an active
    /// `params.flush`).  The thread-check callback answers from here, so
    /// the audio callback never takes a host-state lock just to report its
    /// identity.
    pub(super) static ON_AUDIO_THREAD: Cell<bool> = const { Cell::new(false) };
}

/// Marks the current thread as the CLAP audio thread for as long as it
/// lives.  The UI thread enters one too when it makes an audio-thread call
/// with no block inside the plugin (a forced pause, teardown); it is the
/// plugin's audio thread for that call.  Scopes nest: dropping one restores
/// what was there before.
pub(super) struct AudioThreadScope {
    previous: bool,
}

impl AudioThreadScope {
    pub(super) fn enter() -> Self {
        Self {
            previous: ON_AUDIO_THREAD.with(|flag| flag.replace(true)),
        }
    }
}

impl Drop for AudioThreadScope {
    fn drop(&mut self) {
        ON_AUDIO_THREAD.with(|flag| flag.set(self.previous));
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

/// Callback state shared with the plugin.  The plugin may call into this from
/// its main, audio or helper threads, so everything here is atomics or a mutex
/// that callbacks only try to take.  What both halves of the plugin share
/// (restart requests, latency, ports, the dirty mark) lives in
/// [`PluginShared`], which the callbacks write directly.
pub(crate) struct HostState {
    pub(super) main_thread: std::thread::ThreadId,
    pub(super) shared: Arc<PluginShared>,
    pub(super) process_requested: AtomicBool,
    /// Set by `clap_host_latency.changed`; the main thread re-reads the
    /// latency on its next service.
    pub(super) latency_changed: AtomicBool,
    pub(super) callback_requested: AtomicBool,
    pub(super) params_rescan: AtomicBool,
    /// Set by `clap_host_params.request_flush`.  A processed block delivers
    /// it (and clears it); a skipped one flushes on the audio thread, and a
    /// plugin that is not processing is flushed by the main-thread service.
    pub(super) flush_requested: AtomicBool,
    /// Set once the port buffers are built: from then on a rescan that
    /// changes the ports stops the insert (see `host_audio_ports_rescan`).
    pub(super) ports_fixed: AtomicBool,
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
    pub(super) timers: Mutex<Vec<TimerEntry>>,
    pub(super) next_timer: AtomicU32,
    pub(super) last_log: Mutex<Option<String>>,
}

impl HostState {
    pub(super) fn new(shared: Arc<PluginShared>) -> Self {
        Self {
            // Prefer the thread `mark_main_thread` recorded.  Do not publish
            // the loader's thread: a plugin opened off the UI thread must
            // still answer `is_main_thread` for the UI thread.
            main_thread: recorded_main_thread().unwrap_or_else(|| std::thread::current().id()),
            shared,
            process_requested: AtomicBool::new(false),
            latency_changed: AtomicBool::new(false),
            callback_requested: AtomicBool::new(false),
            params_rescan: AtomicBool::new(false),
            flush_requested: AtomicBool::new(false),
            ports_fixed: AtomicBool::new(false),
            gui_closed: AtomicBool::new(false),
            gui_resize: AtomicU64::new(0),
            gui_visibility: AtomicU8::new(0),
            gui_hints_changed: AtomicBool::new(false),
            #[cfg(target_os = "linux")]
            fds: Mutex::new(Vec::new()),
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

    /// Whether this is the plugin's main thread.
    pub(super) fn on_main_thread(&self) -> bool {
        std::thread::current().id() == self.main_thread
    }

    /// The timers due now, rescheduled.  The list is released before the
    /// caller calls the plugin, which may (un)register timers from there.
    pub(super) fn due_timers(&self) -> Vec<u32> {
        let mut timers = self
            .timers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Instant::now();
        let mut due = Vec::new();
        for timer in timers.iter_mut() {
            if timer.next <= now {
                due.push(timer.id);
                timer.next = now + timer.period;
            }
        }
        due
    }
}

impl HostState {
    /// Polls the registered file descriptors without waiting and returns
    /// the ready ones with their CLAP flags.  The list is released before
    /// the caller hands them to `on_fd`.
    #[cfg(target_os = "linux")]
    pub(super) fn ready_fds(&self) -> Vec<(i32, clap_posix_fd_flags)> {
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
            return Vec::new();
        }
        let ready = unsafe { libc::poll(polled.as_mut_ptr(), polled.len() as libc::nfds_t, 0) };
        if ready <= 0 {
            return Vec::new();
        }
        polled
            .into_iter()
            .map(|entry| (entry.fd, fd_flags(entry.revents)))
            .filter(|(_, flags)| *flags != 0)
            .collect()
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

/// The outcome of `ClapInstance::save_state`.
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

/// Any thread.  The main-thread service pauses the plugin, deactivates and
/// reactivates it, and resumes it (see `PluginHandle::service`).
pub(super) unsafe extern "C" fn host_request_restart(host: *const clap_host) {
    if let Some(state) = unsafe { state(host) } {
        state.shared.request_restart(RESTART_ACTIVATION);
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

/// The UI thread, except while it stands in as the audio thread (a forced
/// stop, teardown, a flush with no block inside): one thread is never both.
pub(super) unsafe extern "C" fn host_is_main_thread(host: *const clap_host) -> bool {
    unsafe { state(host) }
        .is_some_and(|state| state.on_main_thread() && !ON_AUDIO_THREAD.with(Cell::get))
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

/// Asks for a reload when the change would make the port buffers wrong.
/// Only a flag is set, so nothing is reallocated under the audio thread;
/// the path rebuilds the insert with the new ports.  Before
/// the buffers are built (during load) a rescan is harmless: the ports are
/// read after it.
pub(super) unsafe extern "C" fn host_audio_ports_rescan(host: *const clap_host, flags: u32) {
    // An unknown flag means the plugin is asking for something this host did
    // not agree to handle; stop rather than risk stale buffers.
    if flags & (AUDIO_PORTS_RESCAN_STOPS | !AUDIO_PORTS_RESCAN_ALL) == 0 {
        return;
    }
    if let Some(state) = unsafe { state(host) } {
        if state.ports_fixed.load(Ordering::Acquire) {
            state.shared.want_reload();
        }
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
        state.shared.mark_state_dirty();
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
