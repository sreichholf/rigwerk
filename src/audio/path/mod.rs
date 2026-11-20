//! One signal path's controls, as the UI writes them and the audio
//! callback reads them: input routing, mono, channel enable, the two
//! built-in EQs, the gate, the NAM blocks, the plugin effect chain, the
//! cabinet and the path meters.
//!
//! [`Shared`](super::shared::Shared) holds [`MAX_PATHS`] of these as fixed
//! slots, all preallocated, so adding a path never allocates in a callback.
//! Each slot has a [`PathState`]; a slot keeps its index for its whole life,
//! so removing one path never moves the chains of the others, and a removed
//! path can keep running while it fades out.  Every path splits off where
//! the chain starts, after the clock side (ring, resampler, drift); see
//! `mixer::PathProcessor` for the realtime half.
//!
//! # Lifecycle
//!
//! Paths are added, duplicated and removed live, from the UI thread, without
//! restarting the bridge ([`Shared`](super::shared::Shared)`::add_path`,
//! `duplicate_path`, `remove_path`):
//! - add and duplicate take a free ([`PathState::Inactive`]) slot, set it up
//!   while the mixer still skips it, then make it active.  That bumps the
//!   slot's generation, so the mixer starts its chain fresh, fading the
//!   path in.  Both start muted, so the sum never jumps, and the mixer
//!   keeps a path silent while its restored plugins are not open yet.
//! - remove moves the slot to [`PathState::FadingOut`].  The mixer keeps
//!   running it with its path gain ramping to silence and then moves it to
//!   [`PathState::Inactive`] itself (one compare-exchange, from the audio
//!   thread).  With no bridge running, the UI thread finishes the removal
//!   (`Shared::service_paths`), and releases the slot's plugins.
//! - at least one path is always active.
//!
//! # Level, mute, solo
//!
//! Each path has a level (dB), a mute and a solo, applied after its chain
//! and before the master, as one ramped gain.  If any active path is
//! soloed, only soloed paths sound.
//!
//! # Latency
//!
//! [`PathControls::latency_samples`] is the chain's total plugin latency,
//! and the chain really delays by exactly that much at all times: an
//! effect skipped for a block plays its input delayed by its latency, and
//! an effect's dry/wet mix delays its dry side (see `ChainRuntime::process`).
//! A path-level chain bypass skips every effect the same way, and the total
//! still counts.  The mixer lines the paths up per device with it (`output_map`,
//! "Alignment").  The NAM blocks add no latency.  The cabinet adds none
//! either: a short direct head plus partitioned overlap-save, and a delay
//! that is the impulse itself is not reported here.
//!
//! The plugin-chain side of [`PathControls`] (edits, publish, controls,
//! servicing, state) is in `plugins`, with how a chain is handed between
//! the UI and the audio thread ("Chain handover").

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use std::time::Instant;

use super::cab::{CabCommand, CabControls, CabKernel, CabPark};
use super::eq::{EqSettings, EqSlot};
use super::gate::GateSettings;
use super::nam::{NamCommand, NamPark, NamPending, NamStack, PreparedNam};
use super::output_map::level_gain;
use super::plugins::{ChainHome, ChainModel, PluginView, SlotLink};

mod plugins;

#[cfg(test)]
mod tests;

/// Most signal paths a session can hold.  Every per-path array (controls,
/// processors, device maps) is this long; only the active ones run.
pub const MAX_PATHS: usize = 4;

/// Range of a path's level, both the path level itself and its level on
/// one device, dB.  At or below the minimum the path is silent (−∞).
pub const PATH_LEVEL_MIN_DB: f32 = -60.0;
pub const PATH_LEVEL_MAX_DB: f32 = 6.0;

/// A NaN path level becomes 0 dB; the level is clamped to
/// [`PATH_LEVEL_MIN_DB`]..=[`PATH_LEVEL_MAX_DB`].
pub fn sanitize_path_level(level_db: f32) -> f32 {
    if level_db.is_nan() {
        0.0
    } else {
        level_db.clamp(PATH_LEVEL_MIN_DB, PATH_LEVEL_MAX_DB)
    }
}

/// Linear gain of a path level: exactly 1.0 at 0 dB, silence at or below
/// [`PATH_LEVEL_MIN_DB`].
pub(crate) fn path_level_gain(level_db: f32) -> f32 {
    let level_db = sanitize_path_level(level_db);
    if level_db <= PATH_LEVEL_MIN_DB {
        0.0
    } else {
        level_gain(level_db)
    }
}

