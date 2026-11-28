//! cpal's ALSA threads start at normal priority.  The first callback asks
//! for `SCHED_FIFO` once: cpal's `realtime` feature needs rtkit and is a
//! no-op without it.  A refusal leaves the thread as it was.  PipeWire and
//! Windows already promote their own threads.

/// Below PipeWire's data thread (88) and above threaded IRQs (50).
#[cfg(target_os = "linux")]
const WANTED_PRIORITY: libc::c_int = 70;

/// One attempt per thread.  Later calls are the flag check.
#[derive(Default)]
pub(crate) struct PromoteOnce {
    tried: bool,
}

impl PromoteOnce {
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
    // All-zero is valid.  A struct literal cannot name musl's padding.
    // SAFETY: `sched_param`'s zero value is valid.
    let mut param: libc::sched_param = unsafe { std::mem::zeroed() };
    param.sched_priority = priority;
    // SCHED_RESET_ON_FORK keeps a child from inheriting real time.
    // SAFETY: the calling thread, with a valid parameter.
    unsafe {
        libc::pthread_setschedparam(
            libc::pthread_self(),
            libc::SCHED_FIFO | libc::SCHED_RESET_ON_FORK,
            &param,
        ) == 0
    }
}

/// [`WANTED_PRIORITY`], lowered to the soft `RLIMIT_RTPRIO`.  A limit of 0
/// is still tried: root and `CAP_SYS_NICE` ignore the limit.
#[cfg(target_os = "linux")]
fn allowed_priority() -> Option<libc::c_int> {
    // SAFETY: valid arguments.
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
