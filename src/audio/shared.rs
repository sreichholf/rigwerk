//! State shared between the input callback, the output callback and the
//! UI: the ring buffer, the cushion (its fill target), meters, mix settings
//! and what each side really runs at.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// Sentinel for [`Shared::path_delay_ns`]: no PipeWire path delay sampled yet.
const PATH_DELAY_UNKNOWN: u64 = u64::MAX;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use super::clap::{ClapPlugin, HostState};

use super::eq::{EqSettings, EqSlot};
use super::gate::GateSettings;
use super::output_map::DeviceControls;
use super::plugins::{PluginChain, PluginPref, PluginView};
use super::types::{CushionMode, Snapshot};

/// Decimated scope bins shown in the Live page. About 250 ms of audio when
/// the hop follows the stream rate.
pub(crate) const SCOPE_LEN: usize = 256;

/// Peak-hold state touched only by the lane's writer thread. Kept out of the
/// atomics so the audio callback does not RMW every sample.
#[repr(align(64))]
struct ScopeWriter {
    accum: f32,
    count: u32,
}

/// Single-writer peak-hold scope lane. Capture fills [`Shared::scope_in`]; the
/// mixer fills [`Shared::scope_out`]. The UI only reads `bins` / `head`.
///
/// Decimation state is plain data behind [`UnsafeCell`]: one producer thread
/// owns each lane, so the hot path stays lock-free and allocation-free. Only
/// publishing a finished bin hits atomics (and `hop`, which the control
/// thread may retune with the stream rate).
struct ScopeLane {
    bins: Vec<AtomicU32>,
    head: AtomicU64,
    hop: AtomicU32,
    writer: UnsafeCell<ScopeWriter>,
}

// SAFETY: each lane has a single designated writer thread for `writer` and
// for publishing into `bins`/`head`. The UI only loads those atomics.
unsafe impl Sync for ScopeLane {}

impl ScopeLane {
    fn new() -> Self {
        Self {
            bins: (0..SCOPE_LEN).map(|_| AtomicU32::new(0)).collect(),
            head: AtomicU64::new(0),
            // 48 kHz → ~250 ms across SCOPE_LEN until set_rate runs.
            hop: AtomicU32::new(47),
            writer: UnsafeCell::new(ScopeWriter {
                accum: 0.0,
                count: 0,
            }),
        }
    }

    fn set_rate(&self, rate: u32) {
        let hop = ((rate as u64).max(1) * 250 / 1000 / SCOPE_LEN as u64).clamp(1, 512) as u32;
        self.hop.store(hop, Ordering::Relaxed);
    }

    fn push(&self, sample: f32) {
        let hop = self.hop.load(Ordering::Relaxed).max(1);
        // SAFETY: sole writer for this lane (input callback or mixer).
        let writer = unsafe { &mut *self.writer.get() };
        if sample.abs() >= writer.accum.abs() {
            writer.accum = sample;
        }
        writer.count += 1;
        if writer.count < hop {
            return;
        }
        writer.count = 0;
        let value = std::mem::replace(&mut writer.accum, 0.0);
        let index = self.head.fetch_add(1, Ordering::Relaxed);
        self.bins[(index % SCOPE_LEN as u64) as usize].store(value.to_bits(), Ordering::Relaxed);
    }

    fn copy_into(&self, out: &mut [f32]) {
        let len = out.len().min(SCOPE_LEN);
        if len == 0 {
            return;
        }
        let head = self.head.load(Ordering::Relaxed);
        let start = head.wrapping_sub(len as u64);
        for (offset, slot) in out[..len].iter_mut().enumerate() {
            let index = start.wrapping_add(offset as u64) % SCOPE_LEN as u64;
            *slot = f32::from_bits(self.bins[index as usize].load(Ordering::Relaxed));
        }
    }

    fn clear(&self) {
        for bin in &self.bins {
            bin.store(0.0f32.to_bits(), Ordering::Relaxed);
        }
        self.head.store(0, Ordering::Relaxed);
        // SAFETY: reset runs before callbacks touch a fresh session.
        let writer = unsafe { &mut *self.writer.get() };
        writer.accum = 0.0;
        writer.count = 0;
    }
}

/// A CLAP plugin and its host state, as `service_plugins` services them.
type ClapService = (Arc<HostState>, Arc<Mutex<ClapPlugin>>);

/// Armed by `edit_chain` around the edit itself.  If the edit panics, the
/// chain mutex is poisoned and no later edit publishes again, so the
/// published views and CLAP services are cleared here instead of being left
/// to describe (and hold references to) slots the edit may have changed.
struct PublishOnUnwind<'a> {
    shared: &'a Shared,
}

impl Drop for PublishOnUnwind<'_> {
    fn drop(&mut self) {
        self.shared.publish_poisoned();
    }
}