/// The settings of one path the UI owns and pushes: input routing, mono,
/// channel enable, the two EQs, the gate, the cabinet, the path level, mute
/// and solo, and the effect-chain bypass.  The NAM blocks are pushed
/// separately ([`PathControls::set_nam`]): sixteen of them do not fit a
/// snapshot the mixer copies as one [`Copy`] value.  The plugin chain, the
/// NAM captures, and the cabinet kernel live in the engine
/// ([`PathControls`]); neither file path is copied onto the audio thread.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PathMix {
    /// The 2 × 4 input routing (input × tap).  Only taps 1/2 (the path's
    /// L/R) are used; 3/4 are kept for the stored layout.
    pub routes: [[bool; 4]; 2],
    pub mono: bool,
    /// Channel enable for the path's L and R.
    pub output_enabled: [bool; 2],
    /// The EQs before and after the effects, indexed by [`EqSlot`].
    pub eqs: [EqSettings; 2],
    pub gate: GateSettings,
    /// Built-in cabinet.  The impulse path is not here: this stays [`Copy`].
    pub cab: CabControls,
    /// Path level, dB (see `PATH_LEVEL_MIN_DB` / `PATH_LEVEL_MAX_DB`).
    pub level_db: f32,
    pub mute: bool,
    pub solo: bool,
    /// Skips every effect's processing without changing each effect's own
    /// bypass.  Latency stays counted.
    pub chain_bypass: bool,
}

impl Default for PathMix {
    fn default() -> Self {
        Self {
            routes: [[true, false, false, false], [false, true, false, false]],
            mono: false,
            output_enabled: [true, true],
            eqs: [EqSettings::default(), EqSettings::default()],
            gate: GateSettings::default(),
            cab: CabControls::default(),
            level_db: 0.0,
            mute: false,
            solo: false,
            chain_bypass: false,
        }
    }
}

impl PathMix {
    /// Every value in range (EQ, gate and level).
    pub fn sanitized(self) -> Self {
        Self {
            eqs: self.eqs.map(EqSettings::sanitized),
            gate: self.gate.sanitized(),
            cab: self.cab.sanitized(),
            level_db: sanitize_path_level(self.level_db),
            ..self
        }
    }
}

/// Where one path slot is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PathState {
    /// Unused: the mixer skips it.
    Inactive = 0,
    /// Playing.
    Active = 1,
    /// Removed, but still running until its fade-out ends.
    FadingOut = 2,
}

impl PathState {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Active,
            2 => Self::FadingOut,
            _ => Self::Inactive,
        }
    }

    /// Whether the mixer runs a slot in this state.
    pub(crate) fn is_live(self) -> bool {
        self != Self::Inactive
    }
}

/// The slots the mixer runs in one block, in slot order.  Read once at the
/// start of a block, so a UI change mid-block cannot change the set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct LiveSlots {
    slots: [u8; MAX_PATHS],
    count: u8,
}

impl LiveSlots {
    /// Just slot 0: the one path every run has had so far.
    #[cfg(test)]
    pub(crate) fn main() -> Self {
        Self {
            slots: [0; MAX_PATHS],
            count: 1,
        }
    }

    /// The live slots among `states`, in slot order.
    pub(crate) fn from_states(states: impl IntoIterator<Item = PathState>) -> Self {
        let mut live = Self::default();
        for (slot, state) in states.into_iter().enumerate().take(MAX_PATHS) {
            if state.is_live() {
                live.slots[live.count as usize] = slot as u8;
                live.count += 1;
            }
        }
        live
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.slots[..self.count as usize]
            .iter()
            .map(|slot| *slot as usize)
    }

    pub(crate) fn contains(&self, slot: usize) -> bool {
        self.iter().any(|live| live == slot)
    }

    /// The live slots as one flag per slot.
    pub(crate) fn mask(&self) -> [bool; MAX_PATHS] {
        let mut mask = [false; MAX_PATHS];
        for slot in self.iter() {
            mask[slot] = true;
        }
        mask
    }
}

/// A DSP time per block, as the mixer publishes it: one path's, or every
/// live path's together (`Shared::total_dsp`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DspTime {
    /// Smoothed time on the path's chain per block, ns.
    pub avg_ns: u32,
    /// The longest recent block, decaying over about two seconds, ns.
    pub peak_ns: u32,
    /// The block's length: the budget, ns.
    pub block_ns: u32,
}

impl DspTime {
    /// The smoothed time as a share of the block (0 without a block).
    pub fn avg_share(&self) -> f32 {
        self.share(self.avg_ns)
    }

    /// The peak as a share of the block.
    pub fn peak_share(&self) -> f32 {
        self.share(self.peak_ns)
    }

    fn share(&self, ns: u32) -> f32 {
        if self.block_ns == 0 {
            0.0
        } else {
            ns as f32 / self.block_ns as f32
        }
    }
}

/// How often the audio thread tried one piece of plugin work and how often
/// it skipped it for that chunk.  For the chain a skip is a busy chain
/// lock; for an effect it is a pause (a restart, a stopped or failed
/// start), since no effect has a lock the UI could hold.  Measurement only:
/// the audio thread writes these relaxedly and never reads them back.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LockStats {
    pub attempts: u64,
    pub skips: u64,
}

