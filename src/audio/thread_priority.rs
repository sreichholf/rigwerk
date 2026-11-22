//! Real-time scheduling for cpal's stream threads on Linux, and pinning
//! every audio thread to the fast cores of a hybrid CPU.
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
//!
//! On a hybrid CPU (Intel P- and E-cores, ARM big.LITTLE) the kernel places a
//! real-time thread on any core, and an efficiency core takes about twice as
//! long for the same block.  [`prepare_fast_cores`] reads sysfs once, on the
//! thread that starts the bridge, and keeps the set of fastest cores; the
//! first callback on each audio thread then pins it there with one
//! `sched_setaffinity` ([`PromoteOnce`] for cpal's threads, [`PinOnce`] for
//! PipeWire's data thread, whose own priority is higher than ours and is
//! left alone).  The callback never reads a file.  Sysfs varies between
//! drivers and boards: anything missing, unreadable, oversized or
//! inconsistent means no pinning, and the threads run as they did before.
//! A CPU set the user already restricted the process to (`taskset`) is
//! respected: only its fast cores are kept, and none means no pinning.

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
        pin_current_thread();
    }
}

/// Pins the calling thread to the fast cores once; later calls are a plain
/// flag check.  For a thread that is already real time and must keep its
/// own priority (PipeWire's data thread).
#[cfg(target_os = "linux")]
#[derive(Default)]
pub(crate) struct PinOnce {
    tried: bool,
}

#[cfg(target_os = "linux")]
impl PinOnce {
    /// Call at the top of a process callback.  The first call makes at most
    /// one syscall on this thread; every later call returns straight away.
    #[inline]
    pub(crate) fn run(&mut self) {
        if self.tried {
            return;
        }
        self.tried = true;
        pin_current_thread();
    }
}

/// Finds the fast cores, once per process.  Call before an audio thread can
/// run, never on one: it reads sysfs.  Later calls return straight away.
pub(crate) fn prepare_fast_cores() {
    #[cfg(target_os = "linux")]
    FAST_CORES.get_or_init(detect_fast_cores);
}

/// Pins a thread that is not an audio thread (the capture loader) to the
/// fast cores, so what it measures is what an audio thread will see.  May
/// read sysfs.
pub(crate) fn pin_worker_to_fast_cores() {
    prepare_fast_cores();
    pin_current_thread();
}

/// The fast cores found by [`prepare_fast_cores`]: `None` inside when there
/// is nothing to pin to.
#[cfg(target_os = "linux")]
static FAST_CORES: std::sync::OnceLock<Option<libc::cpu_set_t>> = std::sync::OnceLock::new();

/// Pins the calling thread to the prepared fast cores.  `false` when there
/// are none, they were never prepared, or the kernel refused.  No file I/O:
/// one syscall at most.
#[cfg(target_os = "linux")]
fn pin_current_thread() -> bool {
    let Some(Some(set)) = FAST_CORES.get() else {
        return false;
    };
    // SAFETY: `set` is a valid `cpu_set_t` of the size passed; pid 0 is the
    // calling thread.
    unsafe { libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), set) == 0 }
}

#[cfg(not(target_os = "linux"))]
fn pin_current_thread() -> bool {
    false
}

/// The fast cores of this machine, within what the process may use, as a
/// set for `sched_setaffinity`.  `None` when the CPU is not hybrid, sysfs
/// says nothing usable, or the set would not narrow anything.
#[cfg(target_os = "linux")]
fn detect_fast_cores() -> Option<libc::cpu_set_t> {
    let fast = fast_cpus_in(std::path::Path::new(SYSFS_DEVICES))?;
    // SAFETY: an all-zero `cpu_set_t` is the empty set.
    let mut allowed: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    // SAFETY: valid out-pointer of the size passed; pid 0 is this thread.
    let got =
        unsafe { libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut allowed) };
    if got != 0 {
        return None;
    }
    let allowed: Vec<usize> = (0..MAX_CPUS)
        // SAFETY: `cpu < MAX_CPUS`, which fits the set (checked at compile
        // time below).
        .filter(|&cpu| unsafe { libc::CPU_ISSET(cpu, &allowed) })
        .collect();
    let keep = narrowing(&fast, &allowed)?;
    // SAFETY: as above.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    for cpu in keep {
        // SAFETY: every id in `keep` is below `MAX_CPUS` (`parse_cpu_list`).
        unsafe { libc::CPU_SET(cpu, &mut set) };
    }
    Some(set)
}

/// Where sysfs keeps the CPU devices.
#[cfg(target_os = "linux")]
const SYSFS_DEVICES: &str = "/sys/devices";

/// The most CPUs a `cpu_set_t` holds; a larger id in sysfs is not used.
#[cfg(target_os = "linux")]
const MAX_CPUS: usize = 1024;

