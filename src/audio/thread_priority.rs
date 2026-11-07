//! Real-time scheduling for cpal's stream threads on Linux.
//!
//! cpal starts its ALSA input and output threads at normal priority.  Its
//! `realtime` feature can only promote them through rtkit over D-Bus, which
//! would add libdbus as a build dependency; without it the promotion is a
//! silent no-op.  So the first callback on each cpal stream thread asks the
//! kernel for `SCHED_FIFO` itself, once.  That succeeds for a user with an
//! `RLIMIT_RTPRIO` (the usual `audio` group setup in
//! `/etc/security/limits.d`) or `CAP_SYS_NICE`.  A refusal leaves the thread
//! as it was: playback runs exactly as before, just without the priority.
//!
//! PipeWire's own data thread is already real time (module-rt), and Windows
//! gets the same promotion from cpal's `realtime` feature, so this is a no-op
//! there.

/// The priority asked for, unless the RT limit allows less.  Below PipeWire's
/// data thread (88 by default) and the usual IRQ threads (50 is the kernel
/// default for threaded IRQs, so audio stays above them), like JACK's 70.
#[cfg(target_os = "linux")]
const WANTED_PRIORITY: libc::c_int = 70;

/// Tries once per thread; later calls are a plain flag check.
#[derive(Default)]
pub(crate) struct PromoteOnce {
    tried: bool,
}

impl PromoteOnce {
    /// Call at the top of a stream callback.  The first call makes one or two
    /// syscalls on this (the stream's) thread; every later call returns
    /// straight away.
    #[inline]
    pub(crate) fn run(&mut self) {
        if self.tried {
            return;
        }
        self.tried = true;
        promote_current_thread();
    }
}

#[cfg(target_os = "linux")]
fn promote_current_thread() -> bool {
    let Some(priority) = allowed_priority() else {
        return false;
    };
    let param = libc::sched_param {
        sched_priority: priority,
    };
    // SCHED_RESET_ON_FORK keeps a child process from inheriting real time.
    // SAFETY: plain syscall on the calling thread with a valid parameter.
    unsafe {
        libc::pthread_setschedparam(
            libc::pthread_self(),
            libc::SCHED_FIFO | libc::SCHED_RESET_ON_FORK,
            &param,
        ) == 0
    }
}

/// The priority to ask for: [`WANTED_PRIORITY`], lowered to the soft
/// `RLIMIT_RTPRIO`.  A limit of 0 is still tried at the wanted priority,
/// since root or `CAP_SYS_NICE` ignore the limit; the kernel says no otherwise.
#[cfg(target_os = "linux")]
fn allowed_priority() -> Option<libc::c_int> {
    // SAFETY: plain syscalls with valid arguments.
    let (min, max) = unsafe {
        (
            libc::sched_get_priority_min(libc::SCHED_FIFO),
            libc::sched_get_priority_max(libc::SCHED_FIFO),
        )
    };
    if min < 0 || max < min {
        return None;
    }
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limit` is a valid out-pointer.
    let soft = if unsafe { libc::getrlimit(libc::RLIMIT_RTPRIO, &mut limit) } == 0 {
        limit.rlim_cur
    } else {
        0
    };
    Some(priority_for(soft, min, max))
}

/// Pure part of [`allowed_priority`], for tests.
#[cfg(target_os = "linux")]
fn priority_for(soft_limit: libc::rlim_t, min: libc::c_int, max: libc::c_int) -> libc::c_int {
    let wanted = WANTED_PRIORITY.clamp(min, max);
    if soft_limit == 0 {
        return wanted;
    }
    let limit = soft_limit.min(max as libc::rlim_t) as libc::c_int;
    wanted.min(limit).max(min)
}

#[cfg(not(target_os = "linux"))]
fn promote_current_thread() -> bool {
    false
}

#[cfg(all(test, target_os = "linux"))]
mod tests;
