//! Fixed path slots.  Adding one never allocates on the callback, and a
//! slot keeps its index, so removing a path does not move the others.
//!
//! The audio side `try_lock`s the EQ, gate, pitch, NAM and cabinet.  A
//! busy lock keeps the last settings.  Sends are taken after level and
//! match.  A return does not send.  Chain latency always counts, including
//! a bypassed effect; the cabinet adds none.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use std::time::Instant;

use super::cab::{CabCommand, CabControls, CabKernel, CabPark};
use super::eq::{EqSettings, EqSlot};
use super::gate::GateSettings;
use super::nam::{NamCommand, NamPark, NamPending, NamStack, PreparedNam};
use super::output_map::level_gain;
use super::pitch::PitchSettings;
use super::plugins::{ChainHome, ChainModel, PluginView, SlotLink, SlotPark};

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

/// Range of a path's loudness-match trim, dB.  One trim per path: it makes
/// paths switch at the same loudness on every output.
pub const PATH_MATCH_MAX_DB: f32 = 24.0;

/// A NaN match trim becomes 0 dB; the rest is clamped to
/// ±[`PATH_MATCH_MAX_DB`].
pub fn sanitize_path_match(match_db: f32) -> f32 {
    if match_db.is_nan() {
        0.0
    } else {
        match_db.clamp(-PATH_MATCH_MAX_DB, PATH_MATCH_MAX_DB)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PathFeed {
    #[default]
    Input,
    /// The other paths' sends: the path is a return.
    Sends,
}

/// A send at or below this is off.  Sends share the path level's range.
pub const SEND_OFF_DB: f32 = PATH_LEVEL_MIN_DB;

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

/// NAM blocks are not here: sixteen of them do not fit one [`Copy`].
/// Neither are the plugin instances or the cabinet kernel.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PathMix {
    /// The 2 × 4 input routing (input × tap).  Only taps 1/2 (the path's
    /// L/R) are used; 3/4 are kept for the stored layout.
    pub routes: [[bool; 4]; 2],
    pub mono: bool,
    pub output_enabled: [bool; 2],
    /// The EQs before and after the effects, indexed by [`EqSlot`].
    pub eqs: [EqSettings; 2],
    pub gate: GateSettings,
    /// Built-in pitch shifter, first in the chain.  Never runs on a return.
    pub pitch: PitchSettings,
    /// Built-in cabinet.  The impulse path is not here: this stays [`Copy`].
    pub cab: CabControls,
    pub level_db: f32,
    /// Loudness-match trim, dB (±[`PATH_MATCH_MAX_DB`]), applied with the
    /// level on every output.
    pub match_db: f32,
    pub mute: bool,
    pub solo: bool,
    /// Skips every effect's processing without changing each effect's own
    /// bypass.  Latency stays counted.
    pub chain_bypass: bool,
    pub feed: PathFeed,
    /// Only a send to a return, from an input path, plays.
    pub sends: [f32; MAX_PATHS],
}

impl Default for PathMix {
    fn default() -> Self {
        Self {
            routes: [[true, false, false, false], [false, true, false, false]],
            mono: false,
            output_enabled: [true, true],
            eqs: [EqSettings::default(), EqSettings::default()],
            gate: GateSettings::default(),
            pitch: PitchSettings::default(),
            cab: CabControls::default(),
            level_db: 0.0,
            match_db: 0.0,
            mute: false,
            solo: false,
            chain_bypass: false,
            feed: PathFeed::Input,
            sends: [SEND_OFF_DB; MAX_PATHS],
        }
    }
}

impl PathMix {
    /// Every value in range (EQ, gate, pitch, level, match and sends).  A NaN
    /// send is off, not 0 dB.  A return has no solo and no sends of its own.
    pub fn sanitized(self) -> Self {
        let is_return = self.is_return();
        Self {
            eqs: self.eqs.map(EqSettings::sanitized),
            gate: self.gate.sanitized(),
            pitch: self.pitch.sanitized(),
            cab: self.cab.sanitized(),
            level_db: sanitize_path_level(self.level_db),
            match_db: sanitize_path_match(self.match_db),
            solo: self.solo && !is_return,
            sends: if is_return {
                [SEND_OFF_DB; MAX_PATHS]
            } else {
                self.sends.map(sanitize_send)
            },
            ..self
        }
    }

    pub fn is_return(&self) -> bool {
        self.feed == PathFeed::Sends
    }
}

/// A send level in range: NaN is off, the rest is clamped like a path
/// level.
pub fn sanitize_send(level_db: f32) -> f32 {
    if level_db.is_nan() {
        SEND_OFF_DB
    } else {
        sanitize_path_level(level_db)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PathState {
    /// Unused: the mixer skips it.
    Inactive = 0,
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
    #[cfg(test)]
    pub(crate) fn main() -> Self {
        Self {
            slots: [0; MAX_PATHS],
            count: 1,
        }
    }

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
    pub avg_ns: u32,
    /// The longest recent block, decaying over about two seconds, ns.
    pub peak_ns: u32,
    /// The block's length: the budget, ns.
    pub block_ns: u32,
}

impl DspTime {
    pub fn avg_share(&self) -> f32 {
        self.share(self.avg_ns)
    }

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

/// A chain skip is a busy lock; an effect skip is a pause.  The audio
/// thread writes these and never reads them back.
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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PluginLockReport {
    pub chain: LockStats,
    pub effects: Vec<LockStats>,
    /// A panicking edit.  The skip counters then describe nothing.
    pub chain_poisoned: bool,
}

/// No lock.  A reader may see one block's average beside the next peak.
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

    pub(crate) fn store(&self, time: DspTime) {
        self.avg_ns.store(time.avg_ns, Ordering::Relaxed);
        self.peak_ns.store(time.peak_ns, Ordering::Relaxed);
        self.block_ns.store(time.block_ns, Ordering::Relaxed);
    }
}

pub(crate) struct PathControls {
    pub(crate) routes: [AtomicBool; 8],
    pub(crate) mono: AtomicBool,
    pub(crate) output_enabled: [AtomicBool; 2],
    state: AtomicU8,
    /// Bumped when the slot becomes active, so the chain starts fresh.
    generation: AtomicU64,
    /// Path level as a linear gain (`f32` bits), the loudness-match trim
    /// and its linear gain, mute and solo.
    level_gain: AtomicU32,
    level_db: AtomicU32,
    match_gain: AtomicU32,
    match_db: AtomicU32,
    mute: AtomicBool,
    solo: AtomicBool,
    is_return: AtomicBool,
    send_db: [AtomicU32; MAX_PATHS],
    send_gain: [AtomicU32; MAX_PATHS],
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
    /// The trim moves only when the UI applies a result.
    match_measure_generation: AtomicU64,
    match_measuring: AtomicBool,
    match_integrated: AtomicU32,
    match_blocks: AtomicU32,
    /// Written at the end of each block.  Zero until a mixer runs.
    dsp: DspCell,
    /// Declared before `chain_home` so these drop first.  The park
    /// outlives the run and drains on the main thread.
    pub(super) slot_links: Mutex<Arc<[SlotLink]>>,
    /// The audio thread never takes this lock.
    plugin_views: Mutex<Arc<[PluginView]>>,
    plugin_views_version: AtomicU64,
    /// The UI rebuilds the views.  The audio thread never builds them.
    plugin_views_dirty: AtomicBool,
    chain_rate: AtomicU32,
    chain_unopened: AtomicBool,
    /// UI-only.  The audio thread never takes this lock.
    pub(super) chain: Mutex<ChainModel>,
    /// Where the chain's runtime waits while no audio thread holds it.
    chain_home: Arc<ChainHome>,
    /// Shared with the engine, so the park outlives the run.
    slot_park: Arc<SlotPark>,
    /// Stored at each block by the holder, and by an edit from the links.
    latency: AtomicU32,
    /// The mixer aligns with this.  The cabinet adds none.
    path_latency: AtomicU32,
    /// Path-level effect bypass.  The runtime loads it every block; the UI
    /// stores it.  It does not write each slot's bypass.
    chain_bypass: Arc<AtomicBool>,
    plugin_active: AtomicUsize,
    /// Its own flag, so a recount of `plugin_active` cannot clear a wake.
    plugin_wake: AtomicBool,
    /// Measurement only.  Written relaxedly, never read on the callback.
    chain_attempts: AtomicU64,
    chain_skips: AtomicU64,
    /// The chain stays unused for the rest of the run.  It never clears.
    chain_poisoned: AtomicBool,
    /// The two built-in EQs (pre and post effects).  Each version counter
    /// lets the audio callback notice a change without taking the lock.
    eq: [Mutex<EqSettings>; 2],
    eq_version: [AtomicU64; 2],
    gate: Mutex<GateSettings>,
    gate_version: AtomicU64,
    pitch: Mutex<PitchSettings>,
    pitch_version: AtomicU64,
    /// The callback `try_lock`s, and only when the version moved.
    nam: Mutex<NamStack>,
    nam_version: AtomicU64,
    /// The callback only `try_lock`s this.  A replaced model is dropped
    /// on the UI thread.
    pending_nam: Mutex<NamPending>,
    /// Where a model the audio side has let go of waits to be dropped.
    /// Shared with the engine so a bridge-thread drop can park it.
    nam_park: Arc<NamPark>,
    /// Built-in cabinet controls.  The callback copies them with `try_lock`.
    /// The kernel itself is [`pending_cab`], never this lock.
    cab: Mutex<CabControls>,
    cab_version: AtomicU64,
    /// The callback only `try_lock`s this.  A replaced kernel is dropped
    /// on the UI thread.
    pending_cab: Mutex<Option<CabCommand>>,
    /// Where a kernel the audio side has let go of waits to be dropped.
    /// Shared with the engine so a bridge-thread drop can park it.
    cab_park: Arc<CabPark>,
}

impl PathControls {
    /// A path with its own parks.  Tests that never queue a model use this;
    /// a run shares one park each via [`Self::with_park`].
    #[cfg(test)]
    pub(crate) fn new(sample_rate: u32) -> Self {
        Self::with_park(sample_rate, NamPark::new(), CabPark::new(), SlotPark::new())
    }

    pub(crate) fn with_park(
        sample_rate: u32,
        nam_park: Arc<NamPark>,
        cab_park: Arc<CabPark>,
        slot_park: Arc<SlotPark>,
    ) -> Self {
        let chain_bypass = Arc::new(AtomicBool::new(false));
        let chain = ChainModel::new(
            sample_rate,
            Arc::clone(&chain_bypass),
            Arc::clone(&slot_park),
        );
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
            match_gain: AtomicU32::new(1.0f32.to_bits()),
            match_db: AtomicU32::new(0.0f32.to_bits()),
            mute: AtomicBool::new(false),
            solo: AtomicBool::new(false),
            is_return: AtomicBool::new(false),
            send_db: std::array::from_fn(|_| AtomicU32::new(SEND_OFF_DB.to_bits())),
            send_gain: std::array::from_fn(|_| AtomicU32::new(0.0f32.to_bits())),
            removed_at: Mutex::new(None),
            retired: AtomicBool::new(false),
            meter_l: AtomicU32::new(0.0f32.to_bits()),
            meter_r: AtomicU32::new(0.0f32.to_bits()),
            gate_reduction_db: AtomicU32::new(0.0f32.to_bits()),
            match_measure_generation: AtomicU64::new(0),
            match_measuring: AtomicBool::new(false),
            match_integrated: AtomicU32::new(super::SILENCE_LUFS.to_bits()),
            match_blocks: AtomicU32::new(0),
            dsp: DspCell::default(),
            slot_links: Mutex::new(Arc::from(Vec::new())),
            plugin_views: Mutex::new(Arc::from(Vec::new())),
            plugin_views_version: AtomicU64::new(0),
            plugin_views_dirty: AtomicBool::new(false),
            chain_rate: AtomicU32::new(chain.rate()),
            chain_unopened: AtomicBool::new(false),
            chain_home: chain.home(),
            chain: Mutex::new(chain),
            slot_park,
            latency: AtomicU32::new(0),
            path_latency: AtomicU32::new(0),
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
            pitch: Mutex::new(PitchSettings::default()),
            pitch_version: AtomicU64::new(1),
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

    /// Live instances stay with the run that opened them.  Call from the
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
        self.set_match_db(previous.match_db());
        copy_bool(&self.mute, &previous.mute);
        copy_bool(&self.solo, &previous.solo);
        self.set_chain_bypass(previous.chain_bypassed());
        self.set_feed(previous.feed());
        self.set_sends(previous.sends());
        // The alignment figure is measured per block; a new run measures it
        // again.  Nothing of the old run's NAM delay carries over.
        self.path_latency.store(0, Ordering::Relaxed);
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
            let settings = previous
                .pitch
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            self.set_pitch(*settings);
        }
        {
            // The block settings move.  The live models stay with the run
            // that loaded them; the UI queues them again from the saved path.
            let stack = previous.nam.lock().unwrap_or_else(PoisonError::into_inner);
            self.set_nam(&stack);
        }
        {
            // The numeric cabinet controls move.  The live kernel stays
            // with the run that loaded it; the UI installs it again from
            // the saved path.
            let settings = previous.cab.lock().unwrap_or_else(PoisonError::into_inner);
            self.set_cab(*settings);
        }
        // A removed path starts the new run empty.  Capture state on this
        // thread first.
        if previous.state() == PathState::Active {
            self.restore_plugins(previous.plugin_prefs());
        } else {
            self.restore_plugins(Vec::new());
        }
    }

    pub(crate) fn state(&self) -> PathState {
        PathState::from_u8(self.state.load(Ordering::Acquire))
    }

    /// Becoming active bumps the generation first, so a mixer that sees
    /// the slot live never plays the old chain state.
    pub(crate) fn set_state(&self, state: PathState) {
        if state == PathState::Active && self.state() == PathState::Inactive {
            // Generation first, so a live slot is never seen with the old
            // generation.
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

    pub(crate) fn set_mix(&self, mix: &PathMix) {
        self.set_routes(mix.routes);
        self.set_mono(mix.mono);
        self.set_output_enabled(mix.output_enabled);
        for slot in EqSlot::ALL {
            self.set_eq(slot, mix.eqs[slot.index()]);
        }
        self.set_gate(mix.gate);
        self.set_pitch(mix.pitch);
        self.set_cab(mix.cab);
        self.set_level_db(mix.level_db);
        self.set_match_db(mix.match_db);
        self.set_mute(mix.mute);
        self.set_solo(mix.solo);
        self.set_chain_bypass(mix.chain_bypass);
        self.set_feed(mix.feed);
        self.set_sends(mix.sends);
    }

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
            pitch: *self.pitch.lock().unwrap_or_else(PoisonError::into_inner),
            cab: *self.cab.lock().unwrap_or_else(PoisonError::into_inner),
            level_db: self.level_db(),
            match_db: self.match_db(),
            mute: self.muted(),
            solo: self.soloed(),
            chain_bypass: self.chain_bypassed(),
            feed: self.feed(),
            sends: self.sends(),
        }
    }

    pub(crate) fn set_feed(&self, feed: PathFeed) {
        self.is_return
            .store(feed == PathFeed::Sends, Ordering::Relaxed);
    }

    pub(crate) fn feed(&self) -> PathFeed {
        if self.is_return() {
            PathFeed::Sends
        } else {
            PathFeed::Input
        }
    }

    pub(crate) fn is_return(&self) -> bool {
        self.is_return.load(Ordering::Relaxed)
    }

    pub(crate) fn set_sends(&self, sends: [f32; MAX_PATHS]) {
        for (slot, level_db) in sends.into_iter().enumerate() {
            let level_db = sanitize_send(level_db);
            self.send_db[slot].store(level_db.to_bits(), Ordering::Relaxed);
            self.send_gain[slot].store(path_level_gain(level_db).to_bits(), Ordering::Relaxed);
        }
    }

    pub(crate) fn sends(&self) -> [f32; MAX_PATHS] {
        std::array::from_fn(|slot| f32::from_bits(self.send_db[slot].load(Ordering::Relaxed)))
    }

    /// The send to slot `slot` as a linear gain: 0 when off, exactly 1.0
    /// at 0 dB.
    pub(crate) fn send_gain(&self, slot: usize) -> f32 {
        f32::from_bits(self.send_gain[slot].load(Ordering::Relaxed))
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

    pub(crate) fn set_match_db(&self, match_db: f32) {
        let match_db = sanitize_path_match(match_db);
        self.match_db.store(match_db.to_bits(), Ordering::Relaxed);
        self.match_gain
            .store(10.0f32.powf(match_db / 20.0).to_bits(), Ordering::Relaxed);
    }

    pub(crate) fn match_db(&self) -> f32 {
        f32::from_bits(self.match_db.load(Ordering::Relaxed))
    }

    /// The match trim as a linear gain: exactly 1.0 at 0 dB.
    pub(crate) fn match_gain(&self) -> f32 {
        f32::from_bits(self.match_gain.load(Ordering::Relaxed))
    }

    /// Arms this path's Match paths meter: the next block clears it and
    /// starts feeding it, and the generation tells the mixer to start over.
    pub(crate) fn start_match_measurement(&self) {
        self.match_integrated
            .store(super::SILENCE_LUFS.to_bits(), Ordering::Relaxed);
        self.match_blocks.store(0, Ordering::Relaxed);
        self.match_measuring.store(true, Ordering::Release);
        self.match_measure_generation.fetch_add(1, Ordering::AcqRel);
    }

    /// Stops this path's Match paths meter.  The last reading stays until
    /// the next start, so the UI can read it after it stops.
    pub(crate) fn stop_match_measurement(&self) {
        self.match_measuring.store(false, Ordering::Release);
        self.match_measure_generation.fetch_add(1, Ordering::AcqRel);
    }

    /// The generation the mixer compares to know a new measurement began.
    pub(crate) fn match_measure_generation(&self) -> u64 {
        self.match_measure_generation.load(Ordering::Acquire)
    }

    pub(crate) fn match_measuring(&self) -> bool {
        self.match_measuring.load(Ordering::Relaxed)
    }

    pub(crate) fn publish_match_measurement(&self, integrated_lufs: f32, blocks: u32) {
        self.match_integrated
            .store(integrated_lufs.to_bits(), Ordering::Relaxed);
        self.match_blocks.store(blocks, Ordering::Relaxed);
    }

    pub(crate) fn match_measurement(&self) -> (f32, u32) {
        (
            f32::from_bits(self.match_integrated.load(Ordering::Relaxed)),
            self.match_blocks.load(Ordering::Relaxed),
        )
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

    pub(crate) fn set_pitch(&self, settings: PitchSettings) {
        if let Ok(mut current) = self.pitch.lock() {
            if *current != settings {
                *current = settings;
                self.pitch_version.fetch_add(1, Ordering::Release);
            }
        }
    }

    pub(crate) fn pitch_version(&self) -> u64 {
        self.pitch_version.load(Ordering::Acquire)
    }

    pub(crate) fn try_pitch(&self) -> Option<PitchSettings> {
        self.pitch.try_lock().ok().map(|settings| *settings)
    }

    /// Holds the pitch lock, so the audio side finds it busy.
    #[cfg(test)]
    pub(crate) fn hold_pitch(&self) -> std::sync::MutexGuard<'_, PitchSettings> {
        self.pitch.lock().unwrap_or_else(PoisonError::into_inner)
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

    /// A kernel still waiting is dropped here, on the UI thread.
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
        // The last `Arc` can drop on the bridge thread.  A queued model
        // must not be destroyed there.  `get_mut` does not lock.
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
