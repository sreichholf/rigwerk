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
//! # Chain handover
//!
//! The audio thread only try-locks a chain; a missed lock costs that chunk
//! its inserts.  So the edits made all the time never take the chain lock:
//! - mix and bypass are atomics per insert (`SlotControls`) that the audio
//!   thread reads every chunk;
//! - a plugin control goes straight to the plugin through the published
//!   insert links (`slot_links`), holding only that plugin's own lock (the
//!   audio thread then skips just that insert for a chunk, delayed by its
//!   latency, never the chain); the view is patched at once and the saved
//!   value waits in `param_backlog` until the UI takes the chain lock for
//!   something else (an edit, a state capture, a save);
//! - drawing reads the published views, and a quiet service frame takes no
//!   lock.
//!
//! What still takes the chain lock is structural and short: adding,
//! removing, moving or restoring inserts, reopening them for a new rate
//! (slow loads and teardowns run outside the lock), resetting a plugin's
//! controls, syncing from its editor, capturing state for a save, and the
//! publish of the views those edits do.
//!
//! For those rare misses the mixer keeps the timing: rather than a second,
//! lock-free copy of the chain (plugin instances are stateful and cannot be
//! duplicated) it plays the chunk's chain input delayed by
//! [`PathControls::latency_samples`] (`mixer::PathProcessor::run_inserts`).
//! The path stays in time in the sum, so nothing combs and the phase does
//! not jump, but its level and tone are not kept: for that chunk the path
//! plays dry.  The step into the fallback is smoothed (no crossfade is
//! possible), and the chain's own output fades back in once it runs again,
//! after one latency's worth of frames (the inserts missed that input).
//! [`PathControls::process_plugins`] reports whether the chain ran.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use std::time::Instant;

use super::clap::{ClapPlugin, HostState};
use super::eq::{EqSettings, EqSlot};
use super::gate::GateSettings;
use super::output_map::{path_level_gain, sanitize_path_level};
use super::plugins::{
    host_format_parameter, host_set_parameter, PluginChain, PluginHandle, PluginPref, PluginView,
    SlotLink, Vst3Service,
};

/// Most signal paths a session can hold.  Every per-path array (controls,
/// processors, device maps) is this long; only the active ones run.
pub const MAX_PATHS: usize = 4;

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

/// The service lists a publish replaces; dropped after the chain lock is
/// released, since they may hold the last reference to a removed plugin.
type StaleServices = (Arc<[ClapService]>, Arc<[Arc<Vst3Service>]>, Arc<[SlotLink]>);

/// A control value the UI set on a plugin without the chain lock, waiting
/// to be stored in the chain's insert (its view and saved value) the next
/// time the UI takes the lock anyway.
struct ParamWrite {
    index: usize,
    handle: PluginHandle,
    id: u32,
    value: f64,
    display: Option<String>,
}

/// Armed by `edit_chain` around the edit itself.  If the edit panics, the
/// chain mutex is poisoned and no later edit publishes again, so the
/// published views and CLAP services are cleared here instead of being left
/// to describe (and hold references to) slots the edit may have changed.
struct PublishOnUnwind<'a> {
    path: &'a PathControls,
}

impl Drop for PublishOnUnwind<'_> {
    fn drop(&mut self) {
        self.path.publish_poisoned();
    }
}