#[cfg(target_os = "linux")]
const _: () = assert!(std::mem::size_of::<libc::cpu_set_t>() * 8 >= MAX_CPUS);

/// The most bytes read from one sysfs file.  A CPU list or a capacity is a
/// few dozen; anything longer is not one.
#[cfg(target_os = "linux")]
const SYSFS_READ_LIMIT: u64 = 4096;

/// The fast cores under `root` (normally `/sys/devices`), from the online
/// CPUs: the `cpu_core` PMU's CPUs on an Intel hybrid (only when `cpu_atom`
/// is there too), else the CPUs with the highest `cpu_capacity` when not all
/// share it.  `None` when nothing usable says which cores are fast, or every
/// online core is.
#[cfg(target_os = "linux")]
fn fast_cpus_in(root: &std::path::Path) -> Option<Vec<usize>> {
    let online = parse_cpu_list(&read_small(&root.join("system/cpu/online"))?)?;
    if online.len() < 2 {
        return None;
    }
    let intel = read_small(&root.join("cpu_core/cpus"))
        .and_then(|text| parse_cpu_list(&text))
        .zip(read_small(&root.join("cpu_atom/cpus")).and_then(|text| parse_cpu_list(&text)));
    if let Some((core, atom)) = intel {
        let fast: Vec<usize> = core
            .iter()
            .copied()
            .filter(|cpu| online.contains(cpu) && !atom.contains(cpu))
            .collect();
        if !fast.is_empty() && fast.len() < online.len() {
            return Some(fast);
        }
    }
    let mut capacities = Vec::with_capacity(online.len());
    for &cpu in &online {
        let text = read_small(&root.join(format!("system/cpu/cpu{cpu}/cpu_capacity")))?;
        capacities.push((cpu, text.trim().parse::<u64>().ok()?));
    }
    let top = capacities.iter().map(|&(_, capacity)| capacity).max()?;
    if top == 0 {
        return None;
    }
    let fast: Vec<usize> = capacities
        .iter()
        .filter(|&&(_, capacity)| capacity == top)
        .map(|&(cpu, _)| cpu)
        .collect();
    (fast.len() < online.len()).then_some(fast)
}

/// The fast cores the process may use, when they narrow `allowed` at all.
#[cfg(target_os = "linux")]
fn narrowing(fast: &[usize], allowed: &[usize]) -> Option<Vec<usize>> {
    let keep: Vec<usize> = fast
        .iter()
        .copied()
        .filter(|cpu| allowed.contains(cpu))
        .collect();
    (!keep.is_empty() && keep.len() < allowed.len()).then_some(keep)
}

/// A sysfs file, up to [`SYSFS_READ_LIMIT`] bytes of UTF-8.  `None` when it
/// is missing, unreadable, not text, or longer than that.
#[cfg(target_os = "linux")]
fn read_small(path: &std::path::Path) -> Option<String> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    let mut text = String::new();
    file.take(SYSFS_READ_LIMIT + 1)
        .read_to_string(&mut text)
        .ok()?;
    (text.len() as u64 <= SYSFS_READ_LIMIT).then_some(text)
}

/// A kernel CPU list (`0-3,8,10-11`), sorted and without repeats.  `None` for
/// anything else: an empty item, a reversed range, a non-number, or an id of
/// [`MAX_CPUS`] or more.  An empty list is `Some` and empty.
#[cfg(target_os = "linux")]
fn parse_cpu_list(text: &str) -> Option<Vec<usize>> {
    let text = text.trim();
    let mut cpus = Vec::new();
    if text.is_empty() {
        return Some(cpus);
    }
    for item in text.split(',') {
        let item = item.trim();
        let (first, last): (usize, usize) = match item.split_once('-') {
            Some((first, last)) => (first.trim().parse().ok()?, last.trim().parse().ok()?),
            None => {
                let cpu = item.parse().ok()?;
                (cpu, cpu)
            }
        };
        if first > last || last >= MAX_CPUS {
            return None;
        }
        cpus.extend(first..=last);
    }
    cpus.sort_unstable();
    cpus.dedup();
    Some(cpus)
}

#[cfg(target_os = "linux")]
fn promote_current_thread() -> bool {
    let Some(priority) = allowed_priority() else {
        return false;
    };
    // All-zero is a valid `sched_param` everywhere Linux libc defines one;
    // only the priority matters to `SCHED_FIFO`.  (musl's newer libc crate
    // layout adds private padding fields that a struct literal cannot name.)
    // SAFETY: `sched_param` is a plain C struct whose zero value is valid.
    let mut param: libc::sched_param = unsafe { std::mem::zeroed() };
    param.sched_priority = priority;
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
