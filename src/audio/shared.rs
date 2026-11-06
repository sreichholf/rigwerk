//! State shared between the input callback, the output callback and the
//! UI: the ring buffer, the cushion (its fill target), meters, the master
//! and device settings, the signal paths and what each side really runs at.
//!
//! Everything a path owns (routing, mono, channel enable, EQs, gate, plugin
//! chain, level, mute, solo) lives in [`PathControls`], one per path slot in
//! [`Shared::paths`].  Paths are added, duplicated and removed here, live
//! (see the `path` module docs).  The older accessors (`set_eq`, `set_mix`,
//! ...) act on path 0 and are kept for tests.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// Sentinel for [`Shared::path_delay_ns`]: no PipeWire path delay sampled yet.
const PATH_DELAY_UNKNOWN: u64 = u64::MAX;
#[cfg(test)]
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

#[cfg(test)]
use super::eq::{EqSettings, EqSlot};
use super::follower::FollowerBus;
#[cfg(test)]
use super::gate::GateSettings;
use super::output_map::{DeviceControls, OutputDeviceSettings, MAX_FOLLOWERS};
use super::path::{LiveSlots, PathControls, PathMix, PathState, MAX_PATHS};
use super::plugins::PluginPref;
#[cfg(test)]
use super::plugins::PluginView;
use super::scope::ScopeLane;
use super::types::{CushionMode, Snapshot};

pub(crate) use super::scope::SCOPE_LEN;

/// One follower slot as the PipeWire linker / generic supervisor (and the UI)
/// sees it.  Updated live; the audio callback only reads
/// [`Shared::followers`] controls and the ring-active flags.
#[derive(Clone, Debug, Default)]
pub(crate) struct FollowerLink {
    /// cpal / inventory device id for generic open; empty when virtual.
    pub(crate) device_id: String,
    /// PipeWire `node.name` to link to, or empty when missing / virtual.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) node_name: String,
    pub(crate) virtual_out: bool,
    /// Virtual source advertised; ignored for hardware followers.
    pub(crate) publish: bool,
}

/// The live follower list the duplex/native main loop links from.
#[derive(Clone, Debug, Default)]
pub(crate) struct FollowerPlan {
    pub(crate) slots: [FollowerLink; MAX_FOLLOWERS],
    pub(crate) count: usize,
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
    pub(crate) input_enabled: [AtomicBool; 2],
    /// The clock device's channel map, level and mute (applied after the
    /// master) and its per-channel meters.
    pub(crate) clock_device: DeviceControls,
    /// Per-follower map, level, mute and meters.  Only the first
    /// [`Self::follower_count`] slots are live.
    pub(crate) followers: [DeviceControls; MAX_FOLLOWERS],
    /// How many follower slots the mixer and PipeWire linker use right now.
    follower_count: AtomicUsize,
    /// Sink names / publish flags the PipeWire main loop links from.  Not
    /// touched by audio callbacks.
    follower_plan: Mutex<FollowerPlan>,
    /// Per-slot flags the realtime callback can read without the plan lock.
    follower_virtual: [AtomicBool; MAX_FOLLOWERS],
    follower_publish: [AtomicBool; MAX_FOLLOWERS],
    /// Generic follower rings (finished mix → follower device).  Unused on
    /// the PipeWire duplex path, which fans out in-graph instead.
    follower_buses: [FollowerBus; MAX_FOLLOWERS],
    /// Set by the follower supervisor while a cpal stream is draining the
    /// slot's ring.  The clock callback only pushes when this is true.
    follower_ring_active: [AtomicBool; MAX_FOLLOWERS],
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
    /// The signal path slots.  Only live slots (see [`PathState`]) run;
    /// all are preallocated so adding one never allocates.  Slot 0 is the
    /// one path every session has today.
    pub(crate) paths: [PathControls; MAX_PATHS],
}

