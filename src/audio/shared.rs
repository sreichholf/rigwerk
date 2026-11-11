//! State shared between the input callback, the output callback and the
//! UI: the ring buffer, the cushion (its fill target), meters, the master
//! and device settings, the signal paths, the tuner tap and what each side
//! really runs at.
//! An armed round-trip measurement lives here too ([`super::calibration`]):
//! its capture buffer and click are allocated with the session, so the
//! callbacks only touch atomics.
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
use super::output_map::{DeviceControls, DeviceFrame, OutputDeviceSettings, MAX_FOLLOWERS};
use super::path::{DspCell, DspTime, LiveSlots, PathControls, PathMix, PathState, MAX_PATHS};
use super::plugins::PluginPref;
#[cfg(test)]
use super::plugins::PluginView;
use super::scope::ScopeLane;
use super::types::{CushionMode, Snapshot};

use super::calibration::Calibration;

pub(crate) use super::scope::SCOPE_LEN;

/// Native frames the tuner tap keeps: 32 768 frames is 0.68 s at 48 kHz, twice
/// the largest window the analysis asks for.  The reader always copies the
/// newest window, so the extra turn only buys slack before the producer
/// overwrites what is being read.
pub(crate) const TAP_CAPACITY_FRAMES: usize = 32_768;

