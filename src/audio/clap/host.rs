//! Host callbacks.  A plugin may call any of them from any thread, so
//! each only sets an atomic or try-locks a list the UI services.  None
//! calls into the plugin or blocks.  The main-thread id is copied from
//! `plugins::thread`.

use std::ffi::{c_char, c_void, CStr};
use std::ptr;
#[cfg(target_os = "linux")]
use std::sync::atomic::AtomicI32;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
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

use super::ports::{AUDIO_PORTS_RESCAN_ALL, AUDIO_PORTS_RESCAN_STOPS};
use crate::audio::plugins::{
    on_audio_thread, recorded_main_thread, PluginShared, RESTART_ACTIVATION,
};

pub(super) const HOST_NAME: &[u8] = b"RigWerk\0";
pub(super) const HOST_VENDOR: &[u8] = b"RigWerk\0";
pub(super) const HOST_URL: &[u8] = b"https://rigwerk.local\0";
pub(super) const HOST_VERSION: &[u8] = concat!(env!("CARGO_PKG_VERSION"), "\0").as_bytes();

pub(super) struct TimerEntry {
    pub(super) id: u32,
    pub(super) period: Duration,
    pub(super) next: Instant,
}

/// Fixed so `host_log` can run on the process thread without allocating.
/// The UI builds the `String` when it reads the slot.
pub(super) const LOG_BYTES: usize = 256;

/// How many file-descriptor unregister requests may wait for a busy list.
#[cfg(target_os = "linux")]
pub(super) const PENDING_UNREGISTER: usize = 8;

/// An empty slot.  A descriptor is never negative (`host_register_fd`
/// refuses those).
#[cfg(target_os = "linux")]
const EMPTY_PENDING_FD: i32 = -1;

/// `host_log` may run on the process thread, so a write never allocates
/// and never waits.  The message is cut at the first character that does
/// not fit, and [`truncated`](Self::truncated) records that.
pub(super) struct LogSlot {
    bytes: [u8; LOG_BYTES],
    len: usize,
    truncated: bool,
}

impl LogSlot {
    const fn new() -> Self {
        Self {
            bytes: [0; LOG_BYTES],
            len: 0,
            truncated: false,
        }
    }

    /// Invalid UTF-8 becomes U+FFFD.  The copy stops before a character
    /// that would cross the end.  Only the bytes that can still reach the
    /// slot are validated, plus three for the character at its edge.
    fn write(&mut self, bytes: &[u8]) {
        self.len = 0;
        self.truncated = false;
        let limit = LOG_BYTES + 3;
        let cut_by_limit = bytes.len() > limit;
        let mut rest = &bytes[..bytes.len().min(limit)];
        loop {
            match std::str::from_utf8(rest) {
                Ok(text) => {
                    self.push(text);
                    self.truncated |= cut_by_limit;
                    return;
                }
                Err(error) => {
                    // SAFETY: `valid_up_to` is the length of the valid prefix
                    // `from_utf8` just checked, so this slice is valid UTF-8.
                    let valid =
                        unsafe { std::str::from_utf8_unchecked(&rest[..error.valid_up_to()]) };
                    if !self.push(valid) {
                        return;
                    }
                    let skipped = match error.error_len() {
                        Some(len) => len,
                        None => {
                            // An incomplete sequence at the end.  Cut to
                            // the limit, the character continues past it:
                            // drop it whole.  A real fragment is U+FFFD.
                            if cut_by_limit {
                                self.truncated = true;
                                return;
                            }
                            rest.len() - error.valid_up_to()
                        }
                    };
                    let mut encoded = [0u8; 4];
                    let replacement = char::REPLACEMENT_CHARACTER.encode_utf8(&mut encoded);
                    if !self.push(replacement) {
                        return;
                    }
                    rest = &rest[error.valid_up_to() + skipped..];
                }
            }
        }
    }

    /// `false` once a character does not fit.  The message is then
    /// truncated and stops.
    fn push(&mut self, text: &str) -> bool {
        for character in text.chars() {
            let mut encoded = [0u8; 4];
            let bytes = character.encode_utf8(&mut encoded).as_bytes();
            if self.len + bytes.len() > LOG_BYTES {
                self.truncated = true;
                return false;
            }
            self.bytes[self.len..self.len + bytes.len()].copy_from_slice(bytes);
            self.len += bytes.len();
        }
        true
    }

