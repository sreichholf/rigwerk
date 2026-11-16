//! What both halves of a loaded plugin share: [`PluginShared`] (atomics
//! only, so the audio thread, the UI and the plugin's own callbacks may all
//! touch it), the parameter id table [`ParamIds`], and the single UI→audio
//! parameter producer [`ParamOut`].  Format-neutral.
//!
//! Nothing here blocks the audio thread.  [`ParamOut`] has two mutexes, but
//! they are UI-side: a call from another thread (a plugin's `performEdit`
//! from its own or the audio thread) only `try_lock`s them and drops the
//! value when they are busy.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, TryLockError};
use std::thread::ThreadId;

use super::clamp_latency;
use super::pause::PauseHandshake;
use super::transfer::ParamSender;

/// `restart` bit for a CLAP `clap_host.request_restart`.  VST3 restart
/// flags use the low bits.
pub(crate) const RESTART_ACTIVATION: u32 = 1 << 31;

/// The state both halves of one loaded plugin and its host callbacks share.
/// Atomics only: no lock, no allocation, so any thread may call any method
/// (the comments say which thread is expected to).
pub(crate) struct PluginShared {
    pub(crate) pause: PauseHandshake,
    /// Clamped latency in samples, read after every activation (UI).
    latency: AtomicU32,
    /// A re-read moved `latency`; taken by the path service to republish.
    latency_unseen: AtomicBool,
    /// Restart work the UI owes the plugin: VST3 `RestartFlags` bits from
    /// `restartComponent`, or [`RESTART_ACTIVATION`] from CLAP.
    restart: AtomicU32,
    /// The insert needs a structural rebuild (see [`Self::want_reload`]).
    reload: AtomicBool,
    /// First processing failure, packed by [`ProcessFailure::pack`]; 0 is
    /// none.  Written once, by the audio thread.
    failure: AtomicU64,
    /// VST3 `IComponentHandler2::setDirty`, CLAP `state.mark_dirty`.
    state_dirty: AtomicBool,
}

impl PluginShared {
    pub(crate) fn new(latency: u32) -> Self {
        Self {
            pause: PauseHandshake::new(),
            latency: AtomicU32::new(clamp_latency(latency)),
            latency_unseen: AtomicBool::new(false),
            restart: AtomicU32::new(0),
            reload: AtomicBool::new(false),
            failure: AtomicU64::new(0),
            state_dirty: AtomicBool::new(false),
        }
    }

    pub(crate) fn latency(&self) -> u32 {
        self.latency.load(Ordering::Acquire)
    }

    /// Stores a freshly read latency (clamped).  Returns whether it moved,
    /// which also leaves it for [`Self::take_latency_change`].
    pub(crate) fn set_latency(&self, samples: u32) -> bool {
        let samples = clamp_latency(samples);
        let changed = self.latency.swap(samples, Ordering::AcqRel) != samples;
        if changed {
            self.latency_unseen.store(true, Ordering::Release);
        }
        changed
    }

    /// Whether the latency moved since the last call.
    pub(crate) fn take_latency_change(&self) -> bool {
        self.latency_unseen.swap(false, Ordering::AcqRel)
    }

    /// A plugin callback asks for restart work (any thread).
    pub(crate) fn request_restart(&self, bits: u32) {
        self.restart.fetch_or(bits, Ordering::AcqRel);
    }

    /// The restart work asked for so far, cleared (UI, once paused).
    pub(crate) fn take_restart(&self) -> u32 {
        self.restart.swap(0, Ordering::AcqRel)
    }

    pub(crate) fn restart_pending(&self) -> bool {
        self.restart.load(Ordering::Acquire) != 0
    }

    /// Records the first processing failure (audio thread; no allocation).
    /// A later one is ignored.
    pub(crate) fn fail(&self, failure: ProcessFailure) {
        let _ =
            self.failure
                .compare_exchange(0, failure.pack(), Ordering::AcqRel, Ordering::Acquire);
    }

    pub(crate) fn failure(&self) -> Option<ProcessFailure> {
        ProcessFailure::unpack(self.failure.load(Ordering::Acquire))
    }

    /// The plugin asks to be rebuilt (VST3 `kIoChanged` with other buses,
    /// `kReloadComponent`, changed parameter ids; CLAP changed controls or
    /// audio ports): its buffers or tables no longer fit, so it stops
    /// processing, and the path's next service frame builds a fresh
    /// instance from its state and values.  Any thread.
    pub(crate) fn want_reload(&self) {
        self.reload.store(true, Ordering::Release);
    }

