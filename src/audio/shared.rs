//! The ring and the run both callbacks share with the UI.
//!
//! A native PipeWire rate is a reading only once the device or graph
//! reported it.  A request never overwrites a reading.  A callback that
//! fails stores one code; [`Shared::snapshot`] words it.  Calibration,
//! recording and the ASIO panel are preallocated, so the callbacks only
//! touch atomics.

use std::sync::atomic::{
    AtomicBool, AtomicU16, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// No PipeWire path delay sampled yet.
const PATH_DELAY_UNKNOWN: u64 = u64::MAX;

/// One byte on [`Shared`].  The callback may not format a message, so
/// [`Shared::snapshot`] words it.  The first cause wins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallbackFailure {
    Panic,
    /// A partial frame, or no channels.
    BadBlock,
}

impl CallbackFailure {
    /// 0 is no failure.
    fn code(self) -> u8 {
        match self {
            Self::Panic => 1,
            Self::BadBlock => 2,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Panic),
            2 => Some(Self::BadBlock),
            _ => None,
        }
    }

    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::Panic => "A real-time audio callback panicked (most likely inside a plugin)",
            Self::BadBlock => {
                "A real-time audio callback received a malformed block (a partial frame or no channels)"
            }
        }
    }
}

use super::cab::CabPark;
#[cfg(test)]
use super::eq::{EqSettings, EqSlot};
use super::extra_output::ExtraOutputBus;
#[cfg(test)]
use super::gate::GateSettings;
use super::nam::{NamPark, NamStack};
use super::output_map::{DeviceControls, DeviceFrame, OutputDeviceSettings, MAX_EXTRA_OUTPUTS};
use super::path::{DspCell, DspTime, LiveSlots, PathControls, PathMix, PathState, MAX_PATHS};
#[cfg(test)]
use super::plugins::PluginView;
use super::plugins::{PluginPref, SlotPark, MAX_ALIGN};
use super::resampler::{FrameResampler, KernelHandoff};
use super::scope::ScopeLane;
use super::types::{BufferMode, OutputMode, Snapshot};

use super::asio_panel::AsioPanelRequests;
use super::calibration::Calibration;
use super::record::Recorder;

pub(crate) use super::scope::SCOPE_LEN;

/// Twice the largest analysis window.  The reader copies the newest
/// window, so the extra turn is slack before an overwrite.
pub(crate) const TAP_CAPACITY_FRAMES: usize = 32_768;

const _: () = assert!(crate::audio::tuner::TAP_COPY_FRAMES <= TAP_CAPACITY_FRAMES);

/// Updated live.  The audio callback reads the ring-active flags, not this.
#[derive(Clone, Debug, Default)]
pub(crate) struct ExtraOutputLink {
    /// Empty when virtual.
    pub(crate) device_id: String,
    /// PipeWire `node.name`, or empty when missing or virtual.
    pub(crate) node_name: String,
    pub(crate) virtual_output: bool,
    /// Ignored for a hardware extra output.
    pub(crate) virtual_output_on: bool,
}

/// The duplex and native main loops link from this.  Audio callbacks do not.
#[derive(Clone, Debug, Default)]
pub(crate) struct ExtraOutputPlan {
    pub(crate) slots: [ExtraOutputLink; MAX_EXTRA_OUTPUTS],
    pub(crate) count: usize,
}

