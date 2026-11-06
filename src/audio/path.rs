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
//! `mixer::PathProcessor` for the realtime half.  Today only slot 0 is
//! active.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use super::clap::{ClapPlugin, HostState};
use super::eq::{EqSettings, EqSlot};
use super::gate::GateSettings;
use super::plugins::{PluginChain, PluginHandle, PluginPref, PluginView};

/// Most signal paths a session can hold.  Every per-path array (controls,
/// processors, device maps) is this long; only the active ones run.
pub const MAX_PATHS: usize = 4;

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
}

/// A CLAP plugin and its host state, as `service_plugins` services them.
type ClapService = (Arc<HostState>, Arc<Mutex<ClapPlugin>>);

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
    /// Block peaks of the path's output (`f32` bits), before the master.
    meter_l: AtomicU32,
    meter_r: AtomicU32,
    /// The CLAP plugins the UI services every frame, published with the
    /// views so `service_plugins` does not take the chain lock to find them.
    ///
    /// Declared before `plugins` on purpose: fields drop in order, so these
    /// extra references are gone before the chain hands its slots to the
    /// deferred queue, and the main thread's drain is always the last owner.
    pub(super) plugin_services: Mutex<Arc<[ClapService]>>,
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
            meter_l: AtomicU32::new(0.0f32.to_bits()),
            meter_r: AtomicU32::new(0.0f32.to_bits()),
            plugin_services: Mutex::new(Arc::from(Vec::new())),
            plugin_views: Mutex::new(Arc::from(Vec::new())),
            plugin_views_version: AtomicU64::new(0),
            plugin_views_dirty: AtomicBool::new(false),
            published_ports_changed: AtomicUsize::new(0),
            chain_rate: AtomicU32::new(chain.rate()),
            chain_unopened: AtomicBool::new(false),
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
        // Capture the old instances' state on this (UI) thread first, so the
        // rebuilt chain reopens with what the person set since the last save.
        previous.capture_plugin_states();
        let plugins = previous
            .plugins
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .prefs();
        self.restore_plugins(plugins);
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
    #[cfg_attr(not(test), allow(dead_code))] // Shown once the UI has path entries.
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
    /// the chain is poisoned.  The replaced CLAP service list is dropped
    /// only after the lock is released: it may hold the last reference to a
    /// plugin the edit removed.
    pub(super) fn edit_chain<R>(&self, edit: impl FnOnce(&mut PluginChain) -> R) -> Option<R> {
        let (result, stale) = {
            let mut chain = self.plugins.lock().ok()?;
            // Declared after the chain guard, so it runs first while
            // unwinding, still under the lock.
            let unwind = PublishOnUnwind { path: self };
            let result = edit(&mut chain);
            std::mem::forget(unwind);
            let stale = self.publish_plugins(&chain);
            (result, stale)
        };
        drop(stale);
        Some(result)
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
        drop(services);
    }

    /// Publishes the views, the CLAP service list and the cheap flags for
    /// `chain`.  The caller holds the chain lock.  Hands back the previous
    /// service list, which the caller drops after unlocking.
    #[must_use]
    fn publish_plugins(&self, chain: &PluginChain) -> Arc<[ClapService]> {
        self.plugin_active.store(chain.active(), Ordering::Relaxed);
        self.chain_rate.store(chain.rate(), Ordering::Relaxed);
        self.chain_unopened
            .store(chain.has_unopened(), Ordering::Relaxed);
        self.published_ports_changed
            .store(chain.ports_changed_count(), Ordering::Relaxed);
        let views: Arc<[PluginView]> = chain.views().into();
        let services: Arc<[ClapService]> = chain.clap_services().into();
        *self
            .plugin_views
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = views;
        self.plugin_views_version.fetch_add(1, Ordering::Release);
        std::mem::replace(
            &mut *self
                .plugin_services
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
            services,
        )
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

    pub(crate) fn set_plugin_bypass(&self, index: usize, bypass: bool) {
        self.edit_chain(|chain| chain.set_bypass(index, bypass));
    }

    pub(crate) fn set_plugin_mix(&self, index: usize, mix: f32) {
        self.edit_chain(|chain| chain.set_mix(index, mix));
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

    pub(crate) fn set_plugin_parameter(
        &self,
        index: usize,
        id: u32,
        value: f64,
    ) -> Result<(), String> {
        self.edit_chain(|chain| chain.set_parameter(index, id, value))
            .unwrap_or_else(|| Err("Plugin chain unavailable".to_owned()))
    }

    pub(crate) fn reset_plugin_parameters(&self, index: usize) -> Result<(), String> {
        self.edit_chain(|chain| chain.reset_parameters(index))
            .unwrap_or_else(|| Err("Plugin chain unavailable".to_owned()))
    }

    /// The insert views as last published.  Never takes the chain lock, so
    /// drawing the UI cannot make the audio callback pass a block dry.
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

    /// Runs this path's CLAP main-thread callbacks and timers once per UI
    /// frame, outside the chain lock: a plugin's timer can do arbitrary
    /// work.  The plugins come from the published list, so a quiet frame
    /// takes no chain lock at all.
    pub(crate) fn service_plugins(&self) {
        let services = Arc::clone(
            &self
                .plugin_services
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        for (host, plugin) in services.iter() {
            super::plugins::service_clap(host, plugin);
        }
        // An insert failed on the audio thread, or a CLAP plugin changed its
        // ports since the last publish: rebuild the views here, on the UI
        // thread, under a normal chain lock.
        let ports_changed = services
            .iter()
            .filter(|(host, _)| host.ports_changed())
            .count();
        let dirty = self.plugin_views_dirty.swap(false, Ordering::AcqRel);
        if dirty || ports_changed != self.published_ports_changed.load(Ordering::Relaxed) {
            self.edit_chain(|_| ());
        }
        // Dropped outside the chain lock, on this thread, in case a removal
        // left it as the last reference to a plugin.
        drop(services);
    }

    /// Captures the opaque state of every loaded plugin whose saved blob is
    /// missing or stale.  Runs on the UI thread (the CLAP main thread); the
    /// plugins are saved without the chain lock, which is only taken briefly
    /// to list them and to store the results.
    pub(crate) fn capture_plugin_states(&self) {
        let requests = match self.plugins.lock() {
            Ok(chain) => chain.state_requests(),
            Err(_) => return,
        };
        if requests.is_empty() {
            return;
        }
        let captured: Vec<_> = requests.iter().map(super::plugins::capture_state).collect();
        if let Ok(mut chain) = self.plugins.lock() {
            for (request, state) in requests.iter().zip(captured) {
                chain.store_state(request.index, &request.handle, state);
            }
        }
    }

    /// Marks one insert's saved state as out of date (its editor closed).
    pub(crate) fn mark_plugin_state_stale(&self, index: usize) {
        if let Ok(mut chain) = self.plugins.lock() {
            chain.mark_state_stale(index);
        }
    }

    /// The insert chain as it should be saved, with fresh plugin state.
    /// Call it from the UI thread.
    pub(crate) fn plugin_prefs(&self) -> Vec<PluginPref> {
        self.capture_plugin_states();
        self.plugins
            .lock()
            .map(|chain| chain.prefs())
            .unwrap_or_default()
    }

    pub(crate) fn has_plugins(&self) -> bool {
        self.plugin_active.load(Ordering::Relaxed) > 0
    }

    /// Called from the output callback.  The flag keeps the common no-plugin
    /// case lock-free, and `try_lock` keeps a UI edit from stalling the block:
    /// when the chain is busy this block passes through dry.
    pub(crate) fn process_plugins(&self, left: &mut [f32], right: &mut [f32], frames: usize) {
        if self.plugin_active.load(Ordering::Relaxed) == 0 {
            return;
        }
        if let Ok(mut chain) = self.plugins.try_lock() {
            let failed = chain.process(left, right, frames);
            self.plugin_active.store(chain.active(), Ordering::Relaxed);
            if failed {
                // The views are rebuilt on the UI thread, not here.
                self.plugin_views_dirty.store(true, Ordering::Release);
            }
        }
    }
}
