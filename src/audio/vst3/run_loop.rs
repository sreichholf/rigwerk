//! Linux `IRunLoop`, one process-wide registry, serviced on the UI
//! thread.  The host context and every editor frame register into it.
//! Registration `try_lock`s, so another thread never waits.
//!
//! Due handlers are copied out of the lock and checked still registered
//! before the call, so a callback may register or unregister any handler,
//! itself included.  Each entry records the binary its vtable lives in;
//! [`forget_module`] releases those before unload.  That only catches a
//! vtable in the plugin binary (VSTGUI, JUCE), not one in a library the
//! plugin loaded.

use std::ffi::c_void;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use ::vst3::Steinberg::Linux::{
    FileDescriptor, IEventHandler, IEventHandlerTrait, ITimerHandler, ITimerHandlerTrait,
    TimerInterval,
};
use ::vst3::Steinberg::{kInvalidArgument, kResultFalse, kResultTrue, tresult};
use ::vst3::{ComPtr, ComRef, Interface};

/// One registered handler.
struct Entry<H: Interface> {
    /// Tells this registration apart from a later one of the same handler.
    serial: u64,
    handler: ComPtr<H>,
    /// Which `IRunLoop` registered this.  Unregistering through one must
    /// not remove the others' registrations of the same handler.
    owner: usize,
    /// The load address of the binary holding the handler's vtable.
    module_base: usize,
}

struct Timer {
    entry: Entry<ITimerHandler>,
    interval: Duration,
    due: Instant,
}

struct Registry {
    next_serial: u64,
    fds: Vec<(Entry<IEventHandler>, FileDescriptor)>,
    timers: Vec<Timer>,
}

/// Only the UI thread waits for this lock.
static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    next_serial: 0,
    fds: Vec::new(),
    timers: Vec::new(),
});

fn registry() -> MutexGuard<'static, Registry> {
    REGISTRY.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The registry for a plugin's callback, or `None` while it is busy.
fn try_registry() -> Option<MutexGuard<'static, Registry>> {
    match REGISTRY.try_lock() {
        Ok(registry) => Some(registry),
        Err(std::sync::TryLockError::Poisoned(poison)) => Some(poison.into_inner()),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}

/// The load address of the binary `address` lies in, or 0 when `dladdr`
/// does not know it.
pub(super) fn module_base(address: *const c_void) -> usize {
    // SAFETY: `Dl_info` is pointers only, so all zeroes is a valid value.
    let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
    // SAFETY: `dladdr` only looks the address up and fills `info`.
    if unsafe { libc::dladdr(address, &mut info) } == 0 {
        0
    } else {
        info.dli_fbase as usize
    }
}

/// An entry for `handler`, holding its own reference.
///
/// # Safety
/// `handler` must be a live `H`.
unsafe fn entry<H: Interface>(
    registry: &mut Registry,
    handler: ComRef<'_, H>,
    owner: usize,
) -> Entry<H> {
    registry.next_serial += 1;
    // SAFETY: a live COM object starts with its vtable pointer.
    let vtable = unsafe { *handler.as_ptr().cast::<*const c_void>() };
    Entry {
        serial: registry.next_serial,
        handler: handler.to_com_ptr(),
        owner,
        module_base: module_base(vtable),
    }
}

/// One handler per descriptor per run loop (SDK editor host).
///
/// # Safety
/// `handler` must be null or a live handler.
pub(super) unsafe fn register_fd(
    owner: usize,
    handler: *mut IEventHandler,
    fd: FileDescriptor,
) -> tresult {
    // SAFETY: the caller's contract.
    let Some(handler) = (unsafe { ComRef::from_raw(handler) }) else {
        return kInvalidArgument;
    };
    let Some(mut registry) = try_registry() else {
        return kResultFalse;
    };
    if registry
        .fds
        .iter()
        .any(|(entry, registered)| entry.owner == owner && *registered == fd)
    {
        return kInvalidArgument;
    }
    // SAFETY: checked non-null above; the caller's contract.
    let entry = unsafe { entry(&mut registry, handler, owner) };
    registry.fds.push((entry, fd));
    kResultTrue
}

/// The call names only the handler, so every descriptor it watched on
/// this run loop goes at once (JUCE closes its X11 and message-loop
/// descriptors that way).  Another run loop's registration stays.
pub(super) fn unregister_fd(owner: usize, handler: *mut IEventHandler) -> tresult {
    if handler.is_null() {
        return kInvalidArgument;
    }
    let Some(mut registry) = try_registry() else {
        return kResultFalse;
    };
    let removed: Vec<_> = registry
        .fds
        .extract_if(.., |(entry, _)| {
            entry.owner == owner && entry.handler.as_ptr() == handler
        })
        .collect();
    if removed.is_empty() {
        return kResultFalse;
    }
    // A release may run plugin code: not under the lock.
    drop(registry);
    drop(removed);
    kResultTrue
}