    pub(crate) fn reload_wanted(&self) -> bool {
        self.reload.load(Ordering::Acquire)
    }

    pub(crate) fn mark_state_dirty(&self) {
        self.state_dirty.store(true, Ordering::Release);
    }

    pub(crate) fn state_dirty(&self) -> bool {
        self.state_dirty.load(Ordering::Acquire)
    }

    /// Clears the dirty mark before a state save, so a change during the
    /// save marks it again.
    pub(crate) fn take_state_dirty(&self) -> bool {
        self.state_dirty.swap(false, Ordering::AcqRel)
    }
}

/// Why a loaded insert stopped processing.  `Copy` and heap-free, so the
/// audio thread records it in [`PluginShared`] without allocating; the UI
/// writes the text ([`Self::message`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProcessFailure {
    /// VST3 `process` returned this `tresult`.
    Vst3(i32),
    /// VST3 `setProcessing(true)` returned this `tresult`.
    Vst3StartFailed(i32),
    /// A VST3 plugin did not activate again after a restart.
    Vst3Inactive,
    /// A plugin rebuilt on its own request asked again at once.
    ReloadLoop,
    Clap(ClapFailure),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClapFailure {
    NotActive,
    ProcessError,
    StartFailed,
}

impl ClapFailure {
    pub(crate) fn message(self) -> &'static str {
        match self {
            ClapFailure::NotActive => "The CLAP plugin is not active.",
            ClapFailure::ProcessError => "The CLAP plugin reported a processing error.",
            ClapFailure::StartFailed => "The CLAP plugin could not start processing.",
        }
    }

    fn code(self) -> u32 {
        match self {
            ClapFailure::NotActive => 0,
            ClapFailure::ProcessError => 1,
            ClapFailure::StartFailed => 2,
        }
    }

    fn from_code(code: u32) -> Option<Self> {
        Some(match code {
            0 => ClapFailure::NotActive,
            1 => ClapFailure::ProcessError,
            2 => ClapFailure::StartFailed,
            _ => return None,
        })
    }
}

/// Kinds in the high word of a packed failure; 0 is none.
const FAILURE_VST3: u64 = 1;
const FAILURE_VST3_START: u64 = 2;
const FAILURE_CLAP: u64 = 3;
const FAILURE_VST3_INACTIVE: u64 = 4;
const FAILURE_RELOAD_LOOP: u64 = 5;

impl ProcessFailure {
    /// The text the insert's error shows (UI thread).
    pub(crate) fn message(self) -> String {
        match self {
            ProcessFailure::Vst3(code) => format!("Plugin process() failed: {code:#x}"),
            ProcessFailure::Vst3StartFailed(code) => {
                format!("Plugin setProcessing() failed: {code:#x}")
            }
            ProcessFailure::Vst3Inactive => "The VST3 plugin is not active.".to_owned(),
            ProcessFailure::ReloadLoop => "The plugin keeps asking to be reloaded.".to_owned(),
            ProcessFailure::Clap(failure) => failure.message().to_owned(),
        }
    }

    fn pack(self) -> u64 {
        match self {
            ProcessFailure::Vst3(code) => FAILURE_VST3 << 32 | u64::from(code as u32),
            ProcessFailure::Vst3StartFailed(code) => {
                FAILURE_VST3_START << 32 | u64::from(code as u32)
            }
            ProcessFailure::Vst3Inactive => FAILURE_VST3_INACTIVE << 32,
            ProcessFailure::ReloadLoop => FAILURE_RELOAD_LOOP << 32,
            ProcessFailure::Clap(failure) => FAILURE_CLAP << 32 | u64::from(failure.code()),
        }
    }

    fn unpack(packed: u64) -> Option<Self> {
        let code = packed as u32;
        match packed >> 32 {
            FAILURE_VST3 => Some(ProcessFailure::Vst3(code as i32)),
            FAILURE_VST3_START => Some(ProcessFailure::Vst3StartFailed(code as i32)),
            FAILURE_VST3_INACTIVE => Some(ProcessFailure::Vst3Inactive),
            FAILURE_RELOAD_LOOP => Some(ProcessFailure::ReloadLoop),
            FAILURE_CLAP => ClapFailure::from_code(code).map(ProcessFailure::Clap),
            _ => None,
        }
    }
}

/// Index ↔ id for one loaded plugin, built at load and never changed.  The
/// index is the position in the plugin's parameter list.
pub(crate) struct ParamIds {
    ids: Box<[u32]>,
    /// `(id, index)`, sorted by id.
    by_id: Box<[(u32, u32)]>,
}

