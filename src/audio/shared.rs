//! State shared between the input callback, the output callback and the
//! UI: the ring buffer, the cushion (its fill target), meters, mix settings
//! and what each side really runs at.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// Sentinel for [`Shared::path_delay_ns`]: no PipeWire path delay sampled yet.
const PATH_DELAY_UNKNOWN: u64 = u64::MAX;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use super::eq::{EqSettings, EqSlot};
use super::gate::GateSettings;
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
    pub(crate) drift_enabled: AtomicBool,
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
            drift_enabled: AtomicBool::new(true),
            status: Mutex::new("Waiting".to_owned()),
            error: Mutex::new(None),
            device_lost: AtomicBool::new(false),
            source_rate: AtomicU32::new(0),
            output_rate: AtomicU32::new(0),
            output_step_frames: AtomicU32::new(0),
            output_mode: Mutex::new(String::new()),
            path_delay_ns: AtomicU64::new(PATH_DELAY_UNKNOWN),
            plugins: Mutex::new(PluginChain::new(sample_rate)),
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

    /// Replaces the saved insert list without loading anything.  Plugins are
    /// opened when the bridge starts (or when one is added in the UI).
    pub(crate) fn restore_plugins(&self, prefs: Vec<PluginPref>) {
        let old = match self.plugins.lock() {
            Ok(mut chain) => {
                let old = chain.restore(prefs);
                self.plugin_active.store(chain.active(), Ordering::Relaxed);
                old
            }
            Err(_) => Vec::new(),
        };
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
            let plan = {
                let mut chain = match self.plugins.lock() {
                    Ok(chain) => chain,
                    Err(_) => return,
                };
                match chain.reload_plan(rate) {
                    Some(plan) => plan,
                    None => {
                        self.plugin_active.store(chain.active(), Ordering::Relaxed);
                        return;
                    }
                }
            };
            let built: Vec<_> = plan
                .jobs
                .into_iter()
                .map(|(index, pref)| {
                    let (slot, _) = PluginChain::load_slot(pref.clone(), rate);
                    (index, pref, slot)
                })
                .collect();
            let installed = {
                let mut chain = match self.plugins.lock() {
                    Ok(chain) => chain,
                    Err(_) => return,
                };
                match chain.install_reload(plan.previous_rate, rate, built) {
                    Ok(old) => {
                        self.plugin_active.store(chain.active(), Ordering::Relaxed);
                        Some(old)
                    }
                    Err(stale) => {
                        drop(chain);
                        drop(stale);
                        None
                    }
                }
            };
            if let Some(old) = installed {
                drop(old);
                return;
            }
        }
    }

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
            let mut chain = self
                .plugins
                .lock()
                .map_err(|_| "Plugin chain unavailable".to_owned())?;
            match chain.push_slot(slot, rate) {
                Ok(()) => {
                    self.plugin_active.store(chain.active(), Ordering::Relaxed);
                    return result;
                }
                Err(stale) => {
                    // The chain rate changed while the slot was loading, so
                    // it is set up for the wrong rate.  Drop it outside the
                    // chain lock and build a fresh one.
                    drop(chain);
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
        let removed = match self.plugins.lock() {
            Ok(mut chain) => {
                let removed = chain.remove(index);
                self.plugin_active.store(chain.active(), Ordering::Relaxed);
                removed
            }
            Err(_) => None,
        };
        // Destroyed after the chain lock is released.
        drop(removed);
    }

    pub(crate) fn move_plugin(&self, index: usize, delta: isize) {
        if let Ok(mut chain) = self.plugins.lock() {
            chain.move_slot(index, delta);
            self.plugin_active.store(chain.active(), Ordering::Relaxed);
        }
    }

    pub(crate) fn set_plugin_bypass(&self, index: usize, bypass: bool) {
        if let Ok(mut chain) = self.plugins.lock() {
            chain.set_bypass(index, bypass);
            self.plugin_active.store(chain.active(), Ordering::Relaxed);
        }
    }

    pub(crate) fn set_plugin_mix(&self, index: usize, mix: f32) {
        if let Ok(mut chain) = self.plugins.lock() {
            chain.set_mix(index, mix);
            self.plugin_active.store(chain.active(), Ordering::Relaxed);
        }
    }

    pub(crate) fn ensure_plugin_loaded(&self, index: usize) {
        if let Ok(mut chain) = self.plugins.lock() {
            chain.ensure_loaded(index);
        }
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
        let mut chain = self
            .plugins
            .lock()
            .map_err(|_| "Plugin chain unavailable".to_owned())?;
        chain.apply_synced_parameters(index, &handle, parameters);
        Ok(())
    }

    pub(crate) fn set_plugin_parameter(
        &self,
        index: usize,
        id: u32,
        value: f64,
    ) -> Result<(), String> {
        let mut chain = self
            .plugins
            .lock()
            .map_err(|_| "Plugin chain unavailable".to_owned())?;
        let result = chain.set_parameter(index, id, value);
        self.plugin_active.store(chain.active(), Ordering::Relaxed);
        result
    }

    pub(crate) fn reset_plugin_parameters(&self, index: usize) -> Result<(), String> {
        let mut chain = self
            .plugins
            .lock()
            .map_err(|_| "Plugin chain unavailable".to_owned())?;
        let result = chain.reset_parameters(index);
        self.plugin_active.store(chain.active(), Ordering::Relaxed);
        result
    }

    pub(crate) fn plugin_views(&self) -> Vec<PluginView> {
        self.plugins
            .lock()
            .map(|chain| chain.views())
            .unwrap_or_default()
    }

    /// Runs the CLAP main-thread callbacks and timers once per UI frame,
    /// outside the chain lock: a plugin's timer can do arbitrary work.
    pub(crate) fn service_plugins(&self) {
        let services = match self.plugins.lock() {
            Ok(chain) => chain.clap_services(),
            Err(_) => Vec::new(),
        };
        for (host, plugin) in &services {
            super::plugins::service_clap(host, plugin);
        }
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
            chain.process(left, right, frames);
            self.plugin_active.store(chain.active(), Ordering::Relaxed);
        }
    }

    pub(crate) fn reset(&self) {
        self.in_l.store(0.0f32.to_bits(), Ordering::Relaxed);
        self.in_r.store(0.0f32.to_bits(), Ordering::Relaxed);
        self.out_l.store(0.0f32.to_bits(), Ordering::Relaxed);
        self.out_r.store(0.0f32.to_bits(), Ordering::Relaxed);
        self.scope_in.clear();
        self.scope_out.clear();
        self.underruns.store(0, Ordering::Relaxed);
        self.overruns.store(0, Ordering::Relaxed);
        self.input_frames.store(0, Ordering::Relaxed);
        self.frames.store(0, Ordering::Relaxed);
        self.drift_ppm.store(0.0f32.to_bits(), Ordering::Relaxed);
        self.running.store(false, Ordering::Relaxed);
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
}