impl Shared {
    pub(crate) fn new(sample_rate: u32) -> Self {
        let capacity_frames = (sample_rate as usize * 2).max(4096);
        let shared = Self {
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
            input_enabled: [AtomicBool::new(true), AtomicBool::new(true)],
            clock_device: DeviceControls::new(),
            followers: std::array::from_fn(|_| DeviceControls::new()),
            follower_count: AtomicUsize::new(0),
            follower_plan: Mutex::new(FollowerPlan::default()),
            follower_virtual: std::array::from_fn(|_| AtomicBool::new(false)),
            follower_publish: std::array::from_fn(|_| AtomicBool::new(false)),
            follower_buses: std::array::from_fn(|_| FollowerBus::new(sample_rate)),
            follower_ring_active: std::array::from_fn(|_| AtomicBool::new(false)),
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
            paths: std::array::from_fn(|_| PathControls::new(sample_rate)),
        };
        shared.paths[0].set_state(PathState::Active);
        shared
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
        self.mute
            .store(previous.mute.load(Ordering::Relaxed), Ordering::Relaxed);
        self.monitor
            .store(previous.monitor.load(Ordering::Relaxed), Ordering::Relaxed);
        for (to, from) in self.input_enabled.iter().zip(&previous.input_enabled) {
            to.store(from.load(Ordering::Relaxed), Ordering::Relaxed);
        }
        self.drift_enabled.store(
            previous.drift_enabled.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        self.clock_device
            .adopt_settings_from(&previous.clock_device);
        let count = previous.follower_count.load(Ordering::Relaxed);
        self.follower_count.store(count, Ordering::Relaxed);
        for (to, from) in self.followers.iter().zip(previous.followers.iter()) {
            to.adopt_settings_from(from);
        }
        for index in 0..MAX_FOLLOWERS {
            let active = index < count;
            self.follower_virtual[index].store(
                active && previous.follower_virtual[index].load(Ordering::Relaxed),
                Ordering::Relaxed,
            );
            self.follower_publish[index].store(
                active && previous.follower_publish[index].load(Ordering::Relaxed),
                Ordering::Relaxed,
            );
        }
        if let (Ok(mut to), Ok(from)) = (self.follower_plan.lock(), previous.follower_plan.lock()) {
            *to = from.clone();
        }
        self.cushion_mode.store(
            previous.cushion_mode.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        self.manual_cushion_ms.store(
            previous.manual_cushion_ms.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        for (to, from) in self.paths.iter().zip(&previous.paths) {
            to.adopt_settings_from(from);
        }
    }

    /// The slots the mixer runs right now, in slot order.
    pub(crate) fn live_slots(&self) -> LiveSlots {
        LiveSlots::from_states(self.paths.iter().map(PathControls::state))
    }

    /// The live paths' controls, in slot order.
    pub(crate) fn live_paths(&self) -> impl Iterator<Item = &PathControls> {
        self.paths.iter().filter(|path| path.state().is_live())
    }

    /// Path 0, which the pre-path test accessors below act on.
    #[cfg(test)]
    fn main_path(&self) -> &PathControls {
        &self.paths[0]
    }

    /// Path `slot`'s controls.  `slot` must be below [`MAX_PATHS`]; the
    /// app only hands out slots in range.
    pub(crate) fn path(&self, slot: usize) -> &PathControls {
        debug_assert!(slot < MAX_PATHS, "path slot {slot} out of range");
        &self.paths[slot]
    }

    /// Whether some active path (not one fading out) is soloed.
    pub(crate) fn any_solo(&self) -> bool {
        self.paths
            .iter()
            .any(|path| path.state() == PathState::Active && path.soloed())
    }

    /// How many paths are active (removed paths still fading out do not
    /// count).
    pub(crate) fn active_path_count(&self) -> usize {
        self.paths
            .iter()
            .filter(|path| path.state() == PathState::Active)
            .count()
    }

    /// The first inactive slot: its fade-out (if any) is over.  Its old
    /// plugins may still wait for `service_paths` to release them;
    /// `restore_path` replaces them either way.
    fn free_slot(&self) -> Option<usize> {
        self.paths
            .iter()
            .position(|path| path.state() == PathState::Inactive)
    }

    /// Puts a path into `slot` with these settings and plugins and makes it
    /// active.  A slot that was inactive starts its chain fresh; an active
    /// one keeps playing and moves to the new settings with the usual
    /// ramps.  The plugins are restored, not opened: an open bridge opens
    /// them on the next `service_plugins`.  Call it from the UI thread.
    pub(crate) fn restore_path(&self, slot: usize, mix: &PathMix, plugins: Vec<PluginPref>) {
        let path = self.path(slot);
        // Set up before the mixer can see the slot.
        path.clear_removal();
        path.reset_meters();
        path.set_mix(mix);
        path.restore_plugins(plugins);
        path.set_state(PathState::Active);
    }

    /// Adds an empty path (default routing and EQs, no plugins, muted) in
    /// the first free slot, live.  `None` when every slot is taken.
    pub(crate) fn add_path(&self) -> Option<usize> {
        let slot = self.free_slot()?;
        let mix = PathMix {
            mute: true,
            ..PathMix::default()
        };
        self.restore_path(slot, &mix, Vec::new());
        Some(slot)
    }

    /// Adds a copy of path `source` in the first free slot: routing, mono,
    /// enable, EQs, gate, level and the plugin chain with each plugin's
    /// current state.  The copy starts muted and not soloed.  The device
    /// maps are the caller's (the app copies them).  Call it from the UI
    /// thread; the copied plugins open there too, on the next
    /// `service_plugins` while a bridge runs.
    pub(crate) fn duplicate_path(&self, source: usize) -> Option<usize> {
        let from = self.paths.get(source)?;
        if from.state() != PathState::Active {
            return None;
        }
        let slot = self.free_slot()?;
        let mix = PathMix {
            mute: true,
            solo: false,
            ..from.mix()
        };
        // Captures fresh plugin state first.
        let plugins = from.plugin_prefs();
        self.restore_path(slot, &mix, plugins);
        Some(slot)
    }

    /// Removes path `slot`: it fades out and then goes inactive.  Refused
    /// (`false`) for the last active path, or a slot that is not active.
    pub(crate) fn remove_path(&self, slot: usize) -> bool {
        let Some(path) = self.paths.get(slot) else {
            return false;
        };
        if path.state() != PathState::Active || self.active_path_count() <= 1 {
            return false;
        }
        path.begin_removal();
        true
    }

    /// UI-thread upkeep for removed paths: finishes a fade no mixer runs
    /// (no bridge, or one that stalled for `REMOVAL_TIMEOUT`), and releases
    /// the plugins of paths whose fade is over.
    pub(crate) fn service_paths(&self) {
        const REMOVAL_TIMEOUT: Duration = Duration::from_millis(500);
        let running = self.running.load(Ordering::Relaxed);
        for path in &self.paths {
            if path.state() == PathState::FadingOut {
                let overdue = path
                    .removed_at()
                    .is_none_or(|at| at.elapsed() >= REMOVAL_TIMEOUT);
                if !running || overdue {
                    path.finish_fade();
                }
            }
            path.release_if_retired();
        }
    }

    /// Applies new EQ settings to path 0; unchanged settings cost nothing.
    #[cfg(test)]
    pub(crate) fn set_eq(&self, slot: EqSlot, settings: EqSettings) {
        self.main_path().set_eq(slot, settings);
    }

    #[cfg(test)]
    pub(crate) fn set_gate(&self, settings: GateSettings) {
        self.main_path().set_gate(settings);
    }

    /// Opens any unloaded slots (and reloads them when the rate changed)
    /// before playback starts.  Call it from the UI thread.  The bridge
    /// does not load plugins.
    pub(crate) fn prepare_plugins(&self, sample_rate: u32) {
        for path in self.live_paths() {
            path.reload_plugins_for_rate(sample_rate);
        }
    }

    /// Reloads each chain when the opened device rate differs from the rate
    /// its plugins were built at, and opens slots that were only restored.
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
        for path in self.live_paths() {
            // The common case on every UI frame: the chain already runs at
            // the device rate and every slot was opened.  Answered from the
            // published flags instead of taking the chain lock.
            if path.needs_reload(rate) {
                path.reload_plugins_for_rate(rate);
            }
        }
    }

    /// Runs the CLAP main-thread callbacks and timers of every live path
    /// once per UI frame, then follows the device rate and drains plugins
    /// a bridge thread deferred.
    pub(crate) fn service_plugins(&self) {
        for path in self.live_paths() {
            path.service_plugins();
        }
        // The device may have opened at a different rate than `prepare_plugins`
        // used.  Both the reload and any plugins a bridge thread deferred are
        // handled on this thread, which is the UI thread.
        self.align_plugins_to_output_rate();
        super::plugins::drain_deferred_plugins();
    }

    pub(crate) fn reset(&self) {
        self.in_l.store(0.0f32.to_bits(), Ordering::Relaxed);
        self.in_r.store(0.0f32.to_bits(), Ordering::Relaxed);
        self.out_l.store(0.0f32.to_bits(), Ordering::Relaxed);
        self.out_r.store(0.0f32.to_bits(), Ordering::Relaxed);
        self.clock_device.reset();
        for follower in &self.followers {
            follower.reset();
        }
        for bus in &self.follower_buses {
            bus.reset_playback();
        }
        for flag in &self.follower_ring_active {
            flag.store(false, Ordering::Relaxed);
        }
        for path in &self.paths {
            path.reset_meters();
        }
        // Keep the plan and count across a session reset: the UI re-pushes
        // them every frame, and clearing here would drop links for a tick.
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
        // Size the cushion from the device step as soon as it is known.  Exclusive
        // (and large REAL-style) wakes drain a full step each time; waiting for the
        // mixer's second block to call note_output_block leaves the target at the
        // input-side initial guess and underruns into the fade ("helicopter").
        if step_frames > 0 {
            let source = match self.source_rate.load(Ordering::Relaxed) {
                0 => rate.max(1),
                source => source,
            };
            let out = rate.max(1);
            let source_step = (step_frames as u64 * source as u64)
                .div_ceil(out as u64)
                .max(1) as usize;
            self.note_output_block(source_step);
        }
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

    /// The master and bridge-wide switches: master mute, monitor, input
    /// channel enable and drift steering.  Per-path settings go through
    /// [`PathControls::set_mix`].
    pub(crate) fn set_master(
        &self,
        mute: bool,
        monitor: bool,
        input_channels: [bool; 2],
        drift: bool,
    ) {
        self.mute.store(mute, Ordering::Relaxed);
        self.monitor.store(monitor, Ordering::Relaxed);
        self.input_enabled[0].store(input_channels[0], Ordering::Relaxed);
        self.input_enabled[1].store(input_channels[1], Ordering::Relaxed);
        self.drift_enabled.store(drift, Ordering::Relaxed);
    }

    /// How many follower slots are live (≤ [`MAX_FOLLOWERS`]).
    pub(crate) fn follower_count(&self) -> usize {
        self.follower_count
            .load(Ordering::Relaxed)
            .min(MAX_FOLLOWERS)
    }

    /// Pushes follower settings, cushions and the link / open plan.  Level,
    /// mute and map take effect on the next block; add/remove updates
    /// PipeWire links or generic streams on the next main-loop tick.  Not
    /// part of the bridge signature.
    pub(crate) fn set_followers(
        &self,
        settings: &[OutputDeviceSettings],
        plan: &FollowerPlan,
        cushions: &[(CushionMode, f32)],
    ) {
        let count = settings.len().min(plan.count).min(MAX_FOLLOWERS);
        for (index, slot) in settings.iter().take(count).enumerate() {
            self.followers[index].set(slot);
            // PipeWire followers are stereo port pairs; publish the count so
            // the UI has meters as soon as the follower is added live.
            // Generic followers overwrite this when their stream opens.
            if self.followers[index].channels() == 0 {
                self.followers[index].set_channels(2);
            }
            let link = &plan.slots[index];
            self.follower_virtual[index].store(link.virtual_out, Ordering::Relaxed);
            self.follower_publish[index].store(link.publish, Ordering::Relaxed);
            if let Some(&(mode, ms)) = cushions.get(index) {
                self.follower_buses[index].set_cushion(mode, ms);
            }
        }
        for index in count..MAX_FOLLOWERS {
            self.follower_virtual[index].store(false, Ordering::Relaxed);
            self.follower_publish[index].store(false, Ordering::Relaxed);
            self.follower_ring_active[index].store(false, Ordering::Relaxed);
        }
        self.follower_count.store(count, Ordering::Relaxed);
        if let Ok(mut current) = self.follower_plan.lock() {
            *current = FollowerPlan {
                slots: plan.slots.clone(),
                count,
            };
        }
    }

    /// The playback bus for one generic follower slot.
    pub(crate) fn follower_bus(&self, index: usize) -> &FollowerBus {
        &self.follower_buses[index.min(MAX_FOLLOWERS - 1)]
    }

    /// Whether the clock callback should push finished frames into this
    /// follower's ring.
    pub(crate) fn follower_ring_active(&self, index: usize) -> &AtomicBool {
        &self.follower_ring_active[index.min(MAX_FOLLOWERS - 1)]
    }

    /// Pushes one finished stereo frame into every active generic follower
    /// ring.  No-op on the PipeWire duplex path (`direct`).
    pub(crate) fn push_follower_frames(&self, left: f32, right: f32) {
        if self.direct.load(Ordering::Relaxed) {
            return;
        }
        let count = self.follower_count();
        for index in 0..count {
            if self.follower_ring_active[index].load(Ordering::Acquire) {
                self.follower_buses[index].push_frame(left, right);
            }
        }
    }

    /// Notes the clock output's block size on every active follower bus so
    /// their cushions size themselves from the producer step.
    pub(crate) fn note_follower_producer_block(&self, frames: usize) {
        if self.direct.load(Ordering::Relaxed) {
            return;
        }
        let count = self.follower_count();
        for index in 0..count {
            if self.follower_ring_active[index].load(Ordering::Relaxed) {
                self.follower_buses[index].note_input_block(frames);
            }
        }
    }

    /// Whether follower `index` is the virtual RigWerk Out source.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn follower_is_virtual(&self, index: usize) -> bool {
        index < MAX_FOLLOWERS && self.follower_virtual[index].load(Ordering::Relaxed)
    }

    /// Whether the virtual source at `index` should be advertised.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn follower_is_published(&self, index: usize) -> bool {
        index < MAX_FOLLOWERS && self.follower_publish[index].load(Ordering::Relaxed)
    }

    /// A clone of the live follower plan for the PipeWire linker.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn follower_plan(&self) -> FollowerPlan {
        self.follower_plan
            .lock()
            .map(|plan| plan.clone())
            .unwrap_or_default()
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
            follower_count: self
                .follower_count
                .load(Ordering::Relaxed)
                .min(MAX_FOLLOWERS),
            follower_channels: std::array::from_fn(|index| self.followers[index].meters()),
            follower_channel_counts: std::array::from_fn(|index| self.followers[index].channels()),
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

/// Path 0 accessors from before paths existed, for tests.
#[cfg(test)]
impl Shared {
    /// Replaces path 0's saved insert list without loading anything.
    pub(crate) fn restore_plugins(&self, prefs: Vec<PluginPref>) {
        self.main_path().restore_plugins(prefs);
    }

    // `push_slot` hands a mismatched slot back so it can be dropped unlocked.
    #[allow(clippy::result_large_err)]
    pub(crate) fn add_plugin(&self, pref: PluginPref) -> Result<(), String> {
        self.main_path().add_plugin(pref)
    }

    pub(crate) fn remove_plugin(&self, index: usize) {
        self.main_path().remove_plugin(index);
    }

    pub(crate) fn move_plugin(&self, index: usize, delta: isize) {
        self.main_path().move_plugin(index, delta);
    }

    pub(crate) fn set_plugin_bypass(&self, index: usize, bypass: bool) {
        self.main_path().set_plugin_bypass(index, bypass);
    }

    pub(crate) fn set_plugin_mix(&self, index: usize, mix: f32) {
        self.main_path().set_plugin_mix(index, mix);
    }

    pub(crate) fn ensure_plugin_loaded(&self, index: usize) {
        self.main_path().ensure_plugin_loaded(index);
    }

    /// The loaded plugin behind one insert, for its own editor window.
    pub(crate) fn plugin_handle(&self, index: usize) -> Option<super::plugins::PluginHandle> {
        self.main_path().plugin_handle(index)
    }

    pub(crate) fn set_plugin_parameter(
        &self,
        index: usize,
        id: u32,
        value: f64,
    ) -> Result<(), String> {
        self.main_path().set_plugin_parameter(index, id, value)
    }

    /// Path 0's insert views as last published.  Never takes the chain
    /// lock, so drawing the UI cannot make the audio callback pass a block
    /// dry.
    pub(crate) fn plugin_views(&self) -> Arc<[PluginView]> {
        self.main_path().plugin_views()
    }

    /// Bumped on every publish of [`Self::plugin_views`].
    pub(crate) fn plugin_views_version(&self) -> u64 {
        self.main_path().plugin_views_version()
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
        self.set_master(mute, monitor, input_channels, drift);
        self.main_path().set_mono(mono);
        self.main_path().set_output_enabled(output_channels);
    }

    /// Sets path 0's input routing.
    pub(crate) fn set_routes(&self, routes: [[bool; 4]; 2]) {
        self.main_path().set_routes(routes);
    }
}

/// Starting ring target for a given device buffer.  Two blocks is enough to
/// start with; an underrun grows it and a calm stretch shrinks it again.
pub(crate) fn initial_target(buffer_frames: usize) -> usize {
    (buffer_frames * 2).clamp(96, 4096)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::atomic::Ordering;

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
    fn set_output_lifts_the_safe_cushion_for_a_large_exclusive_step() {
        // Bridge opens output before input has filled anything; exclusive may
        // wake with a 10 ms (or larger) GetBufferSize.  The cushion must already
        // cover two of those steps or the first audible block underruns.
        let shared = super::Shared::new(48_000);
        shared.set_cushion(CushionMode::Safe, 10.0);
        shared.set_source_rate(48_000);
        shared.set_target_frames(super::initial_target(128));
        shared.note_input_block(128);
        assert_eq!(shared.target_frames(), 256);
        shared.set_output(48_000, 480, "Exclusive · test");
        assert_eq!(shared.target_frames(), 960);
        assert_eq!(shared.output_step_frames.load(Ordering::Relaxed), 480);
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
    pub(crate) fn claim_main_thread() -> MainClaim {
        static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let serial = SERIAL.lock().unwrap_or_else(|poison| poison.into_inner());
        MainClaim {
            _thread: crate::audio::clap::claim_main_thread_for_test(),
            _serial: serial,
        }
    }

    pub(crate) struct MainClaim {
        // Dropped first, while `serial` is still held.
        _thread: crate::audio::clap::MainThreadClaim,
        _serial: std::sync::MutexGuard<'static, ()>,
    }

    fn chain_rate(shared: &super::Shared) -> u32 {
        shared.paths[0]
            .plugins
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .rate()
    }

    fn built_rates(shared: &super::Shared) -> Vec<Option<u32>> {
        shared.paths[0]
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
            shared.paths[0]
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
        shared.paths[0]
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
        shared.paths[0].edit_chain(|chain| {
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
        let chain = shared.paths[0].plugins.lock().unwrap();
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
    fn a_plugin_latency_change_republishes_the_views() {
        use crate::audio::clap::tests::{
            fake_plugin_with_latency_for_test, restart_fake_with_latency_for_test,
        };
        let _main = claim_main_thread();
        let shared = super::Shared::new(48_000);
        shared.paths[0]
            .edit_chain(|chain| chain.insert_clap_for_test(fake_plugin_with_latency_for_test(64)));
        assert_eq!(shared.plugin_views()[0].latency, 64);
        assert_eq!(shared.paths[0].latency_samples(), 64);
        // A quiet frame publishes nothing.
        let version = shared.paths[0].plugin_views_version();
        shared.service_plugins();
        assert_eq!(shared.paths[0].plugin_views_version(), version);

        // The plugin restarts with a new latency (it has not processed, so
        // the main thread restarts it right away).
        restart_fake_with_latency_for_test(8);
        shared.service_plugins();
        assert_eq!(shared.plugin_views()[0].latency, 8);
        assert_eq!(shared.paths[0].latency_samples(), 8);
    }

    #[test]
    fn a_panicking_edit_clears_what_it_published() {
        let _main = claim_main_thread();
        let shared = super::Shared::new(48_000);
        shared.paths[0].edit_chain(|chain| {
            chain.insert_clap_for_test(crate::audio::clap::tests::fake_plugin_for_test(false))
        });
        assert_eq!(shared.plugin_views().len(), 1);
        assert_eq!(shared.paths[0].plugin_services.lock().unwrap().len(), 1);
        assert!(shared.paths[0].has_plugins());

        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            shared.paths[0].edit_chain(|chain| {
                chain.set_bypass(0, true);
                panic!("an edit failed halfway");
            })
        }));
        assert!(panicked.is_err());
        assert!(shared.paths[0].plugins.is_poisoned());
        assert!(shared.plugin_views().is_empty());
        assert!(shared.paths[0].plugin_services.lock().unwrap().is_empty());
        assert!(!shared.paths[0].has_plugins());
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
        shared.paths[0].edit_chain(|chain| {
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
        shared.paths[0].edit_chain(|chain| {
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
        shared.paths[0].edit_chain(|chain| {
            chain.insert_clap_for_test(crate::audio::clap::tests::fake_plugin_for_test(true))
        });
        assert!(shared.paths[0].has_plugins());
        assert!(shared.plugin_views()[0].error.is_none());

        let mut left = vec![0.5f32; 64];
        let mut right = vec![0.5f32; 64];
        let version = shared.plugin_views_version();
        shared.paths[0].process_plugins(&mut left, &mut right, 64);
        assert!(
            !shared.paths[0].has_plugins(),
            "the failed insert left the chain"
        );
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
        shared.paths[0].edit_chain(|chain| {
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
        assert!(!shared.paths[0].has_plugins());
    }

    #[test]
    fn followers_are_pushed_live_and_cleared() {
        use super::{FollowerLink, FollowerPlan};
        use crate::audio::output_map::OutputDeviceSettings;

        let shared = super::Shared::new(48_000);
        let mut plan = FollowerPlan {
            count: 2,
            ..FollowerPlan::default()
        };
        plan.slots[0] = FollowerLink {
            device_id: "ALSA:hw:CARD=1,DEV=0".to_owned(),
            node_name: "speakers".to_owned(),
            virtual_out: false,
            publish: false,
        };
        plan.slots[1] = FollowerLink {
            device_id: String::new(),
            node_name: String::new(),
            virtual_out: true,
            publish: true,
        };
        shared.set_followers(
            &[
                OutputDeviceSettings {
                    level_db: -6.0,
                    mute: true,
                    ..OutputDeviceSettings::default()
                },
                OutputDeviceSettings::default(),
            ],
            &plan,
            &[(CushionMode::LowLatency, 10.0), (CushionMode::Safe, 10.0)],
        );
        assert_eq!(shared.follower_count(), 2);
        assert_eq!(shared.snapshot().follower_count, 2);
        let live = shared.follower_plan();
        assert_eq!(live.slots[0].node_name, "speakers");
        assert_eq!(live.slots[0].device_id, "ALSA:hw:CARD=1,DEV=0");
        assert!(live.slots[1].virtual_out && live.slots[1].publish);
        assert_eq!(
            shared.follower_bus(0).cushion_mode(),
            CushionMode::LowLatency
        );

        shared.set_followers(&[], &FollowerPlan::default(), &[]);
        assert_eq!(shared.follower_count(), 0);
        assert!(shared.follower_plan().slots[0].node_name.is_empty());
    }
}