/// A zero interval is refused, as the SDK's editor host does.
///
/// # Safety
/// `handler` must be null or a live handler.
pub(super) unsafe fn register_timer(
    owner: usize,
    handler: *mut ITimerHandler,
    milliseconds: TimerInterval,
) -> tresult {
    // SAFETY: the caller's contract.
    let Some(handler) = (unsafe { ComRef::from_raw(handler) }) else {
        return kInvalidArgument;
    };
    if milliseconds == 0 {
        return kInvalidArgument;
    }
    let Some(mut registry) = try_registry() else {
        return kResultFalse;
    };
    let interval = Duration::from_millis(milliseconds);
    // SAFETY: checked non-null above; the caller's contract.
    let entry = unsafe { entry(&mut registry, handler, owner) };
    registry.timers.push(Timer {
        entry,
        interval,
        due: Instant::now() + interval,
    });
    kResultTrue
}

/// The call names only the handler, so one call drops every registration
/// of it on this run loop.
pub(super) fn unregister_timer(owner: usize, handler: *mut ITimerHandler) -> tresult {
    if handler.is_null() {
        return kInvalidArgument;
    }
    let Some(mut registry) = try_registry() else {
        return kResultFalse;
    };
    let removed: Vec<_> = registry
        .timers
        .extract_if(.., |timer| {
            timer.entry.owner == owner && timer.entry.handler.as_ptr() == handler
        })
        .collect();
    if removed.is_empty() {
        return kResultFalse;
    }
    // A release may run plugin code: not under the lock.
    drop(registry);
    drop(removed);
    kResultTrue
}

/// Every run loop.  The reentrant-unregister test's callback has only
/// the raw pointer.
#[cfg(test)]
pub(super) fn unregister_timer_everywhere(handler: *mut ITimerHandler) -> tresult {
    if handler.is_null() {
        return kInvalidArgument;
    }
    let Some(mut registry) = try_registry() else {
        return kResultFalse;
    };
    let removed: Vec<_> = registry
        .timers
        .extract_if(.., |timer| timer.entry.handler.as_ptr() == handler)
        .collect();
    if removed.is_empty() {
        return kResultFalse;
    }
    drop(registry);
    drop(removed);
    kResultTrue
}

/// UI thread, once per frame.  Never blocks on a descriptor.
pub(crate) fn service_run_loop() {
    let now = Instant::now();
    let due: Vec<(u64, ComPtr<ITimerHandler>)> = {
        let mut registry = registry();
        registry
            .timers
            .iter_mut()
            .filter(|timer| timer.due <= now)
            .map(|timer| {
                // A late frame fires once, not once per missed interval.
                timer.due = now + timer.interval;
                (timer.entry.serial, timer.entry.handler.clone())
            })
            .collect()
    };
    for (serial, handler) in due {
        if registry()
            .timers
            .iter()
            .any(|timer| timer.entry.serial == serial)
        {
            // SAFETY: a registered handler, kept alive by the clone.
            unsafe { handler.onTimer() };
        }
    }

    let watched: Vec<(u64, ComPtr<IEventHandler>, FileDescriptor)> = registry()
        .fds
        .iter()
        .map(|(entry, fd)| (entry.serial, entry.handler.clone(), *fd))
        .collect();
    if watched.is_empty() {
        return;
    }
    let mut polled: Vec<libc::pollfd> = watched
        .iter()
        .map(|&(_, _, fd)| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    // SAFETY: `polled` is a valid array of `polled.len()` entries, and a
    // zero timeout never blocks.
    let ready = unsafe { libc::poll(polled.as_mut_ptr(), polled.len() as libc::nfds_t, 0) };
    if ready <= 0 {
        return;
    }
    for (polled, (serial, handler, fd)) in polled.iter().zip(watched) {
        if polled.revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) == 0 {
            continue;
        }
        if registry()
            .fds
            .iter()
            .any(|(entry, _)| entry.serial == serial)
        {
            // SAFETY: a registered handler, kept alive by the clone.
            unsafe { handler.onFDIsSet(fd) };
        }
    }
}

/// While the binary at `base` is still mapped, before its exit.
pub(super) fn forget_module(base: usize) {
    if base == 0 {
        return;
    }
    let (fds, timers): (Vec<_>, Vec<_>) = {
        let mut registry = registry();
        let fds = registry
            .fds
            .extract_if(.., |(entry, _)| entry.module_base == base);
        let fds: Vec<_> = fds.collect();
        let timers: Vec<_> = registry
            .timers
            .extract_if(.., |timer| timer.entry.module_base == base)
            .collect();
        (fds, timers)
    };
    // Released outside the lock: a release may run plugin code.
    drop(fds);
    drop(timers);
}

#[cfg(test)]
pub(super) fn counts() -> (usize, usize) {
    let registry = registry();
    (registry.fds.len(), registry.timers.len())
}

#[cfg(test)]
mod tests;