/// One path's DSP time per block, as the mixer publishes it.
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
    dsp_ns: AtomicU32,
    dsp_peak_ns: AtomicU32,
    block_ns: AtomicU32,
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
    /// (see the module docs, "Chain handover").  Declared before `plugins`
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
            dsp_ns: AtomicU32::new(0),
            dsp_peak_ns: AtomicU32::new(0),
            block_ns: AtomicU32::new(0),
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
        DspTime {
            avg_ns: self.dsp_ns.load(Ordering::Relaxed),
            peak_ns: self.dsp_peak_ns.load(Ordering::Relaxed),
            block_ns: self.block_ns.load(Ordering::Relaxed),
        }
    }

    /// Called by the output callback at the end of each block.
    pub(crate) fn store_dsp_time(&self, time: DspTime) {
        self.dsp_ns.store(time.avg_ns, Ordering::Relaxed);
        self.dsp_peak_ns.store(time.peak_ns, Ordering::Relaxed);
        self.block_ns.store(time.block_ns, Ordering::Relaxed);
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

    /// Runs one edit under the chain lock and republishes what the UI and
    /// the audio flag read from it before releasing the lock.  `None` when
    /// the chain is poisoned.  The replaced service lists are dropped
    /// only after the lock is released: it may hold the last reference to a
    /// plugin the edit removed.
    pub(super) fn edit_chain<R>(&self, edit: impl FnOnce(&mut PluginChain) -> R) -> Option<R> {
        let (result, stale, stored) = {
            let mut chain = self.plugins.lock().ok()?;
            let stored = self.store_backlog(&mut chain);
            // Declared after the chain guard, so it runs first while
            // unwinding, still under the lock.
            let unwind = PublishOnUnwind { path: self };
            let result = edit(&mut chain);
            std::mem::forget(unwind);
            let stale = self.publish_plugins(&chain);
            (result, stale, stored)
        };
        drop((stale, stored));
        Some(result)
    }

    /// Takes the chain lock for UI-side work that does not change the
    /// inserts, with the control values set meanwhile stored first.  The
    /// stored writes (they hold plugin references) are dropped after the
    /// lock is released.
    fn with_chain<R>(&self, work: impl FnOnce(&mut PluginChain) -> R) -> Option<R> {
        let (result, stored) = {
            let mut chain = self.plugins.lock().ok()?;
            let stored = self.store_backlog(&mut chain);
            (work(&mut chain), stored)
        };
        drop(stored);
        Some(result)
    }

    /// Stores the control values set without the chain lock (see
    /// [`Self::set_plugin_parameter`]).  The caller holds the chain lock
    /// and drops the returned writes after releasing it.
    #[must_use]
    fn store_backlog(&self, chain: &mut PluginChain) -> Vec<ParamWrite> {
        let writes = std::mem::take(
            &mut *self
                .param_backlog
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        for write in &writes {
            chain.store_parameter(
                write.index,
                &write.handle,
                write.id,
                write.value,
                write.display.clone(),
            );
        }
        writes
    }

    /// The insert at `index` as last published.
    fn slot_link(&self, index: usize) -> Option<SlotLink> {
        self.slot_links
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(index)
            .cloned()
    }

    /// Changes one insert in the published views, without the chain lock.
    fn patch_view(&self, index: usize, patch: impl FnOnce(&mut PluginView)) {
        let mut views = self
            .plugin_views
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut list = views.to_vec();
        if let Some(view) = list.get_mut(index) {
            patch(view);
            *views = list.into();
            self.plugin_views_version.fetch_add(1, Ordering::Release);
        }
    }

    /// The latency in samples this path's insert chain should add (see
    /// [`PluginChain::latency_samples`]).  Lock-free, for the audio thread.
    pub(crate) fn latency_samples(&self) -> u32 {
        self.latency.load(Ordering::Acquire)
    }

    /// Stores a chain latency without a plugin (screenshots only; the next
    /// chain edit or latency change replaces it).
    pub(crate) fn screenshot_latency(&self, samples: u32) {
        self.latency.store(samples, Ordering::Release);
    }

    /// What a poisoned chain looks like to every reader: no inserts, nothing
    /// to service, nothing to reopen.  This is what `plugin_views` returned
    /// for a poisoned chain before the views were published.
    fn publish_poisoned(&self) {
        self.plugin_active.store(0, Ordering::Relaxed);
        self.chain_unopened.store(false, Ordering::Relaxed);
        self.published_ports_changed.store(0, Ordering::Relaxed);
        *self
            .plugin_views
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Arc::from(Vec::new());
        self.plugin_views_version.fetch_add(1, Ordering::Release);
        // The slots still own these plugins, so this is never the last
        // reference; what matters is that none outlives the chain.
        let services = std::mem::replace(
            &mut *self
                .plugin_services
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
            Arc::from(Vec::new()),
        );
        let vst3 = std::mem::replace(
            &mut *self
                .vst3_services
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
            Arc::from(Vec::new()),
        );
        let links = std::mem::replace(
            &mut *self
                .slot_links
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
            Arc::from(Vec::new()),
        );
        let backlog = std::mem::take(
            &mut *self
                .param_backlog
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        drop((services, vst3, links, backlog));
    }

    /// Publishes the views, the CLAP and VST3 service lists and the cheap
    /// flags for `chain`.  The caller holds the chain lock.  Hands back the
    /// previous service lists, which the caller drops after unlocking.
    #[must_use]
    fn publish_plugins(&self, chain: &PluginChain) -> StaleServices {
        self.plugin_active.store(chain.active(), Ordering::Relaxed);
        self.chain_rate.store(chain.rate(), Ordering::Relaxed);
        self.chain_unopened
            .store(chain.has_unopened(), Ordering::Relaxed);
        self.published_ports_changed
            .store(chain.ports_changed_count(), Ordering::Relaxed);
        let views: Arc<[PluginView]> = chain.views().into();
        let services: Arc<[ClapService]> = chain.clap_services().into();
        let vst3: Arc<[Arc<Vst3Service>]> = chain.vst3_services().into();
        let links: Arc<[SlotLink]> = chain.slot_links().into();
        *self
            .plugin_views
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = views;
        self.plugin_views_version.fetch_add(1, Ordering::Release);
        let stale_clap = std::mem::replace(
            &mut *self
                .plugin_services
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
            services,
        );
        let stale_vst3 = std::mem::replace(
            &mut *self
                .vst3_services
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
            vst3,
        );
        let stale_links = std::mem::replace(
            &mut *self
                .slot_links
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
            links,
        );
        (stale_clap, stale_vst3, stale_links)
    }

    /// Replaces the saved insert list without loading anything.  Plugins are
    /// opened when the bridge starts (or when one is added in the UI).
    pub(crate) fn restore_plugins(&self, prefs: Vec<PluginPref>) {
        let old = self
            .edit_chain(|chain| chain.restore(prefs))
            .unwrap_or_default();
        // Destroyed after the chain lock is released.
        drop(old);
    }

    /// Whether the chain needs a reload for `rate`: it was built at another
    /// rate, or a restored slot was never opened.  Answered from the
    /// published flags, without the chain lock.
    /// The rate the chain was last built for (lock-free).
    pub(crate) fn chain_rate(&self) -> u32 {
        self.chain_rate.load(Ordering::Relaxed)
    }

    pub(crate) fn needs_reload(&self, rate: u32) -> bool {
        rate != self.chain_rate.load(Ordering::Relaxed)
            || self.chain_unopened.load(Ordering::Relaxed)
    }

    /// Loads outside the chain lock and drops the replaced slots only after
    /// releasing it.  `0` means 48 kHz, matching the chain's own fallback.
    pub(crate) fn reload_plugins_for_rate(&self, sample_rate: u32) {
        let rate = if sample_rate == 0 {
            48_000
        } else {
            sample_rate
        };
        const MAX_ATTEMPTS: usize = 3;
        for _ in 0..MAX_ATTEMPTS {
            let plan = match self.edit_chain(|chain| chain.reload_plan(rate)) {
                Some(Some(plan)) => plan,
                Some(None) | None => return,
            };
            let built: Vec<_> = plan
                .jobs
                .into_iter()
                .map(|(index, pref)| {
                    let (slot, _) = PluginChain::load_slot(pref.clone(), rate);
                    (index, pref, slot)
                })
                .collect();
            let installed = match self
                .edit_chain(|chain| chain.install_reload(plan.previous_rate, rate, built))
            {
                Some(Ok(old)) => Some(old),
                // Built for a chain that changed meanwhile; dropped here,
                // after the chain lock was released.
                Some(Err(stale)) => {
                    drop(stale);
                    None
                }
                None => return,
            };
            if let Some(old) = installed {
                drop(old);
                return;
            }
        }
    }

    // `push_slot` hands a mismatched slot back so it can be dropped unlocked.
    #[allow(clippy::result_large_err)]
    pub(crate) fn add_plugin(&self, pref: PluginPref) -> Result<(), String> {
        const MAX_ATTEMPTS: usize = 3;
        let mut attempts = 0;
        loop {
            let rate = self
                .plugins
                .lock()
                .map(|chain| chain.rate())
                .map_err(|_| "Plugin chain unavailable".to_owned())?;
            // Build the slot before taking the chain lock again: loading a
            // dynamic library and calling `start_processing` can take
            // milliseconds, and the audio callback only try-locks the chain.
            let (slot, result) = PluginChain::load_slot(pref.clone(), rate);
            match self
                .edit_chain(|chain| chain.push_slot(slot, rate))
                .ok_or_else(|| "Plugin chain unavailable".to_owned())?
            {
                Ok(()) => return result,
                Err(stale) => {
                    // The chain rate changed while the slot was loading, so
                    // it is set up for the wrong rate.  Drop it outside the
                    // chain lock and build a fresh one.
                    drop(stale);
                }
            }
            attempts += 1;
            if attempts >= MAX_ATTEMPTS {
                return Err(
                    "The plugin chain kept changing sample rate while the plugin was loading."
                        .to_owned(),
                );
            }
        }
    }

    pub(crate) fn remove_plugin(&self, index: usize) {
        let removed = self.edit_chain(|chain| chain.remove(index)).flatten();
        // Destroyed after the chain lock is released.
        drop(removed);
    }

    pub(crate) fn move_plugin(&self, index: usize, delta: isize) {
        self.edit_chain(|chain| chain.move_slot(index, delta));
    }

    /// Bypasses one insert, without the chain lock: the audio thread reads
    /// the flag with its next chunk.
    pub(crate) fn set_plugin_bypass(&self, index: usize, bypass: bool) {
        let Some(link) = self.slot_link(index) else {
            return;
        };
        link.controls.set_bypass(bypass);
        if !bypass {
            // The cheap "anything to run?" flag the audio thread checks
            // before locking; it recounts the active inserts itself.
            self.plugin_active.fetch_max(1, Ordering::Relaxed);
        }
        self.patch_view(index, |view| view.bypass = bypass);
    }

    /// Sets one insert's dry/wet mix, without the chain lock.
    pub(crate) fn set_plugin_mix(&self, index: usize, mix: f32) {
        let Some(link) = self.slot_link(index) else {
            return;
        };
        link.controls.set_mix(mix);
        let mix = link.controls.mix();
        self.patch_view(index, |view| view.mix = mix);
    }

    pub(crate) fn ensure_plugin_loaded(&self, index: usize) {
        self.edit_chain(|chain| chain.ensure_loaded(index));
    }

    /// The loaded plugin behind one insert, for its own editor window.
    pub(crate) fn plugin_handle(&self, index: usize) -> Option<PluginHandle> {
        self.plugins
            .lock()
            .ok()
            .and_then(|chain| chain.handle(index))
    }

    /// Copies control values back after the plugin's own editor changed them.
    /// The plugin is read without the chain lock; only storing the result
    /// takes it, briefly.
    pub(crate) fn sync_plugin_parameters(&self, index: usize) -> Result<(), String> {
        let handle = self
            .plugins
            .lock()
            .map_err(|_| "Plugin chain unavailable".to_owned())?
            .loaded_handle(index)?;
        let Some(parameters) = super::plugins::read_plugin_parameters(&handle) else {
            return Ok(());
        };
        self.edit_chain(|chain| chain.apply_synced_parameters(index, &handle, parameters))
            .ok_or_else(|| "Plugin chain unavailable".to_owned())
    }

    /// Sets one plugin control.  A loaded plugin is reached through the
    /// published links, without the chain lock (only the plugin's own lock,
    /// which the audio thread just skips the insert for): the view is
    /// patched at once and the saved value is stored the next time the UI
    /// takes the chain lock anyway.  An insert that is not open yet goes
    /// through the chain, which opens it.
    pub(crate) fn set_plugin_parameter(
        &self,
        index: usize,
        id: u32,
        value: f64,
    ) -> Result<(), String> {
        let Some(handle) = self.slot_link(index).and_then(|link| {
            if let Some(vst3) = &link.vst3 {
                vst3.nudge();
            }
            link.plugin
        }) else {
            return self
                .edit_chain(|chain| chain.set_parameter(index, id, value))
                .unwrap_or_else(|| Err("Plugin chain unavailable".to_owned()));
        };
        let value = value.clamp(0.0, 1.0);
        host_set_parameter(&handle, id, value)?;
        let display = host_format_parameter(&handle, id, value);
        self.patch_view(index, |view| {
            if let Some(parameter) = view
                .parameters
                .iter_mut()
                .find(|parameter| parameter.id == id)
            {
                parameter.value = value;
                if let Some(display) = &display {
                    parameter.display = display.clone();
                }
            }
        });
        let mut backlog = self
            .param_backlog
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // A drag sends many values for one control; only the last counts.
        backlog.retain(|write| !(write.index == index && write.id == id));
        backlog.push(ParamWrite {
            index,
            handle,
            id,
            value,
            display,
        });
        Ok(())
    }

    pub(crate) fn reset_plugin_parameters(&self, index: usize) -> Result<(), String> {
        self.edit_chain(|chain| chain.reset_parameters(index))
            .unwrap_or_else(|| Err("Plugin chain unavailable".to_owned()))
    }

    /// The insert views as last published.  Never takes the chain lock, so
    /// drawing the UI cannot make the audio callback miss a chain block.
    pub(crate) fn plugin_views(&self) -> Arc<[PluginView]> {
        Arc::clone(
            &self
                .plugin_views
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }

    /// Bumped on every publish of [`Self::plugin_views`].
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn plugin_views_version(&self) -> u64 {
        self.plugin_views_version.load(Ordering::Acquire)
    }

    /// Runs this path's CLAP main-thread callbacks and timers, and the VST3
    /// restart requests, once per UI frame, outside the chain lock: a
    /// plugin's timer can do arbitrary work.  The plugins come from the
    /// published lists, so a quiet frame takes no chain lock at all.
    pub(crate) fn service_plugins(&self) {
        let services = Arc::clone(
            &self
                .plugin_services
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        let vst3 = Arc::clone(
            &self
                .vst3_services
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        let mut latency_changed = false;
        for (host, plugin) in services.iter() {
            super::plugins::service_clap(host, plugin);
            // Taken from the host rather than the call's result, so a change
            // an open editor's own service picked up counts here too.
            latency_changed |= host.take_latency_change();
        }
        for service in vst3.iter() {
            latency_changed |= service.service();
        }
        // An insert failed on the audio thread, or a CLAP plugin changed its
        // ports since the last publish: rebuild the views here, on the UI
        // thread, under a normal chain lock.
        let ports_changed = services
            .iter()
            .filter(|(host, _)| host.ports_changed())
            .count();
        let dirty = self.plugin_views_dirty.swap(false, Ordering::AcqRel);
        if dirty
            || latency_changed
            || ports_changed != self.published_ports_changed.load(Ordering::Relaxed)
        {
            // Also refreshes the chain's latency total.
            self.edit_chain(PluginChain::refresh_latency);
        }
        // Dropped outside the chain lock, on this thread, in case a removal
        // left it as the last reference to a plugin.
        drop((services, vst3));
    }

    /// Captures the opaque state of every loaded plugin whose saved blob is
    /// missing or stale.  Runs on the UI thread (the CLAP main thread); the
    /// plugins are saved without the chain lock, which is only taken briefly
    /// to list them and to store the results.
    pub(crate) fn capture_plugin_states(&self) {
        let Some(requests) = self.with_chain(|chain| chain.state_requests()) else {
            return;
        };
        if requests.is_empty() {
            return;
        }
        let captured: Vec<_> = requests.iter().map(super::plugins::capture_state).collect();
        self.with_chain(|chain| {
            for (request, state) in requests.iter().zip(captured) {
                chain.store_state(request.index, &request.handle, state);
            }
        });
    }

    /// Marks one insert's saved state as out of date (its editor closed).
    pub(crate) fn mark_plugin_state_stale(&self, index: usize) {
        self.with_chain(|chain| chain.mark_state_stale(index));
    }

    /// The insert chain as it should be saved, with fresh plugin state.
    /// Call it from the UI thread.
    pub(crate) fn plugin_prefs(&self) -> Vec<PluginPref> {
        self.capture_plugin_states();
        self.with_chain(|chain| chain.prefs()).unwrap_or_default()
    }

    pub(crate) fn has_plugins(&self) -> bool {
        self.plugin_active.load(Ordering::Relaxed) > 0
    }

    /// Called from the output callback.  Returns whether the chain ran;
    /// `false` leaves the block untouched and the caller plays the chain's
    /// input delayed by [`Self::latency_samples`] instead (see
    /// `mixer::PathProcessor::run_inserts` and the module docs).
    ///
    /// The flags keep the common no-plugin case lock-free.  A chain with no
    /// active insert but some latency (every insert bypassed) still runs, so
    /// its skipped inserts delay the block by their latency.  `try_lock`
    /// keeps a UI edit from stalling the block.
    pub(crate) fn process_plugins(
        &self,
        left: &mut [f32],
        right: &mut [f32],
        frames: usize,
    ) -> bool {
        if self.plugin_active.load(Ordering::Relaxed) == 0 && self.latency_samples() == 0 {
            return true;
        }
        let Ok(mut chain) = self.plugins.try_lock() else {
            return false;
        };
        let failed = chain.process(left, right, frames);
        self.plugin_active.store(chain.active(), Ordering::Relaxed);
        if failed {
            // The views are rebuilt on the UI thread, not here.
            self.plugin_views_dirty.store(true, Ordering::Release);
        }
        true
    }
}