const _: () = assert!(crate::audio::tuner::TAP_COPY_FRAMES <= TAP_CAPACITY_FRAMES);

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
    /// Interleaved stereo samples for the tuner, as `f32` bit patterns.  This
    /// is an overwrite ring rather than a queue: the input callback writes
    /// every frame it pushes into `ring` here too, one reader (the UI) copies
    /// the latest window, and an unread frame is simply overwritten.  A torn
    /// window at the wrap seam is acceptable because pitch analysis does not
    /// need sample continuity across the whole window.
    tap: Vec<AtomicU32>,
    /// Total frames written to the tap.  Slots are `counter % tap_capacity`.
    tap_head: AtomicU64,
    tap_capacity: usize,
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
    /// The DSP time of every live path's chain together, per block: the
    /// smoothed sum and the decaying peak of each block's sum (not the sum
    /// of the paths' peaks, which come from different blocks).  Written by
    /// the mixer at the end of each block, read by the Live overload issue.
    pub(crate) total_dsp: DspCell,
    /// The armed round-trip measurement, or idle.  The capture buffer and the
    /// click are preallocated here so no callback ever allocates for one.
    pub(crate) calibration: Calibration,
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
            tap: (0..TAP_CAPACITY_FRAMES * 2)
                .map(|_| AtomicU32::new(0))
                .collect(),
            tap_head: AtomicU64::new(0),
            tap_capacity: TAP_CAPACITY_FRAMES,
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
            total_dsp: DspCell::default(),
            calibration: Calibration::new(),
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
        // The fade itself, plus the longest alignment delay the path may
        // still have to play out (`MAX_ALIGN_DELAY` frames, under 0.9 s at
        // 44.1 kHz), with room for a stalled block or two.
        const REMOVAL_TIMEOUT: Duration = Duration::from_millis(1_500);
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
        self.total_dsp.store(DspTime::default());
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
        // Defensive: a fresh session has written nothing, so the tap starts
        // empty and `copy_tap` reports nothing until it has a whole window.
        self.tap_head.store(0, Ordering::Relaxed);
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
        // A measurement belongs to the session it was armed in; a restarted
        // bridge must not inherit a duck or a half-filled capture.
        self.calibration.finish();
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

    /// Whether the clock callback maps generic follower `index` for its
    /// ring: a follower stream drains it, and this is not the PipeWire
    /// duplex graph (which plays the followers in-graph).  Read once per
    /// block; the ring is laid out for the device before this turns true.
    pub(crate) fn feeds_follower(&self, index: usize) -> bool {
        !self.direct.load(Ordering::Relaxed)
            && self.follower_ring_active(index).load(Ordering::Acquire)
    }

    /// Pushes follower `index`'s finished device frame (its own sum of the
    /// paths, aligned, mapped and levelled) into its ring, while a stream
    /// still drains it and the ring is still the one laid out at the start
    /// of the block (`generation`, see [`FollowerBus::generation`]).
    pub(crate) fn push_follower_frame(&self, index: usize, generation: u64, frame: &DeviceFrame) {
        if self.follower_ring_active(index).load(Ordering::Relaxed)
            && self.follower_bus(index).generation() == generation
        {
            self.follower_bus(index).push_frame(frame);
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

    /// One raw input frame for an armed round-trip measurement, before the
    /// input gain and channel enable.  Called by every capture callback;
    /// one relaxed load when no measurement is armed.
    pub(crate) fn capture_calibration(&self, left: f32, right: f32) {
        self.calibration.capture_frame(left, right);
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

    /// Called by the one input producer, once per frame, next to `push_frame`
    /// and before the channel mute zeroes a disabled side: the tuner sees the
    /// instrument even when that channel is not being monitored.  Never
    /// blocks, allocates or takes a lock; the release store of `tap_head`
    /// publishes both samples to the reader.
    pub(crate) fn tap_frame(&self, left: f32, right: f32) {
        let head = self.tap_head.load(Ordering::Relaxed);
        let index = (head % self.tap_capacity as u64) as usize * 2;
        self.tap[index].store(left.to_bits(), Ordering::Relaxed);
        self.tap[index + 1].store(right.to_bits(), Ordering::Relaxed);
        self.tap_head.store(head.wrapping_add(1), Ordering::Release);
    }

    /// Copies the latest `frames` interleaved frames (oldest first) into
    /// `out`, which must hold `frames * 2` samples.  Called from the UI
    /// thread.  Returns 0 while the tap has not yet seen a whole window, so a
    /// fresh session reads nothing instead of stale or partly unwritten data.
    pub(crate) fn copy_tap(&self, out: &mut [f32], frames: usize) -> usize {
        if frames == 0 || frames > self.tap_capacity || out.len() < frames * 2 {
            return 0;
        }
        let head = self.tap_head.load(Ordering::Acquire);
        if head < frames as u64 {
            return 0;
        }
        let start = head - frames as u64;
        let capacity = self.tap_capacity as u64;
        // The window can straddle the wrap point, so it is copied in at most
        // two runs rather than a modulo per frame.
        let first = (capacity - start % capacity).min(frames as u64) as usize;
        let mut base = (start % capacity) as usize * 2;
        for frame in 0..first {
            out[frame * 2] = f32::from_bits(self.tap[base].load(Ordering::Relaxed));
            out[frame * 2 + 1] = f32::from_bits(self.tap[base + 1].load(Ordering::Relaxed));
            base += 2;
        }
        let mut base = 0;
        for frame in first..frames {
            out[frame * 2] = f32::from_bits(self.tap[base].load(Ordering::Relaxed));
            out[frame * 2 + 1] = f32::from_bits(self.tap[base + 1].load(Ordering::Relaxed));
            base += 2;
        }
        frames
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
            out_reduction_db: self.clock_device.reduction_db(),
            output_channels: self.clock_device.channels(),
            follower_count: self
                .follower_count
                .load(Ordering::Relaxed)
                .min(MAX_FOLLOWERS),
            follower_channels: std::array::from_fn(|index| self.followers[index].meters()),
            follower_reduction_db: std::array::from_fn(|index| {
                self.followers[index].reduction_db()
            }),
            follower_channel_counts: std::array::from_fn(|index| self.followers[index].channels()),
            follower_missing: std::array::from_fn(|index| {
                self.follower_buses[index].missing.load(Ordering::Relaxed)
            }),
            follower_underruns: std::array::from_fn(|index| {
                self.follower_buses[index].underruns.load(Ordering::Relaxed)
            }),
            follower_drift_at_limit: std::array::from_fn(|index| {
                self.follower_buses[index]
                    .drift_at_limit
                    .load(Ordering::Relaxed)
            }),
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
pub(crate) mod tests;
