//! State shared between the input callback, the output callback and the
//! UI: the ring buffer, the cushion (its fill target), meters, mix settings
//! and what each side really runs at.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use super::eq::{EqSettings, EqSlot};
use super::plugins::{PluginChain, PluginPref, PluginView};
use super::types::{CushionMode, Snapshot};

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
    /// The master plugin insert chain and a cheap "is anything enabled?" flag so
    /// the audio callback skips the lock when no effects are loaded.
    plugins: Mutex<PluginChain>,
    plugin_active: AtomicUsize,
    /// The two built-in EQs (pre and post inserts).  Each version counter
    /// lets the audio callback notice a change without taking the lock.
    eq: [Mutex<EqSettings>; 2],
    eq_version: [AtomicU64; 2],
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
            plugins: Mutex::new(PluginChain::new(sample_rate)),
            plugin_active: AtomicUsize::new(0),
            eq: [
                Mutex::new(EqSettings::default()),
                Mutex::new(EqSettings::default()),
            ],
            eq_version: [AtomicU64::new(1), AtomicU64::new(1)],
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
    /// before playback starts.
    pub(crate) fn prepare_plugins(&self, sample_rate: u32) {
        if let Ok(mut chain) = self.plugins.lock() {
            chain.refresh_rate(sample_rate);
            self.plugin_active.store(chain.active(), Ordering::Relaxed);
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
                    // The bridge changed rate while the slot was loading, so
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
            Err(_) => return,
        };
        for (host, plugin) in &services {
            super::plugins::service_clap(host, plugin);
        }
    }

    pub(crate) fn plugin_prefs(&self) -> Vec<PluginPref> {
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
        self.set_output_mode("");
    }

    pub(crate) fn set_output_mode(&self, text: impl Into<String>) {
        if let Ok(mut mode) = self.output_mode.lock() {
            *mode = text.into();
        }
    }

    pub(crate) fn set_source_rate(&self, rate: u32) {
        self.source_rate.store(rate, Ordering::Relaxed);
        // A manual cushion is set in milliseconds; re-measure it in frames.
        self.lift_to_floor();
    }

    pub(crate) fn set_output(&self, rate: u32, step_frames: u32, mode: impl Into<String>) {
        self.output_rate.store(rate, Ordering::Relaxed);
        self.output_step_frames
            .store(step_frames, Ordering::Relaxed);
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
    pub(crate) fn push_frame(&self, left: f32, right: f32) {
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
            input_rate: self.source_rate.load(Ordering::Relaxed),
            input_block_frames: self.input_block.load(Ordering::Relaxed) as u32,
            output_rate: self.output_rate.load(Ordering::Relaxed),
            output_step_frames: self.output_step_frames.load(Ordering::Relaxed),
            output_mode: self
                .output_mode
                .lock()
                .map(|mode| mode.clone())
                .unwrap_or_default(),
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
}