impl LockStats {
    /// The skips, never more than the attempts: a reset racing the audio
    /// thread can zero the attempts after a skip was counted.
    pub fn skipped(&self) -> u64 {
        self.skips.min(self.attempts)
    }

    /// The skips as a share of the attempts (0 before any attempt).
    pub fn share(&self) -> f32 {
        if self.attempts == 0 {
            0.0
        } else {
            self.skipped() as f32 / self.attempts as f32
        }
    }
}

/// One path's plugin counters: the chunks the chain missed and each
/// effect's pauses.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PluginLockReport {
    pub chain: LockStats,
    pub effects: Vec<LockStats>,
    /// A chain edit panicked, so the chain is unused for the rest of the
    /// run (see `PathControls::chain_poisoned`).  The skip counters then
    /// describe nothing, and Diagnostics says so instead of showing a rate.
    pub chain_poisoned: bool,
}

/// A [`DspTime`] as atomics, so the audio thread can publish it without a
/// lock.  Each field is stored on its own (`Relaxed`): a reader may see one
/// block's average beside the next block's peak, which a meter can afford.
#[derive(Debug, Default)]
pub(crate) struct DspCell {
    avg_ns: AtomicU32,
    peak_ns: AtomicU32,
    block_ns: AtomicU32,
}

impl DspCell {
    pub(crate) fn load(&self) -> DspTime {
        DspTime {
            avg_ns: self.avg_ns.load(Ordering::Relaxed),
            peak_ns: self.peak_ns.load(Ordering::Relaxed),
            block_ns: self.block_ns.load(Ordering::Relaxed),
        }
    }

    /// Called by the output callback at the end of each block.
    pub(crate) fn store(&self, time: DspTime) {
        self.avg_ns.store(time.avg_ns, Ordering::Relaxed);
        self.peak_ns.store(time.peak_ns, Ordering::Relaxed);
        self.block_ns.store(time.block_ns, Ordering::Relaxed);
    }
}