impl ParamIds {
    pub(crate) fn new(ids: Vec<u32>) -> Self {
        let mut by_id: Vec<(u32, u32)> = ids
            .iter()
            .enumerate()
            .map(|(index, id)| (*id, index as u32))
            .collect();
        by_id.sort_unstable();
        Self {
            ids: ids.into(),
            by_id: by_id.into(),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.ids.len()
    }

    pub(crate) fn id(&self, index: usize) -> Option<u32> {
        self.ids.get(index).copied()
    }

    /// Binary search; no allocation, so any thread may call it.
    pub(crate) fn index_of(&self, id: u32) -> Option<usize> {
        self.by_id
            .binary_search_by_key(&id, |(id, _)| *id)
            .ok()
            .map(|at| self.by_id[at].1 as usize)
    }
}

/// The single logical UI→audio parameter producer of one plugin: the panel
/// and a VST3 editor's `performEdit` both push here, serialised by the
/// sender mutex (UI-side only; the audio thread holds the receiver).
pub(crate) struct ParamOut {
    /// Cached at construction (the UI thread loads plugins), so a call
    /// never reads the process-wide main-thread record.
    main_thread: ThreadId,
    ids: Arc<ParamIds>,
    sender: Mutex<ParamSender>,
    /// Editor edits since the last [`Self::take_edits`]: the newest value
    /// per id.  Capacity is one per parameter, reserved here, so replacing
    /// by id never allocates.
    edits: Mutex<Vec<(u32, f64)>>,
    /// An edit from another thread was dropped since the last
    /// [`Self::take_edits`], which logs it (once) on the UI thread.
    dropped: AtomicBool,
    dropped_logged: AtomicBool,
}

impl ParamOut {
    pub(crate) fn new(ids: Arc<ParamIds>, sender: ParamSender) -> Self {
        let edits = Vec::with_capacity(ids.len());
        Self {
            main_thread: std::thread::current().id(),
            ids,
            sender: Mutex::new(sender),
            edits: Mutex::new(edits),
            dropped: AtomicBool::new(false),
            dropped_logged: AtomicBool::new(false),
        }
    }

    /// Queues a panel value for the audio side at offset 0 (UI thread).
    pub(crate) fn push(&self, index: usize, value: f64) -> bool {
        self.sender
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(index, value, 0)
    }

    /// An editor edit (`performEdit`): queued for the audio side and kept
    /// for the next [`Self::take_edits`].  From any other thread than the
    /// UI it only try-locks and drops the value when busy, so a plugin that
    /// edits from its audio thread never blocks it.
    pub(crate) fn push_edit(&self, id: u32, value: f64) -> bool {
        let Some(index) = self.ids.index_of(id) else {
            return false;
        };
        let main = std::thread::current().id() == self.main_thread;
        let pushed = match lock_or_try(&self.sender, main) {
            Some(mut sender) => sender.push(index, value, 0),
            None => false,
        };
        if !pushed {
            if !main {
                // No logging here: this may be the audio thread.
                self.dropped.store(true, Ordering::Relaxed);
            }
            return false;
        }
        if let Some(mut edits) = lock_or_try(&self.edits, main) {
            if let Some(edit) = edits.iter_mut().find(|(edited, _)| *edited == id) {
                edit.1 = value;
            } else if edits.len() < edits.capacity() {
                // Ids are distinct and the capacity is one per id, so this
                // never grows the vector.
                edits.push((id, value));
            }
        }
        true
    }

    /// Moves the edits since the last call into `out` (cleared first).
    pub(crate) fn take_edits(&self, out: &mut Vec<(u32, f64)>) {
        if self.dropped.swap(false, Ordering::Relaxed)
            && !self.dropped_logged.swap(true, Ordering::Relaxed)
        {
            eprintln!("[plugins] dropped a parameter edit made off the UI thread");
        }
        out.clear();
        let mut edits = self.edits.lock().unwrap_or_else(PoisonError::into_inner);
        out.append(&mut edits);
    }

    /// Whether a pushed value has not reached the audio side yet.
    pub(crate) fn has_queued(&self) -> bool {
        self.sender
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .has_queued()
    }
}

/// Locks on the UI thread; only try-locks elsewhere.
fn lock_or_try<T>(mutex: &Mutex<T>, block: bool) -> Option<std::sync::MutexGuard<'_, T>> {
    if block {
        return Some(mutex.lock().unwrap_or_else(PoisonError::into_inner));
    }
    match mutex.try_lock() {
        Ok(guard) => Some(guard),
        Err(TryLockError::Poisoned(poison)) => Some(poison.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}

#[cfg(test)]
mod tests;