pub(crate) struct Shared {
    /// The input side writes `head`; the output side writes `tail`.  They
    /// never share an index.
    ring: Vec<AtomicU32>,
    /// `head - tail` is the fill.  Slots are `counter % capacity_frames`.
    head: AtomicU64,
    tail: AtomicU64,
    pub(crate) capacity_frames: usize,
    /// An overwrite ring, not a queue.  A torn window at the wrap is
    /// acceptable: pitch analysis does not need continuity across it.
    tap: Vec<AtomicU32>,
    tap_head: AtomicU64,
    tap_capacity: usize,
    pub(crate) target_frames: AtomicU64,
    pub(crate) base_frames: AtomicU64,
    /// Both in input frames.
    pub(crate) input_block: AtomicU64,
    pub(crate) output_block: AtomicU64,
    pub(crate) bridge_buffer_mode: AtomicU32,
    pub(crate) fixed_bridge_buffer_ms: AtomicU32,
    pub(crate) in_l: AtomicU32,
    pub(crate) in_r: AtomicU32,
    pub(crate) out_l: AtomicU32,
    pub(crate) out_r: AtomicU32,
    scope_in: ScopeLane,
    scope_out: ScopeLane,
    pub(crate) dropouts: AtomicU64,
    pub(crate) overruns: AtomicU64,
    /// Unlike `head`, this advances when the ring is full, so a stalled
    /// capture is visible.
    pub(crate) input_frames: AtomicU64,
    pub(crate) frames: AtomicU64,
    pub(crate) drift_ppm: AtomicU32,
    pub(crate) running: AtomicBool,
    pub(crate) input_gain: AtomicU32,
    pub(crate) output_gain: AtomicU32,
    pub(crate) mute: AtomicBool,
    pub(crate) metronome_running: AtomicBool,
    /// `f32` bits.
    pub(crate) metronome_bpm: AtomicU32,
    /// An unknown value reads as 4/4.
    pub(crate) metronome_rhythm: AtomicU8,
    /// Linear gain.  0 dB is 1, so the accent peak is the safety ceiling.
    pub(crate) metronome_gain: AtomicU32,
    /// Packed with the rhythm of that beat ([`pack_metronome_beat`]), which
    /// can already name the next bar.  The snapshot reads 0 once an error
    /// or a lost device is set, and while [`Self::running`] is false: a
    /// lingering mixer can publish until teardown ends.
    metronome_beat: AtomicU16,
    pub(crate) input_enabled: [AtomicBool; 2],
    pub(crate) clock_output: DeviceControls,
    /// Only the first [`Self::extra_output_count`] slots are live.
    pub(crate) extra_outputs: [DeviceControls; MAX_EXTRA_OUTPUTS],
    extra_output_count: AtomicUsize,
    /// Not touched by audio callbacks.
    extra_output_plan: Mutex<ExtraOutputPlan>,
    /// The callback reads these without the plan lock.
    extra_output_virtual: [AtomicBool; MAX_EXTRA_OUTPUTS],
    virtual_output_on: [AtomicBool; MAX_EXTRA_OUTPUTS],
    /// Unused on the PipeWire duplex path, which fans out in the graph.
    extra_output_buses: [ExtraOutputBus; MAX_EXTRA_OUTPUTS],
    /// The clock callback pushes only while this is set.
    extra_output_ring_active: [AtomicBool; MAX_EXTRA_OUTPUTS],
    pub(crate) drift_enabled: AtomicBool,
    /// Duplex: one graph callback.  The ring, bridge buffer and drift
    /// steering are unused.
    pub(crate) direct: AtomicBool,
    pub(crate) status: Mutex<String>,
    pub(crate) error: Mutex<Option<String>>,
    pub(crate) device_lost: AtomicBool,
    callback_failure: AtomicU8,
    /// Outputs convert from this rate to their own.
    pub(crate) source_rate: AtomicU32,
    pub(crate) output_rate: AtomicU32,
    pub(crate) output_step_frames: AtomicU32,
    /// Planned off the audio thread.  A running mixer takes one with
    /// `try_lock`.
    pub(crate) clock_kernel: KernelHandoff,
    pub(crate) extra_kernels: [KernelHandoff; MAX_EXTRA_OUTPUTS],
    /// False while [`Self::source_rate`] is only the rate asked for.
    input_rate_reported: AtomicBool,
    /// False while [`Self::output_rate`] is only the rate asked for.
    output_rate_reported: AtomicBool,
    /// False while [`Self::output_step_frames`] is only the quantum asked for.
    output_step_reported: AtomicBool,
    pub(crate) output_mode: Mutex<String>,
    /// 0 until the output has opened.
    opened_output_mode: AtomicU8,
    /// [`PATH_DELAY_UNKNOWN`] until a native playback callback writes it.
    pub(crate) path_delay_ns: AtomicU64,
    /// Preallocated, so adding a path never allocates.
    pub(crate) paths: [PathControls; MAX_PATHS],
    /// Peaks from different blocks do not add.
    pub(crate) total_dsp: DspCell,
    /// Preallocated, so a callback never allocates for one.
    pub(crate) calibration: Calibration,
    /// The UI sets a side.  The bridge thread that owns the driver takes it
    /// between callbacks.
    pub(crate) asio_panel: AsioPanelRequests,
    /// Arming starts the writer, which keeps its own `Arc` so it can finish
    /// after this run is dropped.
    pub(crate) recorder: Arc<Recorder>,
    /// The engine keeps a clone, so a bridge-thread drop of this run does
    /// not destroy them.
    nam_park: Arc<NamPark>,
    /// Same lifetime as [`Self::nam_park`].
    cab_park: Arc<CabPark>,
    /// The one park every run of this engine shares, so a bridge-thread
    /// drop of this run does not destroy them.
    slot_park: Arc<SlotPark>,
}

impl Shared {
    pub(crate) fn new(sample_rate: u32) -> Self {
        Self::with_park(sample_rate, NamPark::new(), CabPark::new(), SlotPark::new())
    }