pub(crate) struct PathControls {
    /// The 2 × 4 input routing (input × tap), row-major.
    pub(crate) routes: [AtomicBool; 8],
    pub(crate) mono: AtomicBool,
    /// Channel enable for the two mix sides (taps 1/2).
    pub(crate) output_enabled: [AtomicBool; 2],
    /// The slot's [`PathState`], as `u8`.
    state: AtomicU8,
    /// Bumped each time the slot becomes active, so the mixer starts that
    /// path's chain (EQ and gate state, ramps) fresh instead of carrying
    /// over whatever the slot played before.
    generation: AtomicU64,
    /// Path level as a linear gain (`f32` bits), mute and solo.
    level_gain: AtomicU32,
    level_db: AtomicU32,
    mute: AtomicBool,
    solo: AtomicBool,
    /// When the UI removed this path (UI thread only; the audio thread
    /// never takes it).  Lets the UI finish a fade no mixer is running.
    removed_at: Mutex<Option<Instant>>,
    /// The slot's plugins still need releasing after a removal.
    retired: AtomicBool,
    /// Block peaks of the path's output (`f32` bits), before the master.
    meter_l: AtomicU32,
    meter_r: AtomicU32,
    /// The gate's deepest gain reduction in the last block, dB (`f32`
    /// bits), 0 while it is open or off.  Written with the meters.
    gate_reduction_db: AtomicU32,
    /// How long the mixer spends on this path's chain per block, ns
    /// (smoothed, and a decaying peak), and how long that block lasts, ns:
    /// its DSP budget.  Written by the mixer at the end of each block (see
    /// `mixer::PathProcessor::publish_dsp`), read by Diagnostics and Live.
    dsp: DspCell,
    /// Each effect's mix, bypass and plugin handle, published with the
    /// views, so the UI sets mix, bypass and plugin controls, services the
    /// plugins, captures their state and saves them without the model lock
    /// (see `plugins`, "Chain handover").
    ///
    /// Declared before `chain_home` on purpose: fields drop in order, so
    /// these extra references are gone before the runtime hands its slots
    /// to the deferred queue, and the main thread's drain is always the
    /// last owner.
    pub(super) slot_links: Mutex<Arc<[SlotLink]>>,
    /// The effect views the UI draws.  Republished by every chain edit, and
    /// read without the model lock.  The audio thread never takes this lock,
    /// so only UI-side readers and writers contend on it, for an `Arc`
    /// clone.
    plugin_views: Mutex<Arc<[PluginView]>>,
    plugin_views_version: AtomicU64,
    /// Set by the audio thread when an effect fails or the chain latency
    /// moved; the UI thread rebuilds the views from the links in
    /// `service_plugins`, so the audio thread never builds them.
    plugin_views_dirty: AtomicBool,
    chain_rate: AtomicU32,
    chain_unopened: AtomicBool,
    /// The effect chain as the UI edits it, and the only sender of its
    /// commands (see `plugins`, "Chain handover").  A UI-only lock: the
    /// audio thread never takes it.
    pub(super) chain: Mutex<ChainModel>,
    /// Where the chain's runtime waits while no audio thread holds it.
    chain_home: Arc<ChainHome>,
    /// The chain's total latency, read by the mixer.  The runtime's holder
    /// stores it at the start of every block; a chain edit stores the total
    /// of its links.
    latency: AtomicU32,
    /// Path-level effect bypass.  The runtime loads it every block; the UI
    /// stores it.  It does not write each slot's bypass.
    chain_bypass: Arc<AtomicBool>,
    plugin_active: AtomicUsize,
    /// The UI wants the next block to enter the chain even with nothing
    /// active (a bypass cleared, a value queued for a bypassed effect).
    /// Its own flag, taken by the audio thread, so the audio thread's
    /// recount of `plugin_active` cannot overwrite a wake.
    plugin_wake: AtomicBool,
    /// How often the audio thread tried to run this path's chain and how
    /// often it did not hold the runtime, so the chunk played the chain's
    /// input delayed instead.
    /// Measurement only: written relaxedly by the audio thread.
    chain_attempts: AtomicU64,
    chain_skips: AtomicU64,
    /// Set when a chain edit panicked: the model may no longer match the
    /// runtime, so the chain stays unused for the rest of the run.  It
    /// is not contention and never clears.
    chain_poisoned: AtomicBool,
    /// The two built-in EQs (pre and post effects).  Each version counter
    /// lets the audio callback notice a change without taking the lock.
    eq: [Mutex<EqSettings>; 2],
    eq_version: [AtomicU64; 2],
    /// Built-in noise gate (PRE or POST effects).
    gate: Mutex<GateSettings>,
    gate_version: AtomicU64,
    /// Built-in NAM blocks, in signal order.  The callback copies the whole
    /// stack with `try_lock`, and only when its version moved.  The captures
    /// themselves are [`pending_nam`], never this lock.
    nam: Mutex<NamStack>,
    nam_version: AtomicU64,
    /// A model the UI has prepared, waiting for the next block, keyed by the
    /// block it is for.  The callback only `try_lock`s this.  Dropping a
    /// replaced model happens on the UI thread, in [`Self::queue_nam`].
    pending_nam: Mutex<NamPending>,
    /// Where a model the audio side has let go of waits to be dropped.
    /// Shared with the engine so a bridge-thread drop can park it.
    nam_park: Arc<NamPark>,
    /// Built-in cabinet controls.  The callback copies them with `try_lock`.
    /// The kernel itself is [`pending_cab`], never this lock.
    cab: Mutex<CabControls>,
    cab_version: AtomicU64,
    /// A kernel the UI has prepared, waiting for the next block.  The
    /// callback only `try_lock`s this.  Dropping a replaced kernel happens
    /// on the UI thread, in [`Self::queue_cab`].
    pending_cab: Mutex<Option<CabCommand>>,
    /// Where a kernel the audio side has let go of waits to be dropped.
    /// Shared with the engine so a bridge-thread drop can park it.
    cab_park: Arc<CabPark>,
}

impl PathControls {
    /// A path with its own park.  Tests that never queue a model use this;
    /// a run shares one park via [`Self::with_park`].
    #[cfg(test)]
    pub(crate) fn new(sample_rate: u32) -> Self {
        Self::with_park(sample_rate, NamPark::new(), CabPark::new())
    }