pub(crate) struct Shared {
    /// Interleaved stereo samples as `f32` bit patterns.  The input side alone
    /// writes the slots at `head`; the output side alone writes `tail`, so the
    /// two never need a lock or touch the same index.
    ring: Vec<AtomicU32>,
    /// Monotonic frame counters.  `head - tail` is the number of frames ready
    /// to read; slots are `counter % capacity_frames`.
    head: AtomicU64,
    tail: AtomicU64,
    pub(crate) capacity_frames: usize,
    pub(crate) target_frames: AtomicU64,
    /// Starting cushion from the chosen buffer (Safe mode's floor).
    pub(crate) base_frames: AtomicU64,
    /// The steady block sizes each side really delivers, in input frames.
    pub(crate) input_block: AtomicU64,
    pub(crate) output_block: AtomicU64,
    /// How the cushion is sized; see [`CushionMode`].
    pub(crate) cushion_mode: AtomicU32,
    pub(crate) manual_cushion_ms: AtomicU32,
    pub(crate) in_l: AtomicU32,
    pub(crate) in_r: AtomicU32,
    pub(crate) out_l: AtomicU32,
    pub(crate) out_r: AtomicU32,
    scope_in: ScopeLane,
    scope_out: ScopeLane,
    pub(crate) underruns: AtomicU64,
    /// Frames the input dropped because the consumer had not kept up.
    pub(crate) overruns: AtomicU64,
    /// Frames the input callback delivered.  Unlike `head`, this advances even
    /// when the ring is full, so a stalled capture stream is visible.
    pub(crate) input_frames: AtomicU64,
    pub(crate) frames: AtomicU64,
    pub(crate) drift_ppm: AtomicU32,
    pub(crate) running: AtomicBool,
    pub(crate) input_gain: AtomicU32,
    pub(crate) output_gain: AtomicU32,
    pub(crate) mute: AtomicBool,
    pub(crate) monitor: AtomicBool,
    pub(crate) mono: AtomicBool,
    pub(crate) input_enabled: [AtomicBool; 2],
    pub(crate) output_enabled: [AtomicBool; 2],
    pub(crate) routes: [AtomicBool; 8],
    /// The clock device's channel map, level and mute (applied after the
    /// master) and its per-channel meters.
    pub(crate) clock_device: DeviceControls,
    pub(crate) drift_enabled: AtomicBool,
    /// Set while input and output run in one PipeWire graph callback (the
    /// duplex node).  The ring, cushion and drift steering are then unused.
    pub(crate) direct: AtomicBool,
    pub(crate) status: Mutex<String>,
    pub(crate) error: Mutex<Option<String>>,
    pub(crate) device_lost: AtomicBool,
    /// The rate frames are pushed into the ring at.  Outputs convert from this
    /// rate to their own, so an input that could not take the requested rate
    /// no longer plays back at the wrong speed and pitch.
    pub(crate) source_rate: AtomicU32,
    pub(crate) output_rate: AtomicU32,
    pub(crate) output_step_frames: AtomicU32,
    pub(crate) output_mode: Mutex<String>,
    /// Nanoseconds from our playback stream to the graph edge (`pw_time.delay`).
    /// [`PATH_DELAY_UNKNOWN`] until a native PipeWire playback callback writes it.
    pub(crate) path_delay_ns: AtomicU64,
    /// The CLAP plugins the UI services every frame, published with the
    /// views so `service_plugins` does not take the chain lock to find them.
    ///
    /// Declared before `plugins` on purpose: fields drop in order, so these
    /// extra references are gone before the chain hands its slots to the
    /// deferred queue, and the main thread's drain is always the last owner.
    plugin_services: Mutex<Arc<[ClapService]>>,
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
    /// The master plugin insert chain and a cheap "is anything enabled?" flag so
    /// the audio callback skips the lock when no effects are loaded.
    plugins: Mutex<PluginChain>,
    plugin_active: AtomicUsize,
    /// The two built-in EQs (pre and post inserts).  Each version counter
    /// lets the audio callback notice a change without taking the lock.
    eq: [Mutex<EqSettings>; 2],
    eq_version: [AtomicU64; 2],
    /// Built-in noise gate (PRE or POST inserts).
    gate: Mutex<GateSettings>,
    gate_version: AtomicU64,
}