    #[cfg(test)]
    pub(super) fn text(&self) -> &str {
        std::str::from_utf8(&self.bytes[..self.len]).unwrap_or_default()
    }

    #[cfg(test)]
    pub(super) fn truncated(&self) -> bool {
        self.truncated
    }
}

/// A poisoned lock is recovered.  A busy one is `None`: a callback
/// never waits behind the UI or the plugin.
fn try_lock_now<T>(mutex: &Mutex<T>) -> Option<MutexGuard<'_, T>> {
    match mutex.try_lock() {
        Ok(guard) => Some(guard),
        Err(TryLockError::Poisoned(error)) => Some(error.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}

/// A plugin may call in from any thread, so this is atomics or a mutex
/// callbacks only try to take.  Restart, latency, ports, and the dirty
/// mark live in [`PluginShared`].
pub(crate) struct HostState {
    pub(super) main_thread: std::thread::ThreadId,
    pub(super) shared: Arc<PluginShared>,
    pub(super) process_requested: AtomicBool,
    /// Set by `clap_host_latency.changed`; the main thread re-reads the
    /// latency on its next service.
    pub(super) latency_changed: AtomicBool,
    pub(super) callback_requested: AtomicBool,
    pub(super) params_rescan: AtomicBool,
    /// `request_flush`.  A processed block delivers it; a skipped one
    /// flushes on the audio thread; an idle plugin is flushed by the
    /// main-thread service.
    pub(super) flush_requested: AtomicBool,
    /// Set once the port buffers are built: from then on a rescan that
    /// changes the ports stops the effect (see `host_audio_ports_rescan`).
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
    /// Unregisters a busy `fds` could not take.  The next service drains
    /// them before it polls.  A full queue refuses and leaves the
    /// descriptor registered.
    #[cfg(target_os = "linux")]
    pub(super) pending_unregister: [AtomicI32; PENDING_UNREGISTER],
    pub(super) timers: Mutex<Vec<TimerEntry>>,
    pub(super) next_timer: AtomicU32,
    /// The latest message from `clap_host_log`, in a fixed slot: a plugin
    /// may log from its process thread, where a `String` would allocate.
    pub(super) last_log: Mutex<Option<LogSlot>>,
}

impl HostState {
    pub(super) fn new(shared: Arc<PluginShared>) -> Self {
        Self {
            // The thread `plugins::thread::mark_main_thread` recorded.
            // Not the loader's thread: a plugin opened off the UI thread
            // must still answer the check for the UI thread.
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
            #[cfg(target_os = "linux")]
            pending_unregister: [const { AtomicI32::new(EMPTY_PENDING_FD) }; PENDING_UNREGISTER],
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

    pub(crate) fn take_gui_visibility(&self) -> Option<GuiVisibility> {
        match self.gui_visibility.swap(0, Ordering::Relaxed) {
            GUI_SHOW => Some(GuiVisibility::Show),
            GUI_HIDE => Some(GuiVisibility::Hide),
            _ => None,
        }
    }

    pub(crate) fn take_gui_hints_changed(&self) -> bool {
        self.gui_hints_changed.swap(false, Ordering::Relaxed)
    }

    /// Built on the caller's thread.  `None` before the first message,
    /// and while the plugin holds the slot: the read never waits.
    #[cfg(test)]
    pub(crate) fn log_message(&self) -> Option<(String, bool)> {
        let last = try_lock_now(&self.last_log)?;
        last.as_ref()
            .map(|slot| (slot.text().to_owned(), slot.truncated()))
    }

    pub(super) fn on_main_thread(&self) -> bool {
        std::thread::current().id() == self.main_thread
    }

    /// Due now, and rescheduled.  The list is released before the caller
    /// calls the plugin, which may register or unregister from there.
    pub(super) fn due_timers(&self) -> Vec<u32> {
        // Never wait: a plugin may be registering a timer from another
        // thread.  The due ones stay due for the next service.
        let Some(mut timers) = try_lock_now(&self.timers) else {
            return Vec::new();
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
    }
}

impl HostState {
    /// `false` when every slot is full, which leaves the descriptor
    /// registered.  A conforming plugin unregisters on the main thread,
    /// where the list is free, so this is the overflow path.
    #[cfg(target_os = "linux")]
    fn queue_unregister(&self, fd: i32) -> bool {
        for slot in &self.pending_unregister {
            if slot
                .compare_exchange(EMPTY_PENDING_FD, fd, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return true;
            }
            if slot.load(Ordering::Acquire) == fd {
                return true;
            }
        }
        false
    }

    /// The caller holds `fds`.  A request queued after this swap waits
    /// for the next service, so this pass still polls that descriptor
    /// once.  CLAP asks for unregister on the main thread, so only a
    /// plugin off that thread hits the window.
    #[cfg(target_os = "linux")]
    fn apply_pending_unregister(&self, fds: &mut Vec<(i32, clap_posix_fd_flags)>) {
        for slot in &self.pending_unregister {
            let fd = slot.swap(EMPTY_PENDING_FD, Ordering::AcqRel);
            if fd != EMPTY_PENDING_FD {
                fds.retain(|&(known, _)| known != fd);
            }
        }
    }

    /// Does not wait.  The list is released before the caller hands the
    /// ready descriptors to `on_fd`.
    #[cfg(target_os = "linux")]
    pub(super) fn ready_fds(&self) -> Vec<(i32, clap_posix_fd_flags)> {
        let mut polled: Vec<libc::pollfd> = {
            // Never wait: the list is a callback's, not this service's.
            let Some(mut fds) = try_lock_now(&self.fds) else {
                return Vec::new();
            };
            self.apply_pending_unregister(&mut fds);
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

/// `poll` reports errors on its own, so `CLAP_POSIX_FD_ERROR` needs no
/// event bit.
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

/// Any thread.  The main-thread service does the pause, the
/// reactivation, and the resume.
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

/// The UI thread, except while it stands in as the audio thread (a
/// forced stop, teardown, a flush with no block inside).  One thread
/// is never both.
pub(super) unsafe extern "C" fn host_is_main_thread(host: *const clap_host) -> bool {
    unsafe { state(host) }.is_some_and(|state| state.on_main_thread() && !on_audio_thread())
}

pub(super) unsafe extern "C" fn host_is_audio_thread(_host: *const clap_host) -> bool {
    on_audio_thread()
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
    let bytes = if message.is_null() {
        &[][..]
    } else {
        unsafe { CStr::from_ptr(message) }.to_bytes()
    };
    // A plugin may log from its process callback; never wait behind the UI.
    if let Some(mut last) = try_lock_now(&state.last_log) {
        last.get_or_insert_with(LogSlot::new).write(bytes);
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

/// Only a flag is set.  The editor window re-reads the hints on its
/// next service.
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

/// The open editor follows this on its next service.  The later of a
/// show and a hide wins.
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

/// The buffers are fixed once the plugin is active.  A change of port
/// layout or channel count stops the effect until it is loaded again.
/// Names, port type, and in-place pairing are absorbed.
pub(super) unsafe extern "C" fn host_audio_ports_flag_supported(
    _host: *const clap_host,
    flag: u32,
) -> bool {
    flag != 0 && flag & !AUDIO_PORTS_RESCAN_ALL == 0
}

/// Only a flag is set, so nothing is reallocated on the audio thread.
/// Before the buffers exist (during load) a rescan is harmless: the
/// ports are read after it.
pub(super) unsafe extern "C" fn host_audio_ports_rescan(host: *const clap_host, flags: u32) {
    // An unknown flag is something this host did not agree to handle.
    // Stop rather than risk stale buffers.
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

/// CLAP only lets latency change while inactive (inside `activate`) or
/// through `request_restart`.  Only a flag is set: the service reads the
/// value once the plugin is active again.  `latency.get` is not allowed
/// mid-`activate`.
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

/// A second registration of the same descriptor is refused.  CLAP asks
/// the plugin to use `modify_fd` for that.
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
    let Some(mut fds) = try_lock_now(&state.fds) else {
        // CLAP calls this on the main thread.  A service pass owns the
        // list briefly; answer false and change nothing, so the plugin
        // can retry.  The next service drains queued unregisters first.
        return false;
    };
    state.apply_pending_unregister(&mut fds);
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
    let Some(mut fds) = try_lock_now(&state.fds) else {
        return false;
    };
    state.apply_pending_unregister(&mut fds);
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
    if fd < 0 {
        return false;
    }
    let Some(mut fds) = try_lock_now(&state.fds) else {
        // Never wait: queue it, and the next service removes it before
        // it polls.  A full queue refuses and leaves the descriptor
        // registered.
        return state.queue_unregister(fd);
    };
    state.apply_pending_unregister(&mut fds);
    let before = fds.len();
    fds.retain(|&(known, _)| known != fd);
    fds.len() != before
}