    pub(crate) fn with_park(
        sample_rate: u32,
        nam_park: Arc<NamPark>,
        cab_park: Arc<CabPark>,
    ) -> Self {
        let chain_bypass = Arc::new(AtomicBool::new(false));
        let chain = ChainModel::new(sample_rate, Arc::clone(&chain_bypass));
        Self {
            routes: [
                AtomicBool::new(true),
                AtomicBool::new(false),
                AtomicBool::new(false),
                AtomicBool::new(false),
                AtomicBool::new(false),
                AtomicBool::new(true),
                AtomicBool::new(false),
                AtomicBool::new(false),
            ],
            mono: AtomicBool::new(false),
            output_enabled: [AtomicBool::new(true), AtomicBool::new(true)],
            state: AtomicU8::new(PathState::Inactive as u8),
            generation: AtomicU64::new(0),
            level_gain: AtomicU32::new(1.0f32.to_bits()),
            level_db: AtomicU32::new(0.0f32.to_bits()),
            mute: AtomicBool::new(false),
            solo: AtomicBool::new(false),
            removed_at: Mutex::new(None),
            retired: AtomicBool::new(false),
            meter_l: AtomicU32::new(0.0f32.to_bits()),
            meter_r: AtomicU32::new(0.0f32.to_bits()),
            gate_reduction_db: AtomicU32::new(0.0f32.to_bits()),
            dsp: DspCell::default(),
            slot_links: Mutex::new(Arc::from(Vec::new())),
            plugin_views: Mutex::new(Arc::from(Vec::new())),
            plugin_views_version: AtomicU64::new(0),
            plugin_views_dirty: AtomicBool::new(false),
            chain_rate: AtomicU32::new(chain.rate()),
            chain_unopened: AtomicBool::new(false),
            chain_home: chain.home(),
            chain: Mutex::new(chain),
            latency: AtomicU32::new(0),
            chain_bypass,
            plugin_active: AtomicUsize::new(0),
            plugin_wake: AtomicBool::new(false),
            chain_attempts: AtomicU64::new(0),
            chain_skips: AtomicU64::new(0),
            chain_poisoned: AtomicBool::new(false),
            eq: [
                Mutex::new(EqSettings::default()),
                Mutex::new(EqSettings::default()),
            ],
            eq_version: [AtomicU64::new(1), AtomicU64::new(1)],
            gate: Mutex::new(GateSettings::default()),
            gate_version: AtomicU64::new(1),
            nam: Mutex::new(NamStack::default()),
            nam_version: AtomicU64::new(1),
            pending_nam: Mutex::new(NamPending::default()),
            nam_park,
            cab: Mutex::new(CabControls::default()),
            cab_version: AtomicU64::new(1),
            pending_cab: Mutex::new(None),
            cab_park,
        }
    }

    /// Copies a previous run's settings for this path.  Live plugin
    /// instances belong to the run that opened them; only the saved
    /// preferences move, and the chain rebuilds from them.  Call it from the
    /// UI thread.
    pub(crate) fn adopt_settings_from(&self, previous: &PathControls) {
        let copy_bool = |to: &AtomicBool, from: &AtomicBool| {
            to.store(from.load(Ordering::Relaxed), Ordering::Relaxed);
        };
        for (to, from) in self.routes.iter().zip(&previous.routes) {
            copy_bool(to, from);
        }
        copy_bool(&self.mono, &previous.mono);
        for (to, from) in self.output_enabled.iter().zip(&previous.output_enabled) {
            copy_bool(to, from);
        }
        self.set_level_db(previous.level_db());
        copy_bool(&self.mute, &previous.mute);
        copy_bool(&self.solo, &previous.solo);
        self.set_chain_bypass(previous.chain_bypassed());
        // A fade-out does not outlive its run.
        self.set_state(match previous.state() {
            PathState::Active => PathState::Active,
            PathState::Inactive | PathState::FadingOut => PathState::Inactive,
        });
        for slot in EqSlot::ALL {
            let settings = previous.eq[slot.index()]
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            self.set_eq(slot, *settings);
        }
        {
            let settings = previous.gate.lock().unwrap_or_else(PoisonError::into_inner);
            self.set_gate(*settings);
        }
        {
            // The block settings move.  The live models stay with the run
            // that loaded them; the UI queues them again from the saved path.
            let stack = previous.nam.lock().unwrap_or_else(PoisonError::into_inner);
            self.set_nam(&stack);
        }
        {
            // The numeric cabinet controls move.  The live kernel stays with
            // the run that loaded it; the UI installs it again from the saved path.
            let settings = previous.cab.lock().unwrap_or_else(PoisonError::into_inner);
            self.set_cab(*settings);
        }
        // Only an active path carries its chain over; a removed one starts
        // the new run empty.  Capture the old instances' state on this
        // (UI) thread first, so the rebuilt chain reopens with what the
        // person set since the last save.
        if previous.state() == PathState::Active {
            self.restore_plugins(previous.plugin_prefs());
        } else {
            self.restore_plugins(Vec::new());
        }
    }

    pub(crate) fn state(&self) -> PathState {
        PathState::from_u8(self.state.load(Ordering::Acquire))
    }