    pub(crate) fn with_park(
        sample_rate: u32,
        nam_park: Arc<NamPark>,
        cab_park: Arc<CabPark>,
        slot_park: Arc<SlotPark>,
    ) -> Self {
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
            bridge_buffer_mode: AtomicU32::new(BufferMode::Auto as u32),
            fixed_bridge_buffer_ms: AtomicU32::new(10.0f32.to_bits()),
            in_l: AtomicU32::new(0.0f32.to_bits()),
            in_r: AtomicU32::new(0.0f32.to_bits()),
            out_l: AtomicU32::new(0.0f32.to_bits()),
            out_r: AtomicU32::new(0.0f32.to_bits()),
            scope_in: ScopeLane::new(),
            scope_out: ScopeLane::new(),
            dropouts: AtomicU64::new(0),
            overruns: AtomicU64::new(0),
            input_frames: AtomicU64::new(0),
            frames: AtomicU64::new(0),
            drift_ppm: AtomicU32::new(0.0f32.to_bits()),
            running: AtomicBool::new(false),
            input_gain: AtomicU32::new(1.0f32.to_bits()),
            output_gain: AtomicU32::new(1.0f32.to_bits()),
            mute: AtomicBool::new(false),
            metronome_running: AtomicBool::new(false),
            metronome_bpm: AtomicU32::new(super::metronome::METRONOME_BPM_DEFAULT.to_bits()),
            metronome_rhythm: AtomicU8::new(super::metronome::ClickRhythm::FourFour as u8),
            metronome_gain: AtomicU32::new(
                super::click_level_gain(super::metronome::CLICK_LEVEL_DEFAULT_DB).to_bits(),
            ),
            metronome_beat: AtomicU16::new(pack_metronome_beat(
                0,
                super::metronome::ClickRhythm::FourFour as u8,
            )),
            input_enabled: [AtomicBool::new(true), AtomicBool::new(true)],
            clock_output: DeviceControls::new(),
            extra_outputs: std::array::from_fn(|_| DeviceControls::new()),
            extra_output_count: AtomicUsize::new(0),
            extra_output_plan: Mutex::new(ExtraOutputPlan::default()),
            extra_output_virtual: std::array::from_fn(|_| AtomicBool::new(false)),
            virtual_output_on: std::array::from_fn(|_| AtomicBool::new(false)),
            extra_output_buses: std::array::from_fn(|_| ExtraOutputBus::new(sample_rate)),
            extra_output_ring_active: std::array::from_fn(|_| AtomicBool::new(false)),
            drift_enabled: AtomicBool::new(true),
            direct: AtomicBool::new(false),
            status: Mutex::new("Waiting".to_owned()),
            error: Mutex::new(None),
            device_lost: AtomicBool::new(false),
            callback_failure: AtomicU8::new(0),
            source_rate: AtomicU32::new(0),
            output_rate: AtomicU32::new(0),
            output_step_frames: AtomicU32::new(0),
            clock_kernel: KernelHandoff::new(),
            extra_kernels: std::array::from_fn(|_| KernelHandoff::new()),
            input_rate_reported: AtomicBool::new(false),
            output_rate_reported: AtomicBool::new(false),
            output_step_reported: AtomicBool::new(false),
            output_mode: Mutex::new(String::new()),
            opened_output_mode: AtomicU8::new(0),
            path_delay_ns: AtomicU64::new(PATH_DELAY_UNKNOWN),
            paths: std::array::from_fn(|_| {
                PathControls::with_park(
                    sample_rate,
                    Arc::clone(&nam_park),
                    Arc::clone(&cab_park),
                    Arc::clone(&slot_park),
                )
            }),
            total_dsp: DspCell::default(),
            calibration: Calibration::new(),
            asio_panel: AsioPanelRequests::new(),
            recorder: Arc::new(Recorder::new()),
            nam_park,
            cab_park,
            slot_park,
        };
        shared.paths[0].set_state(PathState::Active);
        shared
    }

    pub(crate) fn nam_park(&self) -> &Arc<NamPark> {
        &self.nam_park
    }

    pub(crate) fn cab_park(&self) -> &Arc<CabPark> {
        &self.cab_park
    }

    /// Outlives the run: the engine holds the same park.
    pub(crate) fn plugin_park(&self) -> &Arc<SlotPark> {
        &self.slot_park
    }

    /// Live plugin instances stay with the run that opened them.  Only the
    /// saved preferences move.
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
        self.metronome_running.store(
            previous.metronome_running.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        self.metronome_bpm.store(
            previous.metronome_bpm.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        self.metronome_rhythm.store(
            previous.metronome_rhythm.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        self.metronome_gain.store(
            previous.metronome_gain.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        // The displayed beat belongs to the run that published it.
        // A new engine starts at "—", not on the previous run's beat.
        for (to, from) in self.input_enabled.iter().zip(&previous.input_enabled) {
            to.store(from.load(Ordering::Relaxed), Ordering::Relaxed);
        }
        self.drift_enabled.store(
            previous.drift_enabled.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        self.recorder.set_with_click(previous.recorder.with_click());
        self.clock_output
            .adopt_settings_from(&previous.clock_output);
        let count = previous.extra_output_count.load(Ordering::Relaxed);
        self.extra_output_count.store(count, Ordering::Relaxed);
        for (to, from) in self.extra_outputs.iter().zip(previous.extra_outputs.iter()) {
            to.adopt_settings_from(from);
        }
        for index in 0..MAX_EXTRA_OUTPUTS {
            let active = index < count;
            self.extra_output_virtual[index].store(
                active && previous.extra_output_virtual[index].load(Ordering::Relaxed),
                Ordering::Relaxed,
            );
            self.virtual_output_on[index].store(
                active && previous.virtual_output_on[index].load(Ordering::Relaxed),
                Ordering::Relaxed,
            );
        }
        if let (Ok(mut to), Ok(from)) = (
            self.extra_output_plan.lock(),
            previous.extra_output_plan.lock(),
        ) {
            *to = from.clone();
        }
        self.bridge_buffer_mode.store(
            previous.bridge_buffer_mode.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        self.fixed_bridge_buffer_ms.store(
            previous.fixed_bridge_buffer_ms.load(Ordering::Relaxed),
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

    /// A return also waits for its slowest live sender.  Before a block
    /// has run, the published chain latency stands in.
    pub(crate) fn alignment_latencies(&self) -> [u32; MAX_PATHS] {
        let live = |path: &PathControls| path.state().is_live();
        // Unclamped, so the report can flag a path past `MAX_ALIGN`.
        let own = |path: &PathControls| path.path_latency_samples().max(path.latency_samples());
        let wait = self
            .paths
            .iter()
            .filter(|path| live(path) && !path.is_return())
            .map(own)
            .map(|samples| samples.min(MAX_ALIGN))
            .max()
            .unwrap_or(0);
        std::array::from_fn(|slot| {
            let path = &self.paths[slot];
            if live(path) && path.is_return() {
                own(path).saturating_add(wait)
            } else {
                own(path)
            }
        })
    }

    /// A return is solo-safe.  A path fading out is not soloed.
    pub(crate) fn any_solo(&self) -> bool {
        self.paths
            .iter()
            .any(|path| path.state() == PathState::Active && path.soloed() && !path.is_return())
    }

    /// A path still fading out does not count.
    pub(crate) fn active_path_count(&self) -> usize {
        self.paths
            .iter()
            .filter(|path| path.state() == PathState::Active)
            .count()
    }

    /// Its plugins may still be waiting for `service_paths`.  `restore_path`
    /// replaces them either way.
    fn free_slot(&self) -> Option<usize> {
        self.paths
            .iter()
            .position(|path| path.state() == PathState::Inactive)
    }

    /// Plugins are restored, not opened.  UI thread.
    pub(crate) fn restore_path(
        &self,
        slot: usize,
        mix: &PathMix,
        nam: &NamStack,
        plugins: Vec<PluginPref>,
    ) {
        let path = self.path(slot);
        // Before the mixer can see the slot.
        path.clear_removal();
        path.reset_meters();
        path.set_mix(mix);
        path.set_nam(nam);
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
        self.restore_path(slot, &mix, &NamStack::default(), Vec::new());
        Some(slot)
    }

    /// The caller copies the device maps.  UI thread.
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
        let nam = from.nam();
        self.restore_path(slot, &mix, &nam, plugins);
        Some(slot)
    }

    /// `false` for the last active path, or a slot that is not active.
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

    /// Also finishes a fade no mixer is running, after [`REMOVAL_TIMEOUT`].
    pub(crate) fn service_paths(&self) {
        // The fade, plus the longest alignment delay still to play out.
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

    #[cfg(test)]
    pub(crate) fn set_eq(&self, slot: EqSlot, settings: EqSettings) {
        self.main_path().set_eq(slot, settings);
    }

    #[cfg(test)]
    pub(crate) fn set_gate(&self, settings: GateSettings) {
        self.main_path().set_gate(settings);
    }

    /// The bridge does not load plugins.  UI thread.
    pub(crate) fn prepare_plugins(&self, sample_rate: u32) {
        for path in self.live_paths() {
            path.reload_plugins_for_rate(sample_rate);
        }
    }

    /// Only the recorded main thread loads or destroys a plugin.
    pub(crate) fn align_plugins_to_output_rate(&self) {
        if !super::plugins::is_main_thread() {
            return;
        }
        let rate = self.output_rate.load(Ordering::Relaxed);
        if rate == 0 {
            return;
        }
        for path in self.live_paths() {
            // Answered from the published flags, without the model lock.
            if path.needs_reload(rate) {
                path.reload_plugins_for_rate(rate);
            }
        }
    }

    pub(crate) fn service_plugins(&self) {
        // One rebuild when a plugin reload and a new device rate land in
        // the same frame.
        let rate = self.output_rate.load(Ordering::Relaxed);
        let device_rate = (super::plugins::is_main_thread() && rate != 0).then_some(rate);
        for path in &self.paths {
            if path.state().is_live() {
                path.service_plugins_at(device_rate);
            } else {
                // The mixer parks the runtime.  This destroys what came back.
                path.flush_chain();
            }
        }
        // VST3 editor timers, whether or not an editor is open.
        super::vst3::service_run_loop();
        // The device may have opened at another rate than `prepare_plugins`
        // used.  The reload, and anything a bridge thread parked, happen
        // on this thread.
        self.align_plugins_to_output_rate();
        self.slot_park.drain();
    }

    pub(crate) fn reset(&self) {
        self.in_l.store(0.0f32.to_bits(), Ordering::Relaxed);
        self.in_r.store(0.0f32.to_bits(), Ordering::Relaxed);
        self.out_l.store(0.0f32.to_bits(), Ordering::Relaxed);
        self.out_r.store(0.0f32.to_bits(), Ordering::Relaxed);
        self.clock_output.reset();
        for extra_output in &self.extra_outputs {
            extra_output.reset();
        }
        for bus in &self.extra_output_buses {
            bus.reset_playback();
        }
        for flag in &self.extra_output_ring_active {
            flag.store(false, Ordering::Relaxed);
        }
        for path in &self.paths {
            path.reset_meters();
        }
        self.total_dsp.store(DspTime::default());
        // The UI re-pushes the plan every frame.  Clearing it here would
        // drop links for a tick.
        self.scope_in.clear();
        self.scope_out.clear();
        self.dropouts.store(0, Ordering::Relaxed);
        self.overruns.store(0, Ordering::Relaxed);
        self.input_frames.store(0, Ordering::Relaxed);
        self.frames.store(0, Ordering::Relaxed);
        self.drift_ppm.store(0.0f32.to_bits(), Ordering::Relaxed);
        self.running.store(false, Ordering::Relaxed);
        self.direct.store(false, Ordering::Relaxed);
        // A fresh run: no callback has touched these, so the store order
        // does not matter.
        self.head.store(0, Ordering::Relaxed);
        self.tail.store(0, Ordering::Relaxed);
        // A fresh run has written nothing, so the tap stays empty until a
        // whole window exists.
        self.tap_head.store(0, Ordering::Relaxed);
        self.set_status("Starting bridge");
        self.device_lost.store(false, Ordering::Relaxed);
        self.callback_failure.store(0, Ordering::Relaxed);
        if let Ok(mut error) = self.error.lock() {
            *error = None;
        }
        self.source_rate.store(0, Ordering::Relaxed);
        self.input_block.store(0, Ordering::Relaxed);
        self.output_block.store(0, Ordering::Relaxed);
        self.output_rate.store(0, Ordering::Relaxed);
        self.output_step_frames.store(0, Ordering::Relaxed);
        self.clock_kernel.clear();
        for handoff in &self.extra_kernels {
            handoff.clear();
        }
        self.input_rate_reported.store(false, Ordering::Relaxed);
        self.output_rate_reported.store(false, Ordering::Relaxed);
        self.output_step_reported.store(false, Ordering::Relaxed);
        self.path_delay_ns
            .store(PATH_DELAY_UNKNOWN, Ordering::Relaxed);
        self.set_output_mode("");
        self.opened_output_mode.store(0, Ordering::Relaxed);
        self.calibration.finish();
        // A fresh start must not open a panel on the driver's behalf.
        self.asio_panel.clear_requests();
    }

    #[cfg(target_os = "linux")]
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

    /// A reading.  A rate asked for before the stream opened goes through
    /// [`Self::set_requested_source_rate`].
    pub(crate) fn set_source_rate(&self, rate: u32) {
        self.store_source_rate(rate, true, true);
        // A fixed bridge buffer is milliseconds; re-measure it in frames.
        self.lift_to_floor();
    }

    /// Not a reading, and never replaces one.
    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn set_requested_source_rate(&self, rate: u32) {
        if self.source_rate_is_read() {
            return;
        }
        self.store_source_rate(rate, false, true);
        self.lift_to_floor();
    }

    /// Duplex passes `publish_kernel` false: it has no resampler.
    fn store_source_rate(&self, rate: u32, reported: bool, publish_kernel: bool) {
        self.source_rate.store(rate, Ordering::Relaxed);
        self.input_rate_reported.store(reported, Ordering::Relaxed);
        self.scope_in.set_rate(rate);
        if publish_kernel {
            self.post_clock_kernel();
        }
    }

    /// Off the audio thread, after the rate store.
    fn post_clock_kernel(&self) {
        let source = self.source_rate.load(Ordering::Relaxed);
        let output = self.output_rate.load(Ordering::Relaxed);
        if source != 0 && output != 0 {
            self.clock_kernel
                .post(FrameResampler::<(f32, f32)>::plan(source, output));
        }
    }

    #[cfg(any(target_os = "linux", test))]
    fn source_rate_is_read(&self) -> bool {
        self.input_rate_reported.load(Ordering::Relaxed)
    }

    /// Atomics only, so the audio thread may call it.  Does not lift the
    /// bridge buffer: the duplex node has no ring.
    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn note_direct_rate(&self, rate: u32) {
        self.store_source_rate(rate, true, false);
        self.store_output_rate(rate, true, false);
    }

    /// Not a reading, and it never undoes [`Self::note_direct_rate`] or
    /// [`Self::set_output`].
    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn note_requested_direct_rate(&self, rate: u32) {
        if !self.source_rate_is_read() {
            self.store_source_rate(rate, false, false);
        }
        if !self.output_rate_is_read() {
            self.store_output_rate(rate, false, false);
        }
    }

    /// A reading.  A rate asked for before the stream opens goes through
    /// [`Self::set_requested_output`].
    pub(crate) fn set_output(
        &self,
        rate: u32,
        step_frames: u32,
        opened: OutputMode,
        mode: impl Into<String>,
    ) {
        self.store_output(rate, step_frames, opened, mode, true);
    }

    /// Not a reading, and it never overwrites one.
    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn set_requested_output(
        &self,
        rate: u32,
        step_frames: u32,
        opened: OutputMode,
        mode: impl Into<String>,
    ) {
        if self.output_rate_is_read() || self.output_step_is_read() {
            return;
        }
        self.store_output(rate, step_frames, opened, mode, false);
    }

    /// The step and the mode line stay as published.
    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn set_output_rate(&self, rate: u32) {
        self.store_output_rate(rate, true, true);
    }

    fn store_output(
        &self,
        rate: u32,
        step_frames: u32,
        opened: OutputMode,
        mode: impl Into<String>,
        reported: bool,
    ) {
        self.store_output_rate(rate, reported, true);
        self.output_step_frames
            .store(step_frames, Ordering::Relaxed);
        // A step of 0 is not a reading: the callback publishes the real one.
        self.output_step_reported
            .store(reported && step_frames > 0, Ordering::Relaxed);
        self.set_output_mode(mode);
        self.opened_output_mode
            .store(opened.to_u8(), Ordering::Relaxed);
        // Size the bridge buffer from the device step now.  Waiting for the
        // mixer's second block leaves the target at the input-side guess
        // and the first exclusive wake drops out.
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

    fn store_output_rate(&self, rate: u32, reported: bool, publish_kernel: bool) {
        self.output_rate.store(rate, Ordering::Relaxed);
        self.output_rate_reported.store(reported, Ordering::Relaxed);
        self.scope_out.set_rate(rate);
        if !publish_kernel {
            return;
        }
        self.post_clock_kernel();
        // An extra output's producer rate is this clock rate and its device
        // rate was fixed when its stream opened.  Only one that has published
        // a rate has a pair to plan for.
        for (handoff, bus) in self.extra_kernels.iter().zip(&self.extra_output_buses) {
            let playback = bus.playback_rate();
            if rate != 0 && playback != 0 {
                handoff.post(FrameResampler::<(f32, f32)>::plan(rate, playback));
            }
        }
    }

    #[cfg(any(target_os = "linux", test))]
    fn output_rate_is_read(&self) -> bool {
        self.output_rate_reported.load(Ordering::Relaxed)
    }

    #[cfg(any(target_os = "linux", test))]
    fn output_step_is_read(&self) -> bool {
        self.output_step_reported.load(Ordering::Relaxed)
    }

    /// The step the output driver or graph really feeds, frames.  Called by
    /// the playback callbacks.  It marks the step only: the rate is a
    /// separate reading, published by the stream's format handler or by
    /// [`Self::set_output`].
    pub(crate) fn note_output_step(&self, frames: u32) {
        self.output_step_frames.store(frames, Ordering::Relaxed);
        self.output_step_reported.store(true, Ordering::Relaxed);
    }

    pub(crate) fn set_target_frames(&self, frames: usize) {
        self.base_frames
            .store(frames.max(64) as u64, Ordering::Relaxed);
        self.target_frames
            .store(self.floor_frames() as u64, Ordering::Relaxed);
    }

    pub(crate) fn target_frames(&self) -> usize {
        self.target_frames.load(Ordering::Relaxed) as usize
    }

    pub(crate) fn bridge_buffer_mode(&self) -> BufferMode {
        BufferMode::from_u32(self.bridge_buffer_mode.load(Ordering::Relaxed))
    }

    pub(crate) fn floor_frames(&self) -> usize {
        let base = self.base_frames.load(Ordering::Relaxed) as usize;
        let input = self.input_block.load(Ordering::Relaxed) as usize;
        let output = self.output_block.load(Ordering::Relaxed) as usize;
        let floor = match self.bridge_buffer_mode() {
            // Twice the input buffer, and at least one input block or two
            // output blocks.
            BufferMode::Auto => base.max(input).max(output * 2),
            // The least that can work: one block from each side.
            BufferMode::Minimum => {
                if input + output == 0 {
                    base
                } else {
                    input + output
                }
            }
            BufferMode::Fixed => {
                let ms = f32::from_bits(self.fixed_bridge_buffer_ms.load(Ordering::Relaxed));
                let rate = match self.source_rate.load(Ordering::Relaxed) {
                    0 => 48_000,
                    rate => rate,
                };
                (ms.max(0.0) * rate as f32 / 1000.0).round() as usize
            }
        };
        floor.clamp(32, self.capacity_frames / 2)
    }

    /// Fixed pins the target there exactly.
    pub(crate) fn lift_to_floor(&self) {
        let floor = self.floor_frames() as u64;
        let current = self.target_frames.load(Ordering::Relaxed);
        if self.bridge_buffer_mode() == BufferMode::Fixed {
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

    /// Advances even when the ring is full, so a stalled capture is not
    /// mistaken for a consumer that is ahead.
    pub(crate) fn note_input_frames(&self, frames: usize) {
        self.input_frames
            .fetch_add(frames as u64, Ordering::Relaxed);
    }

    pub(crate) fn note_output_block(&self, frames: usize) {
        if self.output_block.swap(frames as u64, Ordering::Relaxed) != frames as u64 {
            self.lift_to_floor();
        }
    }

    /// A new mode, or a new fixed size, starts from its own floor.
    pub(crate) fn set_bridge_buffer(&self, mode: BufferMode, fixed_ms: f32) {
        let mode_changed =
            self.bridge_buffer_mode.swap(mode as u32, Ordering::Relaxed) != mode as u32;
        let ms_changed = self
            .fixed_bridge_buffer_ms
            .swap(fixed_ms.to_bits(), Ordering::Relaxed)
            != fixed_ms.to_bits();
        if mode_changed || (ms_changed && mode == BufferMode::Fixed) {
            self.target_frames
                .store(self.floor_frames() as u64, Ordering::Relaxed);
        }
    }

    pub(crate) fn raise_target_after_dropout(&self) {
        let step = match self.bridge_buffer_mode() {
            BufferMode::Auto => 256,
            // Smaller steps, so one dropout does not undo the minimum setting.
            BufferMode::Minimum => (self.output_block.load(Ordering::Relaxed) / 4).max(64),
            // Fixed stays where the person put it.
            BufferMode::Fixed => return,
        };
        let floor = self.floor_frames() as u64;
        let current = self.target_frames.load(Ordering::Relaxed);
        let ceiling = (self.capacity_frames / 2) as u64;
        let next = (current + step).min(ceiling).max(floor);
        if next != current {
            self.target_frames.store(next, Ordering::Relaxed);
        }
    }

    pub(crate) fn relax_after(&self) -> Duration {
        match self.bridge_buffer_mode() {
            BufferMode::Minimum => Duration::from_secs(2),
            _ => Duration::from_secs(4),
        }
    }

    pub(crate) fn relax_target(&self) {
        let step = match self.bridge_buffer_mode() {
            BufferMode::Auto => 64,
            BufferMode::Minimum => 32,
            BufferMode::Fixed => return,
        };
        let floor = self.floor_frames() as u64;
        let current = self.target_frames.load(Ordering::Relaxed);
        if current > floor {
            self.target_frames
                .store(current.saturating_sub(step).max(floor), Ordering::Relaxed);
        }
    }

    pub(crate) fn set_master(&self, mute: bool, input_channels: [bool; 2], drift: bool) {
        self.mute.store(mute, Ordering::Relaxed);
        self.input_enabled[0].store(input_channels[0], Ordering::Relaxed);
        self.input_enabled[1].store(input_channels[1], Ordering::Relaxed);
        self.drift_enabled.store(drift, Ordering::Relaxed);
    }

    /// How many extra output slots are live (≤ [`MAX_EXTRA_OUTPUTS`]).
    pub(crate) fn extra_output_count(&self) -> usize {
        self.extra_output_count
            .load(Ordering::Relaxed)
            .min(MAX_EXTRA_OUTPUTS)
    }

    /// Not part of the bridge signature.
    pub(crate) fn set_extra_outputs(
        &self,
        settings: &[OutputDeviceSettings],
        plan: &ExtraOutputPlan,
        bridge_buffers: &[(BufferMode, f32)],
    ) {
        let count = settings.len().min(plan.count).min(MAX_EXTRA_OUTPUTS);
        let previous = self
            .extra_output_plan
            .lock()
            .map(|plan| plan.slots.clone())
            .unwrap_or_default();
        for (index, slot) in settings.iter().take(count).enumerate() {
            let old = &previous[index];
            let new = &plan.slots[index];
            if old.device_id != new.device_id
                || old.node_name != new.node_name
                || old.virtual_output != new.virtual_output
            {
                // The trim and the loudness history belong to the device
                // that left.
                self.extra_outputs[index].note_new_device();
            }
            self.extra_outputs[index].set(slot);
            // Stereo until a generic stream opens and overwrites the count.
            if self.extra_outputs[index].channels() == 0 {
                self.extra_outputs[index].set_channels(2);
            }
            let link = &plan.slots[index];
            self.extra_output_virtual[index].store(link.virtual_output, Ordering::Relaxed);
            self.virtual_output_on[index].store(link.virtual_output_on, Ordering::Relaxed);
            if let Some(&(mode, ms)) = bridge_buffers.get(index) {
                self.extra_output_buses[index].set_bridge_buffer(mode, ms);
            }
        }
        for index in count..MAX_EXTRA_OUTPUTS {
            self.extra_output_virtual[index].store(false, Ordering::Relaxed);
            self.virtual_output_on[index].store(false, Ordering::Relaxed);
            self.extra_output_ring_active[index].store(false, Ordering::Relaxed);
        }
        self.extra_output_count.store(count, Ordering::Relaxed);
        if let Ok(mut current) = self.extra_output_plan.lock() {
            *current = ExtraOutputPlan {
                slots: plan.slots.clone(),
                count,
            };
        }
    }

    /// The controls of output `device` (0 is the clock output, then the
    /// extra outputs).
    fn output_controls(&self, device: usize) -> Option<&DeviceControls> {
        if device == 0 {
            Some(&self.clock_output)
        } else {
            (device - 1 < self.extra_output_count()).then(|| &self.extra_outputs[device - 1])
        }
    }

    /// Clears one output's loudness windows and integrated history (0 is
    /// the clock output, then the extra outputs).
    pub(crate) fn reset_loudness(&self, device: usize) {
        if let Some(controls) = self.output_controls(device) {
            controls.request_loudness_reset();
        }
    }

    /// Arms every live path's Match paths meter at once.
    pub(crate) fn start_match_paths(&self) {
        for path in &self.paths {
            if path.state() == PathState::Active {
                path.start_match_measurement();
            }
        }
    }

    /// Stops every path's Match paths meter.  The readings stay until the
    /// next start, so the UI can collect them first.
    pub(crate) fn stop_match_paths(&self) {
        for path in &self.paths {
            path.stop_match_measurement();
        }
    }

    /// Path `slot`'s last Match paths reading: gated integrated LUFS and the
    /// gated block count.
    pub(crate) fn path_match_measurement(&self, slot: usize) -> (f32, u32) {
        self.paths[slot.min(MAX_PATHS - 1)].match_measurement()
    }

    /// Starts an output Learn on `device`.
    pub(crate) fn start_output_learn(&self, device: usize) {
        if let Some(controls) = self.output_controls(device) {
            controls.start_learn();
        }
    }

    /// Stops an output Learn on `device`.
    pub(crate) fn stop_output_learn(&self, device: usize) {
        if let Some(controls) = self.output_controls(device) {
            controls.stop_learn();
        }
    }

    /// The running Learn reading on `device`: gated integrated LUFS and the
    /// number of absolute-gated 400 ms blocks.
    pub(crate) fn loudness_learn(&self, device: usize) -> Option<(f32, u32)> {
        self.output_controls(device)
            .and_then(DeviceControls::learn_reading)
    }

    /// The playback bus for one generic extra output slot.
    pub(crate) fn extra_output_bus(&self, index: usize) -> &ExtraOutputBus {
        &self.extra_output_buses[index.min(MAX_EXTRA_OUTPUTS - 1)]
    }

    /// Whether the clock callback should push finished frames into this
    /// extra output's ring.
    pub(crate) fn extra_output_ring_active(&self, index: usize) -> &AtomicBool {
        &self.extra_output_ring_active[index.min(MAX_EXTRA_OUTPUTS - 1)]
    }

    /// The ring is laid out before this turns true, and duplex never feeds it.
    pub(crate) fn feeds_extra_output(&self, index: usize) -> bool {
        !self.direct.load(Ordering::Relaxed)
            && self.extra_output_ring_active(index).load(Ordering::Acquire)
    }

    /// The generation must still match the start of the block.
    pub(crate) fn push_extra_output_frame(
        &self,
        index: usize,
        generation: u64,
        frame: &DeviceFrame,
    ) {
        if self.extra_output_ring_active(index).load(Ordering::Relaxed)
            && self.extra_output_bus(index).generation() == generation
        {
            self.extra_output_bus(index).push_frame(frame);
        }
    }

    /// Notes the clock output's block size on every active extra output bus so
    /// their bridge buffers size themselves from the producer step.
    pub(crate) fn note_extra_output_producer_block(&self, frames: usize) {
        if self.direct.load(Ordering::Relaxed) {
            return;
        }
        let count = self.extra_output_count();
        for index in 0..count {
            if self.extra_output_ring_active[index].load(Ordering::Relaxed) {
                self.extra_output_buses[index].note_input_block(frames);
            }
        }
    }

    /// Whether extra output `index` is the virtual RigWerk Out source.
    #[cfg(target_os = "linux")]
    pub(crate) fn extra_output_is_virtual(&self, index: usize) -> bool {
        index < MAX_EXTRA_OUTPUTS && self.extra_output_virtual[index].load(Ordering::Relaxed)
    }

    /// Whether the virtual source at `index` should be advertised.
    #[cfg(target_os = "linux")]
    pub(crate) fn virtual_output_is_on(&self, index: usize) -> bool {
        index < MAX_EXTRA_OUTPUTS && self.virtual_output_on[index].load(Ordering::Relaxed)
    }

    /// A clone of the live extra output plan for the PipeWire linker.
    pub(crate) fn extra_output_plan(&self) -> ExtraOutputPlan {
        self.extra_output_plan
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

    /// The first cause wins.
    pub(crate) fn note_callback_failure(&self, failure: CallbackFailure) {
        let _ = self.callback_failure.compare_exchange(
            0,
            failure.code(),
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
    }

    /// Read before the block, so a broken chain is not driven again.
    pub(crate) fn callback_failure(&self) -> Option<CallbackFailure> {
        CallbackFailure::from_code(self.callback_failure.load(Ordering::Relaxed))
    }

    /// Only when the streams are gone.  The flag keeps a panicked callback
    /// out of the chain during teardown.
    pub(crate) fn clear_callback_failure(&self) {
        self.callback_failure.store(0, Ordering::Relaxed);
    }

    pub(crate) fn input_gain(&self) -> f32 {
        f32::from_bits(self.input_gain.load(Ordering::Relaxed))
    }

    pub(crate) fn output_gain(&self) -> f32 {
        f32::from_bits(self.output_gain.load(Ordering::Relaxed))
    }

    /// Before the input gain and the channel enable.
    pub(crate) fn capture_calibration(&self, left: f32, right: f32) {
        self.calibration.capture_frame(left, right);
    }

    /// A full ring counts an overrun and drops the frame.  The scope still
    /// sees it.
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

    /// Before the channel mute, so the tuner hears a side that is not
    /// monitored.
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
    /// fresh run reads nothing instead of stale or partly unwritten data.
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
    #[cfg(target_os = "linux")]
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

    /// One store publishes the beat and the rhythm it belongs to.
    pub(crate) fn publish_metronome_beat(&self, beat: u8, rhythm: u8) {
        self.metronome_beat
            .store(pack_metronome_beat(beat, rhythm), Ordering::Relaxed);
    }

    /// The rhythm byte stays.
    pub(crate) fn clear_metronome_beat(&self) {
        self.metronome_beat.fetch_and(0xFF00, Ordering::Relaxed);
    }

    /// The published beat and its rhythm, from one load.
    pub(crate) fn metronome_beat(&self) -> (u8, u8) {
        unpack_metronome_beat(self.metronome_beat.load(Ordering::Relaxed))
    }

    pub(crate) fn snapshot(&self) -> Snapshot {
        // The explicit device flag takes precedence: a callback caught while
        // that device tears down must not relabel it.
        let device_lost_flag = self.device_lost.load(Ordering::Relaxed);
        let callback_failure = if device_lost_flag {
            None
        } else {
            self.callback_failure()
        };
        let error = self
            .error
            .lock()
            .ok()
            .and_then(|error| error.clone())
            .or_else(|| callback_failure.map(|failure| failure.message().to_owned()));
        let device_lost = device_lost_flag || callback_failure.is_some();
        // A callback mid-block when the bridge failed may store its beat
        // after `set_error`, and a stopping bridge keeps mixing until its
        // teardown ends; a bridge that is not running shows no beat.
        let (beat, _) = self.metronome_beat();
        let running = self.running.load(Ordering::Relaxed);
        let metronome_beat = if error.is_some() || device_lost || !running {
            0
        } else {
            beat
        };
        Snapshot {
            in_l: f32::from_bits(self.in_l.load(Ordering::Relaxed)),
            in_r: f32::from_bits(self.in_r.load(Ordering::Relaxed)),
            out_l: f32::from_bits(self.out_l.load(Ordering::Relaxed)),
            out_r: f32::from_bits(self.out_r.load(Ordering::Relaxed)),
            gate_reduction_db: std::array::from_fn(|slot| self.paths[slot].gate_reduction_db()),
            out_channels: self.clock_output.meters(),
            out_reduction_db: self.clock_output.reduction_db(),
            out_loudness: self.clock_output.loudness(),
            output_channels: self.clock_output.channels(),
            #[cfg(test)]
            extra_output_count: self
                .extra_output_count
                .load(Ordering::Relaxed)
                .min(MAX_EXTRA_OUTPUTS),
            extra_output_channels: std::array::from_fn(|index| self.extra_outputs[index].meters()),
            extra_output_reduction_db: std::array::from_fn(|index| {
                self.extra_outputs[index].reduction_db()
            }),
            extra_output_loudness: std::array::from_fn(|index| {
                self.extra_outputs[index].loudness()
            }),
            extra_output_channel_counts: std::array::from_fn(|index| {
                self.extra_outputs[index].channels()
            }),
            extra_output_missing: std::array::from_fn(|index| {
                self.extra_output_buses[index]
                    .missing
                    .load(Ordering::Relaxed)
            }),
            extra_output_dropouts: std::array::from_fn(|index| {
                self.extra_output_buses[index]
                    .dropouts
                    .load(Ordering::Relaxed)
            }),
            extra_output_drift_at_limit: std::array::from_fn(|index| {
                self.extra_output_buses[index]
                    .drift_at_limit
                    .load(Ordering::Relaxed)
            }),
            extra_output_rates: std::array::from_fn(|index| {
                self.extra_output_buses[index].playback_rate()
            }),
            extra_output_buffer_frames: std::array::from_fn(|index| {
                let bus = &self.extra_output_buses[index];
                if bus.playback_rate() == 0 {
                    0
                } else {
                    bus.device_block()
                }
            }),
            extra_output_ring_fill: std::array::from_fn(|index| {
                let bus = &self.extra_output_buses[index];
                if bus.playback_rate() == 0 {
                    0.0
                } else {
                    bus.fill()
                }
            }),
            extra_output_target_frames: std::array::from_fn(|index| {
                let bus = &self.extra_output_buses[index];
                if bus.playback_rate() == 0 {
                    0
                } else {
                    bus.target_frames()
                }
            }),
            dropouts: self.dropouts.load(Ordering::Relaxed),
            #[cfg(test)]
            input_frames: self.input_frames.load(Ordering::Relaxed),
            frames: self.frames.load(Ordering::Relaxed),
            ring_fill: self.ring_fill(),
            target_frames: self.target_frames(),
            drift_ppm: f32::from_bits(self.drift_ppm.load(Ordering::Relaxed)),
            error,
            device_lost,
            callback_failure,
            #[cfg(test)]
            status: self
                .status
                .lock()
                .map(|status| status.clone())
                .unwrap_or_default(),
            input_rate: self.source_rate.load(Ordering::Relaxed),
            input_rate_reported: self.input_rate_reported.load(Ordering::Relaxed),
            input_block_frames: self.input_block.load(Ordering::Relaxed) as u32,
            output_rate: self.output_rate.load(Ordering::Relaxed),
            output_rate_reported: self.output_rate_reported.load(Ordering::Relaxed),
            output_step_frames: self.output_step_frames.load(Ordering::Relaxed),
            output_step_reported: self.output_step_reported.load(Ordering::Relaxed),
            output_mode: self
                .output_mode
                .lock()
                .map(|mode| mode.clone())
                .unwrap_or_default(),
            opened_output_mode: OutputMode::from_u8(
                self.opened_output_mode.load(Ordering::Relaxed),
            ),
            path_delay_ms: self.path_delay_ms(),
            direct: self.direct.load(Ordering::Relaxed),
            metronome_beat,
        }
    }
}

#[cfg(test)]
impl Shared {
    /// Replaces path 0's saved effect list without loading anything.
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

    /// The loaded plugin behind one effect, for its own editor window.
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

    /// Never takes the chain lock, so drawing cannot make the callback
    /// pass a block dry.
    pub(crate) fn plugin_views(&self) -> Arc<[PluginView]> {
        self.main_path().plugin_views()
    }

    /// Bumped on every publish of [`Self::plugin_views`].
    #[cfg(test)]
    pub(crate) fn plugin_views_version(&self) -> u64 {
        self.main_path().plugin_views_version()
    }

    pub(crate) fn set_mix(
        &self,
        mute: bool,
        mono: bool,
        input_channels: [bool; 2],
        output_channels: [bool; 2],
        drift: bool,
    ) {
        self.set_master(mute, input_channels, drift);
        self.main_path().set_mono(mono);
        self.main_path().set_output_enabled(output_channels);
    }

    /// Sets path 0's input routing.
    pub(crate) fn set_routes(&self, routes: [[bool; 4]; 2]) {
        self.main_path().set_routes(routes);
    }
}

/// The published metronome beat in the low byte, its rhythm in the high one.
pub(crate) fn pack_metronome_beat(beat: u8, rhythm: u8) -> u16 {
    u16::from(beat) | (u16::from(rhythm) << 8)
}

/// `(beat, rhythm)` from [`pack_metronome_beat`].
pub(crate) fn unpack_metronome_beat(packed: u16) -> (u8, u8) {
    let [beat, rhythm] = packed.to_le_bytes();
    (beat, rhythm)
}

/// Two blocks is enough to start.  A dropout grows it.
pub(crate) fn initial_target(buffer_frames: usize) -> usize {
    (buffer_frames * 2).clamp(96, 4096)
}

#[cfg(test)]
pub(crate) mod tests;
