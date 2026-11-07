//! One signal path's controls, as the UI writes them and the audio
//! callback reads them: input routing, mono, channel enable, the two
//! built-in EQs, the gate, the plugin insert chain and the path meters.
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
//! insert skipped for a block plays its input delayed by its latency, and
//! an insert's dry/wet mix delays its dry side (see `PluginChain::process`).
//! The mixer lines the paths up per device with it (`output_map`,
//! "Alignment").
//!
//! The plugin-chain side of [`PathControls`] (edits, publish, controls,
//! servicing, state) is in `plugins`, with how a chain is handed between
//! the UI and the audio thread ("Chain handover").

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use std::time::Instant;

use super::clap::{ClapPlugin, HostState};
use super::eq::{EqSettings, EqSlot};
use super::gate::GateSettings;
use super::output_map::level_gain;
use super::plugins::{PluginChain, PluginView, SlotLink, Vst3Service};

mod plugins;

use plugins::ParamWrite;

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
/// channel enable, the two EQs, the gate, and the path level, mute and
/// solo.  The plugin chain is kept in the engine ([`PathControls`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PathMix {
    /// The 2 × 4 input routing (input × tap).  Only taps 1/2 (the path's
    /// L/R) are used; 3/4 are kept for the stored layout.
    pub routes: [[bool; 4]; 2],
    pub mono: bool,
    /// Channel enable for the path's L and R.
    pub output_enabled: [bool; 2],
    /// The EQs before and after the inserts, indexed by [`EqSlot`].
    pub eqs: [EqSettings; 2],
    pub gate: GateSettings,
    /// Path level, dB (see `PATH_LEVEL_MIN_DB` / `PATH_LEVEL_MAX_DB`).
    pub level_db: f32,
    pub mute: bool,
    pub solo: bool,
}

impl Default for PathMix {
    fn default() -> Self {
        Self {
            routes: [[true, false, false, false], [false, true, false, false]],
            mono: false,
            output_enabled: [true, true],
            eqs: [EqSettings::default(), EqSettings::default()],
            gate: GateSettings::default(),
            level_db: 0.0,
            mute: false,
            solo: false,
        }
    }
}

impl PathMix {
    /// Every value in range (EQ, gate and level).
    pub fn sanitized(self) -> Self {
        Self {
            eqs: self.eqs.map(EqSettings::sanitized),
            gate: self.gate.sanitized(),
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
    /// Just slot 0: the one path every session has had so far.
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

/// A CLAP plugin and its host state, as `service_plugins` services them.
type ClapService = (Arc<HostState>, Arc<Mutex<ClapPlugin>>);

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
    /// How long the mixer spends on this path's chain per block, ns
    /// (smoothed, and a decaying peak), and how long that block lasts, ns:
    /// its DSP budget.  Written by the mixer at the end of each block (see
    /// `mixer::PathProcessor::publish_dsp`), read by Diagnostics and Live.
    dsp: DspCell,
    /// The CLAP plugins the UI services every frame, published with the
    /// views so `service_plugins` does not take the chain lock to find them.
    ///
    /// Declared before `plugins` on purpose: fields drop in order, so these
    /// extra references are gone before the chain hands its slots to the
    /// deferred queue, and the main thread's drain is always the last owner.
    pub(super) plugin_services: Mutex<Arc<[ClapService]>>,
    /// The VST3 plugins `service_plugins` services (restart requests and
    /// latency), published the same way and declared here for the same
    /// reason.
    vst3_services: Mutex<Arc<[Arc<Vst3Service>]>>,
    /// Each insert's mix, bypass and plugin, published with the views, so
    /// the UI sets mix, bypass and plugin controls without the chain lock
    /// (see `plugins`, "Chain handover").  Declared before `plugins`
    /// for the same reason as the service lists.
    slot_links: Mutex<Arc<[SlotLink]>>,
    /// Control values set through `slot_links`, stored into the chain the
    /// next time the UI holds its lock (an edit, a state capture, a save).
    param_backlog: Mutex<Vec<ParamWrite>>,
    /// The insert views the UI draws.  Republished by every chain edit while
    /// that edit still holds the chain lock, and read without it.  This lock
    /// is separate from the chain's and the audio thread never takes it, so
    /// only UI-side readers and writers contend on it, for an `Arc` clone.
    plugin_views: Mutex<Arc<[PluginView]>>,
    plugin_views_version: AtomicU64,
    /// Set by the audio thread when an insert fails; the UI thread rebuilds
    /// the views in `service_plugins`, so the audio thread never builds them.
    plugin_views_dirty: AtomicBool,
    /// What the published views were built from, so the per-frame checks in
    /// `service_plugins` can skip the chain lock when nothing changed.
    published_ports_changed: AtomicUsize,
    chain_rate: AtomicU32,
    chain_unopened: AtomicBool,
    /// The plugin insert chain and a cheap "is anything enabled?" flag so
    /// the audio callback skips the lock when no effects are loaded.
    pub(super) plugins: Mutex<PluginChain>,
    /// The chain's total latency ([`PluginChain::latency_handle`]), read
    /// without the chain lock.
    latency: Arc<AtomicU32>,
    plugin_active: AtomicUsize,
    /// The two built-in EQs (pre and post inserts).  Each version counter
    /// lets the audio callback notice a change without taking the lock.
    eq: [Mutex<EqSettings>; 2],
    eq_version: [AtomicU64; 2],
    /// Built-in noise gate (PRE or POST inserts).
    gate: Mutex<GateSettings>,
    gate_version: AtomicU64,
}

impl PathControls {
    pub(crate) fn new(sample_rate: u32) -> Self {
        let chain = PluginChain::new(sample_rate);
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
            dsp: DspCell::default(),
            plugin_services: Mutex::new(Arc::from(Vec::new())),
            vst3_services: Mutex::new(Arc::from(Vec::new())),
            slot_links: Mutex::new(Arc::from(Vec::new())),
            param_backlog: Mutex::new(Vec::new()),
            plugin_views: Mutex::new(Arc::from(Vec::new())),
            plugin_views_version: AtomicU64::new(0),
            plugin_views_dirty: AtomicBool::new(false),
            published_ports_changed: AtomicUsize::new(0),
            chain_rate: AtomicU32::new(chain.rate()),
            chain_unopened: AtomicBool::new(false),
            latency: chain.latency_handle(),
            plugins: Mutex::new(chain),
            plugin_active: AtomicUsize::new(0),
            eq: [
                Mutex::new(EqSettings::default()),
                Mutex::new(EqSettings::default()),
            ],
            eq_version: [AtomicU64::new(1), AtomicU64::new(1)],
            gate: Mutex::new(GateSettings::default()),
            gate_version: AtomicU64::new(1),
        }
    }

    /// Copies a previous session's settings for this path.  Live plugin
    /// instances belong to the session that opened them; only the saved
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
        // A fade-out does not outlive its session.
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
        // Only an active path carries its chain over; a removed one starts
        // the new session empty.  Capture the old instances' state on this
        // (UI) thread first, so the rebuilt chain reopens with what the
        // person set since the last save.
        if previous.state() == PathState::Active {
            previous.capture_plugin_states();
            let plugins = previous
                .plugins
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .prefs();
            self.restore_plugins(plugins);
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

    /// Whether some insert was restored but not opened yet (lock-free).
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
        self.set_level_db(mix.level_db);
        self.set_mute(mix.mute);
        self.set_solo(mix.solo);
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
            level_db: self.level_db(),
            mute: self.muted(),
            solo: self.soloed(),
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

    pub(crate) fn reset_meters(&self) {
        self.store_meters(0.0, 0.0);
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
}