    /// Moves the slot to `state`.  Becoming active from inactive bumps the
    /// generation, so the chain starts fresh.  Removing (fading out) does
    /// not: the path keeps its EQ and gate state while it fades, and a path
    /// restored during its fade-out carries on without a jump.
    pub(crate) fn set_state(&self, state: PathState) {
        if state == PathState::Active && self.state() == PathState::Inactive {
            // Only this (UI) thread moves a slot out of Inactive, so the
            // slot stays Inactive until the store below.  The generation is
            // bumped first: a mixer that sees the slot live (state, then
            // generation, both Acquire) always sees the new generation and
            // never plays a block with the slot's old chain state.
            self.generation.fetch_add(1, Ordering::Release);
            self.state.store(state as u8, Ordering::Release);
            return;
        }
        let previous = PathState::from_u8(self.state.swap(state as u8, Ordering::AcqRel));
        if state == PathState::Active && previous == PathState::Inactive {
            // A fade-out the mixer finished just now: start fresh anyway.
            self.generation.fetch_add(1, Ordering::Release);
        }
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Ends a fade-out: called by the mixer once the path's gain reached
    /// silence.  A path the UI restored meanwhile stays active.
    pub(crate) fn finish_fade(&self) {
        let _ = self.state.compare_exchange(
            PathState::FadingOut as u8,
            PathState::Inactive as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    /// Starts removing this path (UI thread): it fades out, then goes
    /// inactive.
    pub(crate) fn begin_removal(&self) {
        *self
            .removed_at
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Instant::now());
        self.retired.store(true, Ordering::Release);
        self.set_state(PathState::FadingOut);
    }

    /// Forgets a removal (the slot is being reused or restored).
    pub(crate) fn clear_removal(&self) {
        *self
            .removed_at
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
        self.retired.store(false, Ordering::Release);
    }

    /// Pretends the removal started `ago` earlier, for the overdue path.
    #[cfg(test)]
    pub(crate) fn backdate_removal(&self, ago: std::time::Duration) {
        if let Some(at) = self
            .removed_at
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_mut()
        {
            *at -= ago;
        }
    }

    /// Whether some effect was restored but not opened yet (lock-free).
    /// The mixer keeps such a path silent until the UI thread opens it.
    pub(crate) fn chain_pending(&self) -> bool {
        self.chain_unopened.load(Ordering::Relaxed)
    }

    /// When [`Self::begin_removal`] ran, while the path is fading out.
    pub(crate) fn removed_at(&self) -> Option<Instant> {
        *self
            .removed_at
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Releases a removed path's plugins once its fade is over (UI thread).
    /// Returns whether anything was released.
    pub(crate) fn release_if_retired(&self) -> bool {
        if self.state() != PathState::Inactive || !self.retired.load(Ordering::Acquire) {
            return false;
        }
        self.clear_removal();
        self.restore_plugins(Vec::new());
        true
    }

    /// Sets every UI-owned setting at once.
    pub(crate) fn set_mix(&self, mix: &PathMix) {
        self.set_routes(mix.routes);
        self.set_mono(mix.mono);
        self.set_output_enabled(mix.output_enabled);
        for slot in EqSlot::ALL {
            self.set_eq(slot, mix.eqs[slot.index()]);
        }
        self.set_gate(mix.gate);
        self.set_cab(mix.cab);
        self.set_level_db(mix.level_db);
        self.set_mute(mix.mute);
        self.set_solo(mix.solo);
        self.set_chain_bypass(mix.chain_bypass);
    }

    /// The UI-owned settings as stored now.
    pub(crate) fn mix(&self) -> PathMix {
        let mut routes = [[false; 4]; 2];
        for (input, row) in routes.iter_mut().enumerate() {
            for (tap, on) in row.iter_mut().enumerate() {
                *on = self.route(input, tap);
            }
        }
        let eq = |slot: EqSlot| {
            *self.eq[slot.index()]
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
        };
        PathMix {
            routes,
            mono: self.mono.load(Ordering::Relaxed),
            output_enabled: [
                self.output_enabled[0].load(Ordering::Relaxed),
                self.output_enabled[1].load(Ordering::Relaxed),
            ],
            eqs: [eq(EqSlot::Pre), eq(EqSlot::Post)],
            gate: *self.gate.lock().unwrap_or_else(PoisonError::into_inner),
            cab: *self.cab.lock().unwrap_or_else(PoisonError::into_inner),
            level_db: self.level_db(),
            mute: self.muted(),
            solo: self.soloed(),
            chain_bypass: self.chain_bypassed(),
        }
    }

    pub(crate) fn set_level_db(&self, level_db: f32) {
        let level_db = sanitize_path_level(level_db);
        self.level_db.store(level_db.to_bits(), Ordering::Relaxed);
        self.level_gain
            .store(path_level_gain(level_db).to_bits(), Ordering::Relaxed);
    }

    pub(crate) fn level_db(&self) -> f32 {
        f32::from_bits(self.level_db.load(Ordering::Relaxed))
    }

    /// The path level as a linear gain: exactly 1.0 at 0 dB.
    pub(crate) fn level_gain(&self) -> f32 {
        f32::from_bits(self.level_gain.load(Ordering::Relaxed))
    }

    pub(crate) fn set_mute(&self, mute: bool) {
        self.mute.store(mute, Ordering::Relaxed);
    }

    pub(crate) fn muted(&self) -> bool {
        self.mute.load(Ordering::Relaxed)
    }

    pub(crate) fn set_solo(&self, solo: bool) {
        self.solo.store(solo, Ordering::Relaxed);
    }

    pub(crate) fn soloed(&self) -> bool {
        self.solo.load(Ordering::Relaxed)
    }

    /// Skips every effect's processing.  The audio thread reads the same
    /// atomic inside the chain.  Each slot's own bypass is not written.
    pub(crate) fn set_chain_bypass(&self, bypass: bool) {
        self.chain_bypass.store(bypass, Ordering::Relaxed);
    }

    pub(crate) fn chain_bypassed(&self) -> bool {
        self.chain_bypass.load(Ordering::Relaxed)
    }

    pub(crate) fn set_routes(&self, routes: [[bool; 4]; 2]) {
        for (input, row) in routes.iter().enumerate() {
            for (output, enabled) in row.iter().enumerate() {
                self.routes[input * 4 + output].store(*enabled, Ordering::Relaxed);
            }
        }
    }

    /// Whether input `input` reaches tap `tap`.
    pub(crate) fn route(&self, input: usize, tap: usize) -> bool {
        self.routes[input * 4 + tap].load(Ordering::Relaxed)
    }

    pub(crate) fn set_mono(&self, mono: bool) {
        self.mono.store(mono, Ordering::Relaxed);
    }

    pub(crate) fn set_output_enabled(&self, channels: [bool; 2]) {
        self.output_enabled[0].store(channels[0], Ordering::Relaxed);
        self.output_enabled[1].store(channels[1], Ordering::Relaxed);
    }

    /// The path meters: the last block's peaks after the path's own chain,
    /// before the master.
    pub(crate) fn meters(&self) -> (f32, f32) {
        (
            f32::from_bits(self.meter_l.load(Ordering::Relaxed)),
            f32::from_bits(self.meter_r.load(Ordering::Relaxed)),
        )
    }

    /// Called by the output callback at the end of each block.
    pub(crate) fn store_meters(&self, left: f32, right: f32) {
        self.meter_l.store(left.to_bits(), Ordering::Relaxed);
        self.meter_r.store(right.to_bits(), Ordering::Relaxed);
    }

    /// The gate's deepest gain reduction in the last block, in dB: 0 while
    /// it is open or off, up to its range while it is closed.
    pub(crate) fn gate_reduction_db(&self) -> f32 {
        f32::from_bits(self.gate_reduction_db.load(Ordering::Relaxed))
    }

    pub(crate) fn store_gate_reduction(&self, db: f32) {
        self.gate_reduction_db
            .store(db.to_bits(), Ordering::Relaxed);
    }

    pub(crate) fn reset_meters(&self) {
        self.store_meters(0.0, 0.0);
        self.store_gate_reduction(0.0);
        self.store_dsp_time(DspTime::default());
    }

    /// The path's DSP time per block and the block's length (all zero
    /// until a mixer has run it).
    pub(crate) fn dsp_time(&self) -> DspTime {
        self.dsp.load()
    }

    /// Called by the output callback at the end of each block.
    pub(crate) fn store_dsp_time(&self, time: DspTime) {
        self.dsp.store(time);
    }

    /// Applies new EQ settings; unchanged settings cost nothing.
    pub(crate) fn set_eq(&self, slot: EqSlot, settings: EqSettings) {
        if let Ok(mut current) = self.eq[slot.index()].lock() {
            if *current != settings {
                *current = settings;
                self.eq_version[slot.index()].fetch_add(1, Ordering::Release);
            }
        }
    }

    pub(crate) fn eq_version(&self, slot: EqSlot) -> u64 {
        self.eq_version[slot.index()].load(Ordering::Acquire)
    }

    /// The current settings, or `None` if the UI holds the lock right now
    /// (the audio callback then simply tries again next block).
    pub(crate) fn try_eq(&self, slot: EqSlot) -> Option<EqSettings> {
        self.eq[slot.index()]
            .try_lock()
            .ok()
            .map(|settings| *settings)
    }

    pub(crate) fn set_gate(&self, settings: GateSettings) {
        if let Ok(mut current) = self.gate.lock() {
            if *current != settings {
                *current = settings;
                self.gate_version.fetch_add(1, Ordering::Release);
            }
        }
    }

    pub(crate) fn gate_version(&self) -> u64 {
        self.gate_version.load(Ordering::Acquire)
    }

    pub(crate) fn try_gate(&self) -> Option<GateSettings> {
        self.gate.try_lock().ok().map(|settings| *settings)
    }

    /// Applies the NAM blocks.  Does not parse or build a model.
    pub(crate) fn set_nam(&self, stack: &NamStack) {
        let stack = stack.sanitized();
        if let Ok(mut current) = self.nam.lock() {
            if *current != stack {
                *current = stack;
                self.nam_version.fetch_add(1, Ordering::Release);
            }
        }
    }

    pub(crate) fn nam_version(&self) -> u64 {
        self.nam_version.load(Ordering::Acquire)
    }

    /// The current blocks, or `None` if the UI holds the lock right now.
    pub(crate) fn try_nam(&self) -> Option<NamStack> {
        self.nam.try_lock().ok().map(|stack| *stack)
    }

    /// The blocks as stored now (the UI thread).
    pub(crate) fn nam(&self) -> NamStack {
        *self.nam.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn nam_park(&self) -> &Arc<NamPark> {
        &self.nam_park
    }

    /// Queues a prewarmed model for block `id` for the next block.  A model
    /// still waiting for that block is dropped here, on the UI thread.
    pub(crate) fn queue_nam(&self, id: u32, prepared: PreparedNam) {
        let live = *self.nam.lock().unwrap_or_else(PoisonError::into_inner);
        let mut pending = self
            .pending_nam
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(NamCommand::Install(old)) =
            pending.queue(id, NamCommand::Install(prepared), &live)
        {
            drop(old);
        }
    }

    /// Asks the next block to drop block `id`'s live model out of the
    /// processor.  A model still waiting is dropped here, on the UI thread.
    pub(crate) fn clear_nam(&self, id: u32) {
        let live = *self.nam.lock().unwrap_or_else(PoisonError::into_inner);
        let mut pending = self
            .pending_nam
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(NamCommand::Install(old)) = pending.queue(id, NamCommand::Clear, &live) {
            drop(old);
        }
    }

    /// The queued commands, if the UI is not holding them.  The callback
    /// puts a command back when it cannot park the model it would replace.
    pub(crate) fn pending_nam_mut(&self) -> Result<std::sync::MutexGuard<'_, NamPending>, ()> {
        match self.pending_nam.try_lock() {
            Ok(guard) => Ok(guard),
            Err(std::sync::TryLockError::Poisoned(poison)) => Ok(poison.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => Err(()),
        }
    }

    /// Applies new cabinet controls.  Does not read or resample an impulse.
    pub(crate) fn set_cab(&self, settings: CabControls) {
        let settings = settings.sanitized();
        if let Ok(mut current) = self.cab.lock() {
            if *current != settings {
                *current = settings;
                self.cab_version.fetch_add(1, Ordering::Release);
            }
        }
    }

    pub(crate) fn cab_version(&self) -> u64 {
        self.cab_version.load(Ordering::Acquire)
    }

    /// The current controls, or `None` if the UI holds the lock right now.
    pub(crate) fn try_cab(&self) -> Option<CabControls> {
        self.cab.try_lock().ok().map(|settings| *settings)
    }

    pub(crate) fn cab_park(&self) -> &Arc<CabPark> {
        &self.cab_park
    }

    /// Queues a kernel for the next block.  A kernel still waiting is
    /// dropped here, on the UI thread.  The same path at the same rate is
    /// the caller's to skip; this always replaces.
    pub(crate) fn queue_cab(&self, kernel: CabKernel) {
        let mut slot = self
            .pending_cab
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(CabCommand::Install(old)) = slot.replace(CabCommand::Install(kernel)) {
            drop(old);
        }
    }

    /// Asks the next block to take the live kernel out of the processor.
    /// A kernel still waiting is dropped here, on the UI thread.
    pub(crate) fn clear_cab(&self) {
        let mut slot = self
            .pending_cab
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(CabCommand::Install(old)) = slot.replace(CabCommand::Clear) {
            drop(old);
        }
    }

    /// The queued command, if the UI is not holding it.  The callback puts
    /// a command back when it cannot park the kernel it would replace.
    pub(crate) fn pending_cab_mut(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, Option<CabCommand>>, ()> {
        match self.pending_cab.try_lock() {
            Ok(guard) => Ok(guard),
            Err(std::sync::TryLockError::Poisoned(poison)) => Ok(poison.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => Err(()),
        }
    }
}

impl Drop for PathControls {
    fn drop(&mut self) {
        // `Shared`'s last `Arc` can drop on the `rigwerk-bridge` thread:
        // `replace_run` drops the engine's clone while the bridge still
        // holds one, and `run_bridge` drops that one after the callback
        // has stopped.  A model still queued must not be destroyed there.
        // `get_mut` does not lock; this is the last reference to the
        // controls.  The park outlives the run (the engine holds it), and
        // the UI thread is what later drops the model.
        let pending = self
            .pending_nam
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);
        for (_, command) in pending.drain() {
            if let NamCommand::Install(prepared) = command {
                self.nam_park.retire(prepared.model);
            }
        }
        let pending = self
            .pending_cab
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(CabCommand::Install(kernel)) = pending.take() {
            self.cab_park.retire(kernel);
        }
    }
}