impl Shared {
    pub(crate) fn new(sample_rate: u32) -> Self {
        let capacity_frames = (sample_rate as usize * 2).max(4096);
        let chain = PluginChain::new(sample_rate);
        Self {
            ring: (0..capacity_frames * 2)
                .map(|_| AtomicU32::new(0))
                .collect(),
            head: AtomicU64::new(0),
            tail: AtomicU64::new(0),
            capacity_frames,
            target_frames: AtomicU64::new((sample_rate as u64 / 187).max(256)),
            base_frames: AtomicU64::new((sample_rate as u64 / 187).max(256)),
            input_block: AtomicU64::new(0),
            output_block: AtomicU64::new(0),
            cushion_mode: AtomicU32::new(CushionMode::Safe as u32),
            manual_cushion_ms: AtomicU32::new(10.0f32.to_bits()),
            in_l: AtomicU32::new(0.0f32.to_bits()),
            in_r: AtomicU32::new(0.0f32.to_bits()),
            out_l: AtomicU32::new(0.0f32.to_bits()),
            out_r: AtomicU32::new(0.0f32.to_bits()),
            scope_in: ScopeLane::new(),
            scope_out: ScopeLane::new(),
            underruns: AtomicU64::new(0),
            overruns: AtomicU64::new(0),
            input_frames: AtomicU64::new(0),
            frames: AtomicU64::new(0),
            drift_ppm: AtomicU32::new(0.0f32.to_bits()),
            running: AtomicBool::new(false),
            input_gain: AtomicU32::new(1.0f32.to_bits()),
            output_gain: AtomicU32::new(1.0f32.to_bits()),
            mute: AtomicBool::new(false),
            monitor: AtomicBool::new(true),
            mono: AtomicBool::new(false),
            input_enabled: [AtomicBool::new(true), AtomicBool::new(true)],
            output_enabled: [AtomicBool::new(true), AtomicBool::new(true)],
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
            clock_device: DeviceControls::new(),
            drift_enabled: AtomicBool::new(true),
            direct: AtomicBool::new(false),
            status: Mutex::new("Waiting".to_owned()),
            error: Mutex::new(None),
            device_lost: AtomicBool::new(false),
            source_rate: AtomicU32::new(0),
            output_rate: AtomicU32::new(0),
            output_step_frames: AtomicU32::new(0),
            output_mode: Mutex::new(String::new()),
            path_delay_ns: AtomicU64::new(PATH_DELAY_UNKNOWN),
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

    /// Copies the settings that outlive one bridge start into this fresh
    /// session.  Live plugin instances belong to the session that opened them;
    /// only the saved preferences move, and the chain rebuilds from them.
    pub(crate) fn adopt_settings_from(&mut self, previous: &Shared) {
        self.input_gain.store(
            previous.input_gain.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        self.output_gain.store(
            previous.output_gain.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        self.set_mix(
            previous.mute.load(Ordering::Relaxed),
            previous.monitor.load(Ordering::Relaxed),
            previous.mono.load(Ordering::Relaxed),
            [
                previous.input_enabled[0].load(Ordering::Relaxed),
                previous.input_enabled[1].load(Ordering::Relaxed),
            ],
            [
                previous.output_enabled[0].load(Ordering::Relaxed),
                previous.output_enabled[1].load(Ordering::Relaxed),
            ],
            previous.drift_enabled.load(Ordering::Relaxed),
        );
        let mut routes = [[false; 4]; 2];
        for (input, row) in routes.iter_mut().enumerate() {
            for (output, enabled) in row.iter_mut().enumerate() {
                *enabled = previous.routes[input * 4 + output].load(Ordering::Relaxed);
            }
        }
        self.set_routes(routes);
        self.clock_device
            .adopt_settings_from(&previous.clock_device);
        self.cushion_mode.store(
            previous.cushion_mode.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        self.manual_cushion_ms.store(
            previous.manual_cushion_ms.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
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
    fn edit_chain<R>(&self, edit: impl FnOnce(&mut PluginChain) -> R) -> Option<R> {
        let (result, stale) = {
            let mut chain = self.plugins.lock().ok()?;
            // Declared after the chain guard, so it runs first while
            // unwinding, still under the lock.
            let unwind = PublishOnUnwind { shared: self };
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

    /// Opens any unloaded slots (and reloads them when the rate changed)
    /// before playback starts.  Call it from the UI thread.  The bridge
    /// does not load plugins.
    pub(crate) fn prepare_plugins(&self, sample_rate: u32) {
        self.reload_plugins_for_rate(sample_rate);
    }

    /// Reloads the chain when the opened device rate differs from the rate
    /// plugins were built at, and opens slots that were only restored.
    /// Runs only on the recorded main thread.  Any other caller returns
    /// without loading or destroying a plugin.
    pub(crate) fn align_plugins_to_output_rate(&self) {
        if !super::clap::is_main_thread() {
            return;
        }
        let rate = self.output_rate.load(Ordering::Relaxed);
        if rate == 0 {
            return;
        }
        // The common case on every UI frame: the chain already runs at the
        // device rate and every slot was opened.  Answer it from the
        // published flags instead of taking the chain lock.
        if rate == self.chain_rate.load(Ordering::Relaxed)
            && !self.chain_unopened.load(Ordering::Relaxed)
        {
            return;
        }
        self.reload_plugins_for_rate(rate);
    }

    /// Loads outside the chain lock and drops the replaced slots only after
    /// releasing it.  `0` means 48 kHz, matching the chain's own fallback.
    fn reload_plugins_for_rate(&self, sample_rate: u32) {
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
    pub(crate) fn plugin_handle(&self, index: usize) -> Option<super::plugins::PluginHandle> {
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

    /// Runs the CLAP main-thread callbacks and timers once per UI frame,
    /// outside the chain lock: a plugin's timer can do arbitrary work.  The
    /// plugins come from the published list, so a quiet frame takes no
    /// chain lock at all.
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
        // The device may have opened at a different rate than `prepare_plugins`
        // used.  Both the reload and any plugins a bridge thread deferred are
        // handled on this thread, which is the UI thread.
        self.align_plugins_to_output_rate();
        super::plugins::drain_deferred_plugins();
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

    pub(crate) fn reset(&self) {
        self.in_l.store(0.0f32.to_bits(), Ordering::Relaxed);
        self.in_r.store(0.0f32.to_bits(), Ordering::Relaxed);
        self.out_l.store(0.0f32.to_bits(), Ordering::Relaxed);
        self.out_r.store(0.0f32.to_bits(), Ordering::Relaxed);
        self.clock_device.reset();
        self.scope_in.clear();
        self.scope_out.clear();
        self.underruns.store(0, Ordering::Relaxed);
        self.overruns.store(0, Ordering::Relaxed);
        self.input_frames.store(0, Ordering::Relaxed);
        self.frames.store(0, Ordering::Relaxed);
        self.drift_ppm.store(0.0f32.to_bits(), Ordering::Relaxed);
        self.running.store(false, Ordering::Relaxed);
        self.direct.store(false, Ordering::Relaxed);
        // Only called on a session no callback has touched yet (every start
        // and preview builds a fresh `Shared`), so the order of the two
        // stores does not matter.
        self.head.store(0, Ordering::Relaxed);
        self.tail.store(0, Ordering::Relaxed);
        self.set_status("Starting bridge");
        self.device_lost.store(false, Ordering::Relaxed);
        if let Ok(mut error) = self.error.lock() {
            *error = None;
        }
        self.source_rate.store(0, Ordering::Relaxed);
        self.input_block.store(0, Ordering::Relaxed);
        self.output_block.store(0, Ordering::Relaxed);
        self.output_rate.store(0, Ordering::Relaxed);
        self.output_step_frames.store(0, Ordering::Relaxed);
        self.path_delay_ns
            .store(PATH_DELAY_UNKNOWN, Ordering::Relaxed);
        self.set_output_mode("");
    }

    /// Records the graph/device delay past our playback node, in nanoseconds.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn set_path_delay_ns(&self, ns: u64) {
        self.path_delay_ns.store(ns, Ordering::Relaxed);
    }

    pub(crate) fn path_delay_ms(&self) -> Option<f32> {
        let ns = self.path_delay_ns.load(Ordering::Relaxed);
        (ns != PATH_DELAY_UNKNOWN).then_some(ns as f32 / 1_000_000.0)
    }

    pub(crate) fn set_output_mode(&self, text: impl Into<String>) {
        if let Ok(mut mode) = self.output_mode.lock() {
            *mode = text.into();
        }
    }

    pub(crate) fn set_source_rate(&self, rate: u32) {
        self.source_rate.store(rate, Ordering::Relaxed);
        self.scope_in.set_rate(rate);
        // A manual cushion is set in milliseconds; re-measure it in frames.
        self.lift_to_floor();
    }

    /// Publishes the one graph rate both sides of the duplex node run at.
    /// Atomics only, so the audio thread may call it when the graph rate
    /// changes; the mode text is updated later from the main loop.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn note_direct_rate(&self, rate: u32) {
        self.source_rate.store(rate, Ordering::Relaxed);
        self.scope_in.set_rate(rate);
        self.output_rate.store(rate, Ordering::Relaxed);
        self.scope_out.set_rate(rate);
    }

    pub(crate) fn set_output(&self, rate: u32, step_frames: u32, mode: impl Into<String>) {
        self.output_rate.store(rate, Ordering::Relaxed);
        self.output_step_frames
            .store(step_frames, Ordering::Relaxed);
        self.scope_out.set_rate(rate);
        self.set_output_mode(mode);
    }

    /// Sets the starting cushion for a new bridge.
    pub(crate) fn set_target_frames(&self, frames: usize) {
        self.base_frames
            .store(frames.max(64) as u64, Ordering::Relaxed);
        self.target_frames
            .store(self.floor_frames() as u64, Ordering::Relaxed);
    }

    pub(crate) fn target_frames(&self) -> usize {
        self.target_frames.load(Ordering::Relaxed) as usize
    }

    pub(crate) fn cushion_mode(&self) -> CushionMode {
        CushionMode::from_u32(self.cushion_mode.load(Ordering::Relaxed))
    }

    /// The smallest cushion the current mode allows, in input frames.
    pub(crate) fn floor_frames(&self) -> usize {
        let base = self.base_frames.load(Ordering::Relaxed) as usize;
        let input = self.input_block.load(Ordering::Relaxed) as usize;
        let output = self.output_block.load(Ordering::Relaxed) as usize;
        let floor = match self.cushion_mode() {
            // Today's behaviour: twice the input buffer, never less than one
            // input block or two output blocks.
            CushionMode::Safe => base.max(input).max(output * 2),
            // The least that can work: one block from each side.
            CushionMode::LowLatency => {
                if input + output == 0 {
                    base
                } else {
                    input + output
                }
            }
            CushionMode::Manual => {
                let ms = f32::from_bits(self.manual_cushion_ms.load(Ordering::Relaxed));
                let rate = match self.source_rate.load(Ordering::Relaxed) {
                    0 => 48_000,
                    rate => rate,
                };
                (ms.max(0.0) * rate as f32 / 1000.0).round() as usize
            }
        };
        floor.clamp(32, self.capacity_frames / 2)
    }

    /// Brings the target up to the floor; Manual pins it there exactly.
    pub(crate) fn lift_to_floor(&self) {
        let floor = self.floor_frames() as u64;
        let current = self.target_frames.load(Ordering::Relaxed);
        if self.cushion_mode() == CushionMode::Manual {
            if current != floor {
                self.target_frames.store(floor, Ordering::Relaxed);
            }
        } else if current < floor {
            self.target_frames.store(floor, Ordering::Relaxed);
        }
    }

    pub(crate) fn note_input_block(&self, frames: usize) {
        if self.input_block.swap(frames as u64, Ordering::Relaxed) != frames as u64 {
            self.lift_to_floor();
        }
    }

    /// Called by every input callback.  It advances independently of the ring,
    /// so a capture stream that stops delivering is distinguishable from a
    /// consumer that is simply staying ahead.
    pub(crate) fn note_input_frames(&self, frames: usize) {
        self.input_frames
            .fetch_add(frames as u64, Ordering::Relaxed);
    }

    pub(crate) fn note_output_block(&self, frames: usize) {
        if self.output_block.swap(frames as u64, Ordering::Relaxed) != frames as u64 {
            self.lift_to_floor();
        }
    }

    /// Applies a cushion choice while the bridge runs.  A new mode or manual
    /// size starts from its own floor straight away.
    pub(crate) fn set_cushion(&self, mode: CushionMode, manual_ms: f32) {
        let mode_changed = self.cushion_mode.swap(mode as u32, Ordering::Relaxed) != mode as u32;
        let ms_changed = self
            .manual_cushion_ms
            .swap(manual_ms.to_bits(), Ordering::Relaxed)
            != manual_ms.to_bits();
        if mode_changed || (ms_changed && mode == CushionMode::Manual) {
            self.target_frames
                .store(self.floor_frames() as u64, Ordering::Relaxed);
        }
    }

    pub(crate) fn raise_target_after_underrun(&self) {
        let step = match self.cushion_mode() {
            CushionMode::Safe => 256,
            // Smaller steps, so one dropout does not undo the low setting.
            CushionMode::LowLatency => (self.output_block.load(Ordering::Relaxed) / 4).max(64),
            // Manual stays where the person put it.
            CushionMode::Manual => return,
        };
        let floor = self.floor_frames() as u64;
        let current = self.target_frames.load(Ordering::Relaxed);
        let ceiling = (self.capacity_frames / 2) as u64;
        let next = (current + step).min(ceiling).max(floor);
        if next != current {
            self.target_frames.store(next, Ordering::Relaxed);
        }
    }

    /// How long playback must stay clean before the cushion shrinks a step.
    pub(crate) fn relax_after(&self) -> Duration {
        match self.cushion_mode() {
            CushionMode::LowLatency => Duration::from_secs(2),
            _ => Duration::from_secs(4),
        }
    }

    pub(crate) fn relax_target(&self) {
        let step = match self.cushion_mode() {
            CushionMode::Safe => 64,
            CushionMode::LowLatency => 32,
            CushionMode::Manual => return,
        };
        let floor = self.floor_frames() as u64;
        let current = self.target_frames.load(Ordering::Relaxed);
        if current > floor {
            self.target_frames
                .store(current.saturating_sub(step).max(floor), Ordering::Relaxed);
        }
    }

    pub(crate) fn set_mix(
        &self,
        mute: bool,
        monitor: bool,
        mono: bool,
        input_channels: [bool; 2],
        output_channels: [bool; 2],
        drift: bool,
    ) {
        self.mute.store(mute, Ordering::Relaxed);
        self.monitor.store(monitor, Ordering::Relaxed);
        self.mono.store(mono, Ordering::Relaxed);
        self.input_enabled[0].store(input_channels[0], Ordering::Relaxed);
        self.input_enabled[1].store(input_channels[1], Ordering::Relaxed);
        self.output_enabled[0].store(output_channels[0], Ordering::Relaxed);
        self.output_enabled[1].store(output_channels[1], Ordering::Relaxed);
        self.drift_enabled.store(drift, Ordering::Relaxed);
    }

    pub(crate) fn set_routes(&self, routes: [[bool; 4]; 2]) {
        for (input, row) in routes.iter().enumerate() {
            for (output, enabled) in row.iter().enumerate() {
                self.routes[input * 4 + output].store(*enabled, Ordering::Relaxed);
            }
        }
    }

    pub(crate) fn set_status(&self, text: impl Into<String>) {
        if let Ok(mut status) = self.status.lock() {
            *status = text.into();
        }
    }

    pub(crate) fn set_error(&self, text: impl Into<String>) {
        let text = text.into();
        self.running.store(false, Ordering::Relaxed);
        self.set_status("Bridge stopped");
        if let Ok(mut error) = self.error.lock() {
            *error = Some(text);
        }
    }

    pub(crate) fn set_device_lost(&self, text: impl Into<String>) {
        self.device_lost.store(true, Ordering::Relaxed);
        self.set_error(text);
    }

    pub(crate) fn input_gain(&self) -> f32 {
        f32::from_bits(self.input_gain.load(Ordering::Relaxed))
    }

    pub(crate) fn output_gain(&self) -> f32 {
        f32::from_bits(self.output_gain.load(Ordering::Relaxed))
    }

    /// Called by the one input producer.  Never blocks: a full ring counts an
    /// overrun and drops this frame, so the consumer is never pushed aside.
    ///
    /// The Live scope is a side channel: it does not change ring capacity,
    /// head/tail timing, or which frames the mixer receives.
    pub(crate) fn push_frame(&self, left: f32, right: f32) {
        // Observe capture even when the play ring is full and this frame drops.
        self.scope_in.push(0.5 * (left + right));
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        if head.wrapping_sub(tail) >= self.capacity_frames as u64 {
            self.overruns.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let index = (head % self.capacity_frames as u64) as usize * 2;
        // The release store of `head` publishes both samples to the consumer.
        self.ring[index].store(left.to_bits(), Ordering::Relaxed);
        self.ring[index + 1].store(right.to_bits(), Ordering::Relaxed);
        self.head.store(head.wrapping_add(1), Ordering::Release);
    }

    /// Peak-hold into the input scope for a path that does not go through
    /// the ring (the PipeWire duplex node).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn push_scope_in(&self, left: f32, right: f32) {
        self.scope_in.push(0.5 * (left + right));
    }

    /// Peak-hold into the output scope after the sample is fully processed.
    /// Does not modify `left`/`right` or the device write.
    pub(crate) fn push_scope_out(&self, left: f32, right: f32) {
        self.scope_out.push(0.5 * (left + right));
    }

    /// Oldest→newest decimated waveform bins for the Live scope.
    pub(crate) fn copy_scope(&self, input: &mut [f32], output: &mut [f32]) {
        self.scope_in.copy_into(input);
        self.scope_out.copy_into(output);
    }

    /// Called by the one output consumer.  Never blocks.
    pub(crate) fn pop_frame(&self) -> Option<(f32, f32)> {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        if tail == head {
            return None;
        }
        let index = (tail % self.capacity_frames as u64) as usize * 2;
        // The acquire load above paired with the producer's release store.
        let left = f32::from_bits(self.ring[index].load(Ordering::Relaxed));
        let right = f32::from_bits(self.ring[index + 1].load(Ordering::Relaxed));
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        Some((left, right))
    }

    pub(crate) fn ring_fill(&self) -> f32 {
        let frames = self.ring_frames() as f32;
        let target = self.target_frames().max(1) as f32;
        (frames / target).clamp(0.0, 1.0)
    }

    pub(crate) fn ring_frames(&self) -> usize {
        // Either side may advance between the loads, so saturate rather than
        // wrapping a momentary tail-ahead-of-head observation into a huge fill.
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Relaxed);
        head.saturating_sub(tail).min(self.capacity_frames as u64) as usize
    }

    pub(crate) fn snapshot(&self) -> Snapshot {
        Snapshot {
            in_l: f32::from_bits(self.in_l.load(Ordering::Relaxed)),
            in_r: f32::from_bits(self.in_r.load(Ordering::Relaxed)),
            out_l: f32::from_bits(self.out_l.load(Ordering::Relaxed)),
            out_r: f32::from_bits(self.out_r.load(Ordering::Relaxed)),
            out_channels: self.clock_device.meters(),
            output_channels: self.clock_device.channels(),
            underruns: self.underruns.load(Ordering::Relaxed),
            overruns: self.overruns.load(Ordering::Relaxed),
            input_frames: self.input_frames.load(Ordering::Relaxed),
            frames: self.frames.load(Ordering::Relaxed),
            ring_fill: self.ring_fill(),
            target_frames: self.target_frames(),
            drift_ppm: f32::from_bits(self.drift_ppm.load(Ordering::Relaxed)),
            error: self.error.lock().ok().and_then(|error| error.clone()),
            device_lost: self.device_lost.load(Ordering::Relaxed),
            status: self
                .status
                .lock()
                .map(|status| status.clone())
                .unwrap_or_default(),
            input_rate: self.source_rate.load(Ordering::Relaxed),
            input_block_frames: self.input_block.load(Ordering::Relaxed) as u32,
            output_rate: self.output_rate.load(Ordering::Relaxed),
            output_step_frames: self.output_step_frames.load(Ordering::Relaxed),
            output_mode: self
                .output_mode
                .lock()
                .map(|mode| mode.clone())
                .unwrap_or_default(),
            path_delay_ms: self.path_delay_ms(),
            direct: self.direct.load(Ordering::Relaxed),
        }
    }
}

/// Starting ring target for a given device buffer.  Two blocks is enough to
/// start with; an underrun grows it and a calm stretch shrinks it again.
pub(crate) fn initial_target(buffer_frames: usize) -> usize {
    (buffer_frames * 2).clamp(96, 4096)
}

#[cfg(test)]
mod tests {
    use crate::audio::CushionMode;

    /// A bridge like the BT-W3 case: 128-sample input, 480-sample output step.
    fn cushion_bridge(mode: CushionMode, manual_ms: f32) -> super::Shared {
        let shared = super::Shared::new(48_000);
        shared.set_cushion(mode, manual_ms);
        shared.set_source_rate(48_000);
        shared.set_target_frames(super::initial_target(128));
        shared.note_input_block(128);
        shared.note_output_block(480);
        shared
    }

    #[test]
    fn scope_lane_keeps_peak_hold_bins_in_order() {
        let shared = super::Shared::new(48_000);
        shared.set_source_rate(48_000);
        let hop = ((48_000u64 * 250) / 1000 / super::SCOPE_LEN as u64).clamp(1, 512) as usize;
        // Quiet samples, then a louder one that must win the first closed bin.
        for _ in 0..hop - 1 {
            shared.push_frame(0.1, 0.1);
        }
        shared.push_frame(0.8, 0.8);
        for _ in 0..hop {
            shared.push_frame(-0.5, -0.5);
        }
        let mut input = vec![0.0; super::SCOPE_LEN];
        let mut output = vec![0.0; super::SCOPE_LEN];
        shared.copy_scope(&mut input, &mut output);
        assert!(
            (input[super::SCOPE_LEN - 2] - 0.8).abs() < 1.0e-5,
            "first closed bin should hold the louder sample, got {}",
            input[super::SCOPE_LEN - 2]
        );
        assert!(
            (input[super::SCOPE_LEN - 1] + 0.5).abs() < 1.0e-5,
            "second bin should keep sign of the peak, got {}",
            input[super::SCOPE_LEN - 1]
        );
        assert!(output.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn safe_cushion_keeps_two_output_blocks_and_grows_256() {
        let shared = cushion_bridge(CushionMode::Safe, 10.0);
        assert_eq!(shared.target_frames(), 960);
        shared.raise_target_after_underrun();
        assert_eq!(shared.target_frames(), 1216);
        shared.relax_target();
        assert_eq!(shared.target_frames(), 1152);
    }

    #[test]
    fn low_latency_cushion_is_one_block_from_each_side() {
        let shared = cushion_bridge(CushionMode::LowLatency, 10.0);
        assert_eq!(shared.target_frames(), 608);
        shared.raise_target_after_underrun();
        assert_eq!(shared.target_frames(), 728);
        shared.relax_target();
        assert_eq!(shared.target_frames(), 696);
    }

    #[test]
    fn manual_cushion_stays_where_it_is_put() {
        let shared = cushion_bridge(CushionMode::Manual, 7.5);
        assert_eq!(shared.target_frames(), 360);
        shared.raise_target_after_underrun();
        assert_eq!(shared.target_frames(), 360);
        // Changing the size while running applies at once.
        shared.set_cushion(CushionMode::Manual, 12.0);
        assert_eq!(shared.target_frames(), 576);
    }

    #[test]
    fn switching_mode_while_running_applies_at_once() {
        let shared = cushion_bridge(CushionMode::Safe, 10.0);
        shared.raise_target_after_underrun();
        shared.set_cushion(CushionMode::LowLatency, 10.0);
        assert_eq!(shared.target_frames(), 608);
    }

    /// The CLAP main thread is process-wide.  These cases claim it for the
    /// calling thread and must not overlap.  The thread id is restored
    /// before the serial lock is released.
    fn claim_main_thread() -> MainClaim {
        static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let serial = SERIAL.lock().unwrap_or_else(|poison| poison.into_inner());
        MainClaim {
            _thread: crate::audio::clap::claim_main_thread_for_test(),
            _serial: serial,
        }
    }

    struct MainClaim {
        // Dropped first, while `serial` is still held.
        _thread: crate::audio::clap::MainThreadClaim,
        _serial: std::sync::MutexGuard<'static, ()>,
    }

    fn chain_rate(shared: &super::Shared) -> u32 {
        shared
            .plugins
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .rate()
    }

    fn built_rates(shared: &super::Shared) -> Vec<Option<u32>> {
        shared
            .plugins
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .built_rates()
    }

    #[test]
    fn publishing_output_rate_reloads_only_on_the_main_thread() {
        let _main = claim_main_thread();
        let shared = super::Shared::new(48_000);
        shared.restore_plugins(vec![crate::audio::PluginPref {
            path: "missing-rate-probe.vst3".to_owned(),
            ..crate::audio::PluginPref::default()
        }]);
        assert_eq!(chain_rate(&shared), 48_000);
        assert_eq!(built_rates(&shared), vec![None]);

        // Publishing the device rate does not itself rebuild the chain.
        shared.set_output(44_100, 128, "test output");
        assert_eq!(chain_rate(&shared), 48_000);
        assert_eq!(built_rates(&shared), vec![None]);

        std::thread::scope(|scope| {
            scope.spawn(|| shared.align_plugins_to_output_rate());
        });
        assert_eq!(
            built_rates(&shared),
            vec![None],
            "a bridge thread must not open the empty slot"
        );

        // The follow-up `service_plugins` calls, invoked directly.
        shared.align_plugins_to_output_rate();
        assert_eq!(chain_rate(&shared), 44_100);
        assert_eq!(built_rates(&shared), vec![Some(44_100)]);

        // A later device rate still waits for that follow-up, including the
        // one `service_plugins` runs each frame.
        shared.set_output(48_000, 128, "test output");
        assert_eq!(chain_rate(&shared), 44_100);
        assert_eq!(built_rates(&shared), vec![Some(44_100)]);
        shared.service_plugins();
        assert_eq!(chain_rate(&shared), 48_000);
        assert_eq!(built_rates(&shared), vec![Some(48_000)]);
    }

    #[test]
    fn off_thread_drop_defers_plugin_slots_until_the_main_thread() {
        let _main = claim_main_thread();
        let plugins = crate::audio::plugins::tracked_slots_alive;
        let queued = crate::audio::plugins::deferred_slot_count;

        // The UI thread drops the chain immediately.
        {
            let shared = super::Shared::new(48_000);
            shared
                .plugins
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .insert_tracked_slot_for_test();
            assert_eq!(plugins(), 1);
            drop(shared);
            assert_eq!(plugins(), 0);
            assert_eq!(queued(), 0);
        }

        let shared = super::Shared::new(48_000);
        shared
            .plugins
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert_tracked_slot_for_test();
        assert_eq!(plugins(), 1);
        std::thread::scope(|scope| {
            scope.spawn(move || drop(shared));
        });
        assert_eq!(plugins(), 1, "the slot stays alive on the deferred queue");
        assert_eq!(queued(), 1);
        crate::audio::plugins::drain_deferred_plugins();
        assert_eq!(plugins(), 0, "the main thread drops the deferred slot");
        assert_eq!(queued(), 0);
    }

    fn missing(path: &str) -> crate::audio::PluginPref {
        crate::audio::PluginPref {
            path: path.to_owned(),
            name: path.to_owned(),
            ..crate::audio::PluginPref::default()
        }
    }

    fn names(shared: &super::Shared) -> Vec<String> {
        shared
            .plugin_views()
            .iter()
            .map(|view| view.name.clone())
            .collect()
    }

    /// Asserts that a publish happened since `last` and remembers it.
    fn assert_published(shared: &super::Shared, last: &mut u64) {
        let version = shared.plugin_views_version();
        assert!(version > *last, "the edit did not republish the views");
        *last = version;
    }

    #[test]
    fn published_views_follow_every_chain_edit() {
        let _main = claim_main_thread();
        let shared = super::Shared::new(48_000);
        assert!(shared.plugin_views().is_empty());
        let mut version = shared.plugin_views_version();

        shared.restore_plugins(vec![missing("a.vst3"), missing("b.vst3")]);
        assert_published(&shared, &mut version);
        assert_eq!(names(&shared), ["a.vst3", "b.vst3"]);

        shared.set_plugin_bypass(1, true);
        assert_published(&shared, &mut version);
        assert!(shared.plugin_views()[1].bypass);

        shared.set_plugin_mix(0, 0.25);
        assert_published(&shared, &mut version);
        assert_eq!(shared.plugin_views()[0].mix, 0.25);

        shared.move_plugin(0, 1);
        assert_published(&shared, &mut version);
        assert_eq!(names(&shared), ["b.vst3", "a.vst3"]);

        // Opening a slot whose file is missing leaves its error visible.
        shared.ensure_plugin_loaded(0);
        assert_published(&shared, &mut version);
        assert!(shared.plugin_views()[0].error.is_some());

        assert!(shared.set_plugin_parameter(1, 7, 0.5).is_err());
        assert_published(&shared, &mut version);
        assert!(shared.plugin_views()[1].error.is_some());

        shared.remove_plugin(0);
        assert_published(&shared, &mut version);
        assert_eq!(names(&shared), ["a.vst3"]);

        // A failed add stays in the list with its error.
        assert!(shared.add_plugin(missing("c.vst3")).is_err());
        assert_published(&shared, &mut version);
        assert_eq!(names(&shared), ["a.vst3", "c.vst3"]);
        assert!(shared.plugin_views()[1].error.is_some());

        // A snapshot the UI already holds is not changed by later edits.
        let held = shared.plugin_views();
        shared.restore_plugins(Vec::new());
        assert_eq!(held.len(), 2);
        assert!(shared.plugin_views().is_empty());
    }

    /// Holds the chain lock on this thread while the UI-side calls run on a
    /// worker that stands in for the main thread.  None of them may wait for
    /// the lock: the worker must finish while it is still held.
    #[test]
    fn drawing_and_quiet_service_frames_never_wait_for_the_chain_lock() {
        let _main = claim_main_thread();
        let shared = super::Shared::new(48_000);
        shared.restore_plugins(vec![missing("a.vst3")]);
        shared.set_output(48_000, 128, "test output");
        // Opens the restored slot (it fails), so the chain is settled.
        shared.service_plugins();
        assert!(shared.plugin_views()[0].error.is_some());
        shared.edit_chain(|chain| {
            chain.insert_clap_for_test(crate::audio::clap::tests::fake_plugin_for_test(false))
        });
        let Some(handle) = shared.plugin_handle(1) else {
            panic!("the fake is loaded");
        };
        let editor = match &handle {
            crate::audio::PluginHandle::Clap(plugin) => std::sync::Arc::clone(plugin),
            crate::audio::PluginHandle::Vst3(_) => unreachable!(),
        };
        drop(handle);

        const HELD: std::time::Duration = std::time::Duration::from_secs(5);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let chain = shared.plugins.lock().unwrap();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let _worker_main = crate::audio::clap::claim_main_thread_for_test();
                for _ in 0..100 {
                    let views = shared.plugin_views();
                    assert_eq!(views.len(), 2);
                    // The open-editor poll: identity from the published view.
                    assert_eq!(
                        views[1].instance,
                        Some(crate::audio::plugin_instance_id(&editor))
                    );
                    shared.service_plugins();
                    shared.align_plugins_to_output_rate();
                }
                done_tx.send(()).unwrap();
            });
            let finished = done_rx.recv_timeout(HELD);
            drop(chain);
            assert!(finished.is_ok(), "a UI-side call waited for the chain lock");
        });
    }

    #[test]
    fn a_panicking_edit_clears_what_it_published() {
        let _main = claim_main_thread();
        let shared = super::Shared::new(48_000);
        shared.edit_chain(|chain| {
            chain.insert_clap_for_test(crate::audio::clap::tests::fake_plugin_for_test(false))
        });
        assert_eq!(shared.plugin_views().len(), 1);
        assert_eq!(shared.plugin_services.lock().unwrap().len(), 1);
        assert!(shared.has_plugins());

        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            shared.edit_chain(|chain| {
                chain.set_bypass(0, true);
                panic!("an edit failed halfway");
            })
        }));
        assert!(panicked.is_err());
        assert!(shared.plugins.is_poisoned());
        assert!(shared.plugin_views().is_empty());
        assert!(shared.plugin_services.lock().unwrap().is_empty());
        assert!(!shared.has_plugins());
        // A poisoned chain refuses further edits and publishes nothing new.
        shared.set_plugin_bypass(0, false);
        assert!(shared.plugin_views().is_empty());
    }

    #[test]
    fn views_name_the_loaded_instance() {
        let _main = claim_main_thread();
        let shared = super::Shared::new(48_000);
        shared.restore_plugins(vec![missing("a.vst3")]);
        assert_eq!(shared.plugin_views()[0].instance, None);
        shared.edit_chain(|chain| {
            chain.insert_clap_for_test(crate::audio::clap::tests::fake_plugin_for_test(false))
        });
        let Some(handle) = shared.plugin_handle(1) else {
            panic!("the fake is loaded");
        };
        assert_eq!(
            shared.plugin_views()[1].instance,
            Some(handle.instance_id())
        );
        // An editor still holding the old instance no longer matches once
        // the insert is replaced.
        shared.remove_plugin(1);
        shared.edit_chain(|chain| {
            chain.insert_clap_for_test(crate::audio::clap::tests::fake_plugin_for_test(false))
        });
        assert_ne!(
            shared.plugin_views()[1].instance,
            Some(handle.instance_id())
        );
    }

    #[test]
    fn a_processing_failure_is_published_on_the_next_service_frame() {
        let _main = claim_main_thread();
        let shared = super::Shared::new(48_000);
        shared.edit_chain(|chain| {
            chain.insert_clap_for_test(crate::audio::clap::tests::fake_plugin_for_test(true))
        });
        assert!(shared.has_plugins());
        assert!(shared.plugin_views()[0].error.is_none());

        let mut left = vec![0.5f32; 64];
        let mut right = vec![0.5f32; 64];
        let version = shared.plugin_views_version();
        shared.process_plugins(&mut left, &mut right, 64);
        assert!(!shared.has_plugins(), "the failed insert left the chain");
        assert_eq!(
            shared.plugin_views_version(),
            version,
            "the audio thread must not rebuild the views"
        );
        assert!(shared.plugin_views()[0].error.is_none());

        shared.service_plugins();
        let error = shared.plugin_views()[0].error.clone();
        assert!(
            error
                .as_deref()
                .is_some_and(|text| text.starts_with("Processing failed")),
            "{error:?}"
        );
        // Nothing else changed, so the next frame does not publish again.
        let version = shared.plugin_views_version();
        shared.service_plugins();
        assert_eq!(shared.plugin_views_version(), version);
    }

    #[test]
    fn a_clap_port_change_is_published_on_the_next_service_frame() {
        let _main = claim_main_thread();
        let shared = super::Shared::new(48_000);
        shared.edit_chain(|chain| {
            chain.insert_clap_for_test(crate::audio::clap::tests::fake_plugin_for_test(false))
        });
        assert!(shared.plugin_views()[0].error.is_none());
        let Some(crate::audio::PluginHandle::Clap(plugin)) = shared.plugin_handle(0) else {
            panic!("the fake is a loaded CLAP slot");
        };
        crate::audio::clap::tests::rescan_ports_for_test(&plugin.lock().unwrap());
        drop(plugin);
        assert!(shared.plugin_views()[0].error.is_none());

        shared.service_plugins();
        assert_eq!(
            shared.plugin_views()[0].error.as_deref(),
            Some(crate::audio::clap::AUDIO_PORTS_CHANGED)
        );
        assert!(!shared.has_plugins());
    }
}
