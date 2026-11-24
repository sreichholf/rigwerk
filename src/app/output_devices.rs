//! The output devices under the master, as plain rows.
//!
//! The console strips, the output sheet and the issues drawer read the same
//! list.  A row is names,
//! flags, and settings.  The colour of a device's state stays in `ui`.
//! [`ExtraOutputReading`] is what an extra output's own stream really runs at,
//! from the live snapshot.

use std::time::{Duration, Instant};

use crate::app::chain::ChainSelection;
use crate::app::latency::output_mode_phrase;
use crate::app::screen::{format_ms, Sheet};
use crate::app::settings::ExtraOutputPref;
use crate::app::LogTone;
use crate::app::RigWerkApp;
#[cfg(target_os = "windows")]
use crate::audio::VIRTUAL_OUTPUT_NAME;
use crate::audio::{
    low_bits, ChannelMap, EngineState, LoudnessReading, MapPreset, MixSide, OutputDeviceSettings,
    PathMapSettings, UnusedChannels, DEVICE_LEVEL_MAX_DB, DEVICE_LEVEL_MIN_DB,
    LOUDNESS_TARGET_MAX_LUFS, LOUDNESS_TARGET_MIN_LUFS, LOUDNESS_TRIM_MAX_DB, MAX_EXTRA_OUTPUTS,
    MAX_OUTPUT_CHANNELS, MAX_PATHS, VIRTUAL_OUTPUT_ID,
};

/// At the extra output cap.
pub(crate) const ADD_DEVICE_HINT_FULL: &str = "Four extra outputs is the maximum for now";

/// One output device as the console and its sheet list it.
#[derive(Clone, Debug)]
pub(crate) struct OutputDeviceRow {
    pub(crate) name: String,
    pub(crate) backend: String,
    /// Channels the meters show: the real count, capped at
    /// [`MAX_OUTPUT_CHANNELS`]. See [`device_channel_count`].
    pub(crate) channels: usize,
    /// Channels the click-mask chips offer.  See [`click_channels`].
    pub(crate) click_channels: usize,
    /// The device whose callback runs the chain.
    pub(crate) clock: bool,
    /// RigWerk Out virtual source.
    pub(crate) virtual_output: bool,
    /// Index into `extra_outputs` when this is not the clock output.
    pub(crate) extra_output_index: Option<usize>,
    /// Device id missing from the current inventory.  For the clock
    /// output: the last scan did not list it, and it is held for one more.
    pub(crate) missing: bool,
    pub(crate) virtual_output_on: bool,
    pub(crate) settings: OutputDeviceSettings,
    /// Safety-limiter gain reduction, dB.  0 when the device is not
    /// limiting, and when the bridge is not running.
    pub(crate) reduction_db: f32,
    /// Loudness readings and the applied output trim.  No reading while
    /// the bridge is stopped.
    pub(crate) loudness: LoudnessReading,
}

/// Where an output's trim came from.  Runtime only: a save stores the value,
/// not how it was set, so a loaded nonzero trim reads as *Set by hand*.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) enum OutputTrimOrigin {
    /// No trim.
    #[default]
    None,
    /// A finished Learn, with the loudness it measured.
    Learned { lufs: f32 },
    /// Dragged or typed.
    Hand,
}

/// What one extra output's own stream runs at, from the live snapshot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ExtraOutputReading {
    /// The rate the device opened at, Hz.
    pub(crate) rate: u32,
    /// The frames its driver asks for per callback.  0 until the first.
    pub(crate) buffer_frames: u32,
    /// The ring's fill against its bridge buffer, 0 to 1.
    pub(crate) fill: f32,
    /// The bridge buffer, in ms at the clock output's rate.
    pub(crate) bridge_buffer_ms: f32,
}

/// A loudness measurement in progress.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LoudnessMeasurementKind {
    Learn,
    /// Every live path at once, for `Match paths`.
    Paths,
}

/// One running loudness measurement: an output Learn or `Match paths`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LoudnessMeasurement {
    pub(crate) device: usize,
    pub(crate) kind: LoudnessMeasurementKind,
    pub(crate) started: Instant,
}

/// How long one output is measured before its level is learned.
const LEARN_SECONDS: f32 = 8.0;
/// Learn needs this many absolute-gated 400 ms blocks, about 3 s: the meter
/// steps every 100 ms, so 27 blocks span 0.4 + 2.6 s.
const LEARN_MIN_BLOCKS: u32 = 27;
/// A reading more than this far under the target is noise, not program.
const LOUDNESS_GATE_BELOW_TARGET_LU: f32 = 20.0;

/// The new output trim after an output measured `integrated_lufs`.
pub(crate) fn learned_trim_db(current: f32, target_lufs: f32, integrated_lufs: f32) -> f32 {
    (current + target_lufs - integrated_lufs).clamp(-LOUDNESS_TRIM_MAX_DB, LOUDNESS_TRIM_MAX_DB)
}

/// Whether a measured loudness is close enough to the target to adjust
/// against.  Hiss and room noise are far below it and would otherwise get
/// the full boost.
pub(crate) fn usable_loudness(target_lufs: f32, measured_lufs: f32) -> bool {
    measured_lufs > target_lufs - LOUDNESS_GATE_BELOW_TARGET_LU
}

/// The learned trim from a finished Learn window, or `None` when there was
/// not enough signal above the absolute gate or the reading was too far
/// under the target.
pub(crate) fn learned_trim_after(
    current: f32,
    target_lufs: f32,
    integrated_lufs: f32,
    blocks: u32,
) -> Option<f32> {
    (blocks >= LEARN_MIN_BLOCKS
        && integrated_lufs > crate::audio::SILENCE_LUFS
        && usable_loudness(target_lufs, integrated_lufs))
    .then(|| learned_trim_db(current, target_lufs, integrated_lufs))
}

/// The badge an output strip carries (`docs/ui-spec.md` §3.3).  The last
/// three are drawn in amber.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutputBadge {
    Clock,
    Extra,
    /// A PipeWire extra output, played in the clock output's graph.
    InGraph,
    Virtual,
    /// Not listed, or lost and being retried.
    Missing,
    Starting,
    Error,
}

impl OutputBadge {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Clock => "Clock",
            Self::Extra => "Extra",
            Self::InGraph => "In graph",
            Self::Virtual => "Virtual",
            Self::Missing => "Missing · retrying",
            Self::Starting => "Starting",
            Self::Error => "Error",
        }
    }

    /// Missing, starting or failed: amber, with the word.
    pub(crate) fn warns(self) -> bool {
        matches!(self, Self::Missing | Self::Starting | Self::Error)
    }
}

/// What a click on a *Plays* dot did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlaysClick {
    /// The map was empty: the path now plays on *Out 1/2*.
    Applied,
    /// The map was a plain *Out 1/2*: it is cleared.
    Cleared,
    /// Any other map: nothing changed, the output sheet should open so a
    /// custom map is never lost to one click.
    OpenSheet,
    /// No such path or output.
    Nothing,
}

/// The metronome channels an output gets when it is switched on and no
/// earlier choice fits it: 3 and 4 with four or more click channels, else
/// 1 and 2 (or 1 on a mono device).
pub(crate) fn default_click_mask(click_channels: usize) -> u8 {
    if click_channels >= 4 {
        0b1100
    } else {
        0b0011 & low_bits(click_channels)
    }
}

/// The loudest of a device's first `channels` meters.
pub(crate) fn loudest(levels: &[f32], channels: usize) -> f32 {
    levels
        .iter()
        .take(channels)
        .fold(0.0_f32, |loudest, level| loudest.max(*level))
}

/// Channels to show for a device: what its open stream reported, else what
/// the device scan found, else stereo; never more than the map can address.
// An ASIO scan may report a stub channel count; a running ASIO stream
// publishes the real open count (capped at MAX_OUTPUT_CHANNELS).
pub(crate) fn device_channel_count(reported: u32, inventory: usize) -> usize {
    let channels = if reported > 0 {
        reported as usize
    } else if inventory > 0 {
        inventory
    } else {
        2
    };
    channels.min(MAX_OUTPUT_CHANNELS)
}

/// Channels the output sheet offers for a click mask.  A native PipeWire node
/// (`pipewire:` id) is opened stereo: the playback stream and the duplex
/// node both map two channels, even when the sink reports more.  A chip
/// past channel 2 would toggle a lane that is never written.  A device
/// opened through cpal, the ALSA `pipewire` PCM included, keeps its
/// measured count, so this limits only the chips, never `channels`.
pub(crate) fn click_channels(device_id: &str, channels: usize) -> usize {
    #[cfg(target_os = "linux")]
    if crate::audio::is_native_id(device_id) {
        return channels.min(2);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = device_id;
    channels
}

impl RigWerkApp {
    /// Extra output `index`'s live readings.  `None` while the bridge is not
    /// running or the extra output has no stream of its own: not open yet,
    /// missing, or played in the PipeWire graph.
    pub(crate) fn extra_output_reading(&self, index: usize) -> Option<ExtraOutputReading> {
        if self.engine.state != EngineState::Running {
            return None;
        }
        let snapshot = &self.snapshot;
        let rate = *snapshot.extra_output_rates.get(index)?;
        if rate == 0 {
            return None;
        }
        let clock_rate = match snapshot.output_rate {
            0 => rate,
            clock => clock,
        };
        Some(ExtraOutputReading {
            rate,
            buffer_frames: snapshot.extra_output_buffer_frames[index],
            fill: snapshot.extra_output_ring_fill[index],
            bridge_buffer_ms: snapshot.extra_output_target_frames[index] as f32 * 1000.0
                / clock_rate as f32,
        })
    }

    /// The output devices under the master, clock output first, then extra outputs.
    pub(crate) fn output_device_rows(&self) -> Vec<OutputDeviceRow> {
        let output = self.selected_output();
        let reported = if self.engine.state == EngineState::Running {
            self.snapshot.output_channels
        } else {
            0
        };
        let running = self.engine.state == EngineState::Running;
        let channels = device_channel_count(reported, output.channels);
        // A missed scan keeps the clock output selected (see
        // `hold_unconfirmed_devices`), so it is still in the inventory.  The
        // miss counter is what says the scan did not see it.
        let clock_missing = !output.demo && self.output_misses > 0;
        let mut rows = vec![OutputDeviceRow {
            name: output.name.clone(),
            backend: output.backend.clone(),
            channels,
            click_channels: click_channels(&output.id, channels),
            clock: true,
            virtual_output: false,
            extra_output_index: None,
            missing: clock_missing,
            virtual_output_on: false,
            settings: self.clock_output,
            reduction_db: if running {
                self.snapshot.out_reduction_db
            } else {
                0.0
            },
            loudness: if running {
                self.snapshot.out_loudness
            } else {
                LoudnessReading::default()
            },
        }];
        for (index, extra_output) in self.extra_outputs.iter().enumerate() {
            let (channels, missing) = if extra_output.virtual_output {
                (2, false)
            } else {
                let found = self
                    .inventory
                    .outputs
                    .iter()
                    .find(|device| device.id == extra_output.id);
                let channels = if self.engine.state == EngineState::Running {
                    let count = self
                        .snapshot
                        .extra_output_channel_counts
                        .get(index)
                        .copied()
                        .unwrap_or(0);
                    device_channel_count(count, found.map(|d| d.channels).unwrap_or(2))
                } else {
                    device_channel_count(0, found.map(|d| d.channels).unwrap_or(2))
                };
                (channels, found.is_none())
            };
            rows.push(OutputDeviceRow {
                name: if extra_output.name.is_empty() {
                    extra_output.id.clone()
                } else {
                    extra_output.name.clone()
                },
                backend: extra_output.backend.clone(),
                channels,
                click_channels: click_channels(&extra_output.id, channels),
                clock: false,
                virtual_output: extra_output.virtual_output,
                extra_output_index: Some(index),
                missing,
                virtual_output_on: extra_output.virtual_output_on,
                settings: extra_output.settings,
                reduction_db: if running {
                    self.snapshot
                        .extra_output_reduction_db
                        .get(index)
                        .copied()
                        .unwrap_or(0.0)
                } else {
                    0.0
                },
                loudness: if running {
                    self.snapshot
                        .extra_output_loudness
                        .get(index)
                        .copied()
                        .unwrap_or_default()
                } else {
                    LoudnessReading::default()
                },
            });
        }
        rows
    }

    /// Output `device`'s strip badge (0 = clock output, then extra outputs).
    pub(crate) fn output_badge(&self, device: usize) -> OutputBadge {
        let rows = self.output_device_rows();
        let Some(row) = rows.get(device) else {
            return OutputBadge::Missing;
        };
        let retrying = row.extra_output_index.is_some_and(|index| {
            self.engine.state == EngineState::Running
                && self
                    .snapshot
                    .extra_output_missing
                    .get(index)
                    .copied()
                    .unwrap_or(false)
        });
        if row.missing || retrying {
            return OutputBadge::Missing;
        }
        match self.engine.state {
            EngineState::Starting => return OutputBadge::Starting,
            EngineState::Error if row.clock => return OutputBadge::Error,
            _ => {}
        }
        if row.clock {
            OutputBadge::Clock
        } else if row.virtual_output {
            OutputBadge::Virtual
        } else if self.extra_outputs_in_graph() {
            OutputBadge::InGraph
        } else {
            OutputBadge::Extra
        }
    }

    /// Whether extra outputs play in the clock output's PipeWire graph.
    pub(crate) fn extra_outputs_in_graph(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            crate::audio::is_native_id(&self.selected_output().id)
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }

    /// The buffer line of output `device`'s strip.  The clock output: its
    /// output mode and, while the bridge runs, the buffer time it really
    /// opened.  An extra output: its bridge buffer (`auto 5.3 ms`), or *In
    /// graph*.
    pub(crate) fn output_buffer_line(&self, device: usize) -> String {
        if device == 0 {
            let running = self.engine.state == EngineState::Running;
            let mode = self
                .snapshot
                .opened_output_mode
                .filter(|_| running)
                .map(output_mode_phrase)
                .unwrap_or_else(|| self.chosen_output_mode());
            let rate = self.snapshot.output_rate;
            let frames = self.snapshot.output_step_frames;
            if running && rate > 0 && frames > 0 {
                return format!(
                    "{mode} · {}",
                    format_ms(frames as f32 * 1000.0 / rate as f32)
                );
            }
            return mode.to_owned();
        }
        let index = device - 1;
        if self
            .extra_outputs
            .get(index)
            .is_some_and(|extra| extra.virtual_output)
            || self.extra_outputs_in_graph()
        {
            return "In graph".to_owned();
        }
        let mode = match self.bridge_buffer_mode {
            crate::audio::BufferMode::Auto => "auto",
            crate::audio::BufferMode::Minimum => "minimum",
            crate::audio::BufferMode::Fixed => "fixed",
        };
        match self.extra_output_reading(index) {
            Some(reading) => format!("{mode} {}", format_ms(reading.bridge_buffer_ms)),
            None => mode.to_owned(),
        }
    }

    /// The output mode the clock output is set to open with.
    fn chosen_output_mode(&self) -> &'static str {
        if cfg!(target_os = "windows") {
            self.wasapi_output_mode.label()
        } else if self.low_latency_output {
            "Low latency"
        } else {
            "Standard"
        }
    }

    /// Output `device`'s level, clamped to the device range.
    pub(crate) fn set_output_level(&mut self, device: usize, level_db: f32) {
        if let Some(settings) = self.device_settings_mut().nth(device) {
            let level_db = if level_db.is_nan() { 0.0 } else { level_db };
            settings.level_db = level_db.clamp(DEVICE_LEVEL_MIN_DB, DEVICE_LEVEL_MAX_DB);
        }
    }

    pub(crate) fn set_output_mute(&mut self, device: usize, mute: bool) {
        if let Some(settings) = self.device_settings_mut().nth(device) {
            settings.mute = mute;
        }
    }

    /// Sets output `device`'s loudness target, clamped to the sheet's range.
    pub(crate) fn set_output_loudness_target(&mut self, device: usize, target_lufs: f32) {
        if let Some(settings) = self.device_settings_mut().nth(device) {
            let target_lufs = if target_lufs.is_nan() {
                crate::audio::LOUDNESS_TARGET_DEFAULT_LUFS
            } else {
                target_lufs.clamp(LOUDNESS_TARGET_MIN_LUFS, LOUDNESS_TARGET_MAX_LUFS)
            };
            settings.loudness.target_lufs = target_lufs;
        }
    }

    /// Clears output `device`'s loudness windows and integrated history.
    pub(crate) fn reset_output_loudness(&mut self, device: usize) {
        self.engine.reset_loudness(device);
    }

    /// Sets output `device`'s trim by hand.  A NaN leaves the value alone;
    /// the rest is clamped to ±[`LOUDNESS_TRIM_MAX_DB`].
    pub(crate) fn set_output_trim(&mut self, device: usize, trim_db: f32) {
        if trim_db.is_nan() {
            return;
        }
        if let Some(settings) = self.device_settings_mut().nth(device) {
            settings.loudness.trim_db = trim_db.clamp(-LOUDNESS_TRIM_MAX_DB, LOUDNESS_TRIM_MAX_DB);
        }
        self.note_output_trim_origin(device, OutputTrimOrigin::Hand);
    }

    /// Clears output `device`'s trim.  The mapper ramps it to unity.
    pub(crate) fn clear_output_learn(&mut self, device: usize) {
        if let Some(settings) = self.device_settings_mut().nth(device) {
            settings.loudness.trim_db = 0.0;
        }
        self.note_output_trim_origin(device, OutputTrimOrigin::None);
    }

    /// Where output `device`'s current trim came from.
    pub(crate) fn output_trim_origin(&self, device: usize) -> OutputTrimOrigin {
        self.output_trim_origins
            .get(device)
            .copied()
            .unwrap_or_default()
    }

    fn note_output_trim_origin(&mut self, device: usize, origin: OutputTrimOrigin) {
        if let Some(slot) = self.output_trim_origins.get_mut(device) {
            *slot = origin;
        }
    }

    /// Rebuilds the runtime trim notes from the saved values: a nonzero trim
    /// a save restored reads as set by hand.
    pub(crate) fn reset_output_trim_origins(&mut self) {
        self.output_trim_origins = [OutputTrimOrigin::None; MAX_EXTRA_OUTPUTS + 1];
        let trims: Vec<f32> = self
            .device_settings()
            .iter()
            .take(MAX_EXTRA_OUTPUTS + 1)
            .map(|settings| settings.loudness.trim_db)
            .collect();
        for (device, trim_db) in trims.into_iter().enumerate() {
            if trim_db.abs() >= 0.05 {
                self.output_trim_origins[device] = OutputTrimOrigin::Hand;
            }
        }
    }

    /// Starts measuring output `device` so its output trim can be set.
    pub(crate) fn start_output_learn(&mut self, device: usize) {
        self.cancel_loudness_measurement();
        if self.engine.state != EngineState::Running {
            self.push_log(
                "Start the bridge before learning an output.",
                LogTone::Amber,
            );
            return;
        }
        let Some(settings) = self.device_settings().get(device).copied() else {
            return;
        };
        if settings.mute {
            self.push_log("Unmute the output before learning it.", LogTone::Amber);
            return;
        }
        let device_name = self
            .output_device_rows()
            .get(device)
            .map(|row| row.name.clone())
            .unwrap_or_else(|| "this output".to_owned());
        self.engine.start_output_learn(device);
        self.loudness_measurement = Some(LoudnessMeasurement {
            device,
            kind: LoudnessMeasurementKind::Learn,
            started: Instant::now(),
        });
        self.push_log(
            format!("Learning {device_name}… play the rig for eight seconds."),
            LogTone::Blue,
        );
    }

    /// Stops a running Learn, if any.
    pub(crate) fn cancel_loudness_measurement(&mut self) {
        if let Some(request) = self.loudness_measurement.take() {
            match request.kind {
                LoudnessMeasurementKind::Learn => {
                    self.engine.stop_output_learn(request.device);
                }
                LoudnessMeasurementKind::Paths => {
                    self.engine.stop_match_paths();
                    self.match_paths_result = None;
                }
            }
        }
    }

    /// Whether `device` is being learned right now.
    pub(crate) fn is_learning_loudness(&self, device: usize) -> bool {
        self.loudness_measurement.is_some_and(|request| {
            request.device == device && matches!(request.kind, LoudnessMeasurementKind::Learn)
        })
    }

    /// Finishes a Learn once its window has passed.  Runs once per UI frame
    /// from [`Self::publish_engine`].
    pub(crate) fn poll_loudness_measurement(&mut self) {
        let Some(request) = self.loudness_measurement else {
            return;
        };
        if self.engine.state != EngineState::Running {
            self.cancel_loudness_measurement();
            return;
        }
        let seconds = match request.kind {
            LoudnessMeasurementKind::Learn => LEARN_SECONDS,
            LoudnessMeasurementKind::Paths => crate::app::match_paths::MATCH_PATHS_SECONDS,
        };
        if request.started.elapsed() < Duration::from_secs_f32(seconds) {
            return;
        }
        match request.kind {
            LoudnessMeasurementKind::Learn => self.finish_output_learn(request.device),
            LoudnessMeasurementKind::Paths => self.finish_match_paths(),
        }
    }

    fn finish_output_learn(&mut self, device: usize) {
        let Some((integrated, blocks)) = self.engine.loudness_learn(device) else {
            self.engine.stop_output_learn(device);
            self.loudness_measurement = None;
            self.push_log(
                "No loudness reading for the output; play the rig and try again.",
                LogTone::Amber,
            );
            return;
        };
        self.engine.stop_output_learn(device);
        self.loudness_measurement = None;
        let Some(settings) = self.device_settings().get(device).copied() else {
            return;
        };
        let target = settings.loudness.target_lufs;
        let current = settings.loudness.trim_db;
        let Some(wanted) = learned_trim_after(current, target, integrated, blocks) else {
            self.push_log(
                "Too little signal, or too far below the target, to learn the output; play the rig for longer.",
                LogTone::Amber,
            );
            return;
        };
        if let Some(settings) = self.device_settings_mut().nth(device) {
            settings.loudness.trim_db = wanted;
        }
        self.note_output_trim_origin(device, OutputTrimOrigin::Learned { lufs: integrated });
        self.push_devices_to_engine();
        let device_name = self
            .output_device_rows()
            .get(device)
            .map(|row| row.name.clone())
            .unwrap_or_else(|| "the output".to_owned());
        if (wanted - (current + target - integrated)).abs() > 0.05 {
            self.push_log(
                format!(
                    "{device_name} hit the output trim limit; adjust the path levels or play level and learn again."
                ),
                LogTone::Amber,
            );
        } else {
            self.push_log(
                format!("Learned {device_name}: {integrated:.1} LUFS, trim {wanted:+.1} dB."),
                LogTone::Blue,
            );
        }
    }

    /// A click on path `slot`'s *Plays* dot on output `device`: an empty
    /// map gets *Out 1/2*, a plain *Out 1/2* is cleared, and any other map
    /// is left alone for the output sheet.  Bits past the device's
    /// channels are kept, as every map edit keeps them.
    pub(crate) fn click_plays(&mut self, device: usize, slot: usize) -> PlaysClick {
        if slot >= MAX_PATHS || self.paths[slot].is_none() {
            return PlaysClick::Nothing;
        }
        let channels = self.device_channels(device);
        let Some(settings) = self.device_settings_mut().nth(device) else {
            return PlaysClick::Nothing;
        };
        let map = settings.path_map[slot].map;
        let shown = low_bits(channels);
        let visible = ChannelMap {
            left: map.left & shown,
            right: map.right & shown,
        };
        let plain = ChannelMap {
            left: 0b01 & shown,
            right: 0b10 & shown,
        };
        if !visible.reaches_any(channels) {
            settings.path_map[slot].map = map.with_preset(MapPreset::Stereo12, channels);
            PlaysClick::Applied
        } else if visible == plain {
            settings.path_map[slot].map = map.with_preset(MapPreset::Clear, channels);
            PlaysClick::Cleared
        } else {
            PlaysClick::OpenSheet
        }
    }

    /// [`Self::click_plays`], opening the output sheet when the map is a
    /// custom one.
    pub(crate) fn press_plays(&mut self, device: usize, slot: usize) {
        if self.click_plays(device, slot) == PlaysClick::OpenSheet {
            self.open_sheet(Sheet::Output { output: device });
        }
    }

    /// Whether the metronome plays on any of output `device`'s channels.
    pub(crate) fn metronome_on(&self, device: usize) -> bool {
        let rows = self.output_device_rows();
        rows.get(device)
            .is_some_and(|row| row.settings.click_mask & low_bits(row.click_channels) != 0)
    }

    /// The *Metronome* toggle of output `device`.  Only the click channels
    /// change.  Off remembers them in `last_click_mask`; on restores that
    /// choice where this output has those channels, or else takes 3 and 4
    /// on four or more channels, or 1 and 2.  A native PipeWire output has
    /// two click channels at most (see [`click_channels`]).
    pub(crate) fn toggle_metronome(&mut self, device: usize) {
        let rows = self.output_device_rows();
        let Some(row) = rows.get(device) else {
            return;
        };
        let width = low_bits(row.click_channels);
        let on = row.settings.click_mask & width != 0;
        let last = self.last_click_mask;
        let Some(settings) = self.device_settings_mut().nth(device) else {
            return;
        };
        if on {
            let mask = settings.click_mask & width;
            settings.click_mask &= !width;
            self.last_click_mask = mask;
        } else {
            let mask = match last & width {
                0 => default_click_mask(row.click_channels),
                kept => kept,
            };
            settings.click_mask = (settings.click_mask & !width) | mask;
        }
    }
}

#[cfg(test)]
mod tests;

/// What a device's map plays, in one line (`docs/ui-spec.md` §6.2), and
/// whether nothing is routed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MapSummary {
    pub(crate) text: String,
    /// No side of the path reaches any of the device's channels.
    pub(crate) nothing_routed: bool,
    /// Nothing routed and unused channels are silent: the output plays
    /// silence.  The only case the sheet and the drawer warn about.
    pub(crate) silent: bool,
}

/// Pairs as *Out 1/2* (*L/R* on a stereo device, *swapped* when reversed),
/// single channels as *Out 3 ← L*, and *rest mono copy* when unreached
/// channels carry the average.  Follows the mapping rule
/// (`ChannelMap::sides_at`), so a one-channel device reads *Out 1 ← mono*
/// for any non-empty map.
pub(crate) fn map_summary(
    settings: &OutputDeviceSettings,
    path: usize,
    channels: usize,
) -> MapSummary {
    let channels = channels.min(MAX_OUTPUT_CHANNELS);
    let map = settings.map(path);
    let copy = settings.unused == UnusedChannels::MonoCopy;
    if !map.reaches_any(channels) {
        return MapSummary {
            text: if copy {
                "nothing routed · mono copy on all".to_owned()
            } else {
                "nothing routed · plays silence".to_owned()
            },
            nothing_routed: true,
            silent: !copy,
        };
    }
    let sides = |channel: usize| map.sides_at(channels, channel);
    let mut parts = Vec::new();
    let mut unreached = false;
    let mut channel = 0;
    while channel < channels {
        if channel % 2 == 0 && channel + 1 < channels {
            let pair = (sides(channel), sides(channel + 1));
            let name = if channels == 2 {
                "L/R".to_owned()
            } else {
                format!("Out {}/{}", channel + 1, channel + 2)
            };
            if pair == ((true, false), (false, true)) {
                parts.push(name);
                channel += 2;
                continue;
            }
            if pair == ((false, true), (true, false)) {
                parts.push(format!("{name} swapped"));
                channel += 2;
                continue;
            }
        }
        let out = channel + 1;
        match sides(channel) {
            (true, true) => parts.push(format!("Out {out} ← mono")),
            (true, false) => parts.push(format!("Out {out} ← L")),
            (false, true) => parts.push(format!("Out {out} ← R")),
            (false, false) => unreached = true,
        }
        channel += 1;
    }
    if unreached && copy {
        parts.push("rest mono copy".to_owned());
    }
    MapSummary {
        text: parts.join(" · "),
        nothing_routed: false,
        silent: false,
    }
}

/// One edit of an output device's settings, asked for while drawing and
/// applied after it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum MapEdit {
    /// Turn one matrix cell of a path on or off.
    Cell {
        slot: usize,
        side: MixSide,
        channel: usize,
        on: bool,
    },
    /// A preset: the path's whole map.
    Map { slot: usize, map: ChannelMap },
    /// The path's level on the device; `commit` on a click or at the end of
    /// a drag (only then is it logged).
    Level {
        slot: usize,
        level_db: f32,
        commit: bool,
    },
    /// The path's polarity on the device.
    Invert { slot: usize, invert: bool },
    /// The path's alignment trim on the device, ms.
    Trim {
        slot: usize,
        trim_ms: f32,
        commit: bool,
    },
    /// The device's align switch.
    Align(bool),
    /// What the device's unreached channels play.
    Unused(UnusedChannels),
}

impl RigWerkApp {
    /// The stored settings of output `device` (0 is the clock output).
    pub(crate) fn output_settings_mut(
        &mut self,
        device: usize,
    ) -> Option<&mut OutputDeviceSettings> {
        if device == 0 {
            Some(&mut self.clock_output)
        } else {
            self.extra_outputs
                .get_mut(device - 1)
                .map(|extra_output| &mut extra_output.settings)
        }
    }

    /// Applies one edit of output `device` and logs it: a map edit with the
    /// device's new map, a level or trim only when committed, polarity and
    /// the align switch on their own lines.
    pub(crate) fn apply_map_edit(&mut self, device: usize, edit: MapEdit) {
        let Some(row) = self.output_device_rows().into_iter().nth(device) else {
            return;
        };
        let multi = self.path_count() > 1;
        let path_name = |app: &Self, slot: usize| {
            app.paths
                .get(slot)
                .and_then(Option::as_ref)
                .map_or_else(String::new, |path| path.name.clone())
        };
        let edit_entry = |app: &mut Self, slot: usize, change: &dyn Fn(&mut PathMapSettings)| {
            let Some(mut entry) = app.device_path_map(device, slot) else {
                return false;
            };
            change(&mut entry);
            app.set_device_path_map(device, slot, entry);
            true
        };
        match edit {
            MapEdit::Cell {
                slot,
                side,
                channel,
                on,
            } => {
                if !edit_entry(self, slot, &|entry| entry.map.set(side, channel, on)) {
                    return;
                }
                self.log_map(device, &row.name, row.channels, slot, multi);
            }
            MapEdit::Map { slot, map } => {
                if !edit_entry(self, slot, &|entry| entry.map = map) {
                    return;
                }
                self.log_map(device, &row.name, row.channels, slot, multi);
            }
            MapEdit::Level {
                slot,
                level_db,
                commit,
            } => {
                if !edit_entry(self, slot, &|entry| entry.level_db = level_db) || !commit {
                    return;
                }
                let level = self
                    .device_path_map(device, slot)
                    .map_or(level_db, |entry| entry.level_db);
                self.push_log(
                    format!(
                        "{} level on {}: {}.",
                        path_name(self, slot),
                        row.name,
                        level_text(level)
                    ),
                    crate::app::LogTone::Teal,
                );
            }
            MapEdit::Invert { slot, invert } => {
                if !edit_entry(self, slot, &|entry| entry.invert = invert) {
                    return;
                }
                self.push_log(
                    format!(
                        "{} polarity on {}: {}.",
                        path_name(self, slot),
                        row.name,
                        if invert { "flipped" } else { "normal" }
                    ),
                    if invert {
                        crate::app::LogTone::Amber
                    } else {
                        crate::app::LogTone::Teal
                    },
                );
            }
            MapEdit::Trim {
                slot,
                trim_ms,
                commit,
            } => {
                if !edit_entry(self, slot, &|entry| entry.trim_ms = trim_ms) || !commit {
                    return;
                }
                let trim = self
                    .device_path_map(device, slot)
                    .map_or(trim_ms, |entry| entry.trim_ms);
                self.push_log(
                    format!(
                        "{} trim on {}: {trim:+.1} ms.",
                        path_name(self, slot),
                        row.name
                    ),
                    crate::app::LogTone::Teal,
                );
            }
            MapEdit::Align(align) => {
                self.set_device_align(device, align);
                self.push_log(
                    format!(
                        "{}: alignment {}.",
                        row.name,
                        if align {
                            "on, every path waits for the slowest"
                        } else {
                            "off, each path plays as its chain finishes"
                        }
                    ),
                    if align {
                        crate::app::LogTone::Teal
                    } else {
                        crate::app::LogTone::Amber
                    },
                );
            }
            MapEdit::Unused(mode) => {
                if let Some(settings) = self.output_settings_mut(device) {
                    settings.unused = mode;
                }
                self.push_log(
                    format!(
                        "{}: unused channels {}.",
                        row.name,
                        match mode {
                            UnusedChannels::Silent => "silent",
                            UnusedChannels::MonoCopy => "play a mono copy",
                        }
                    ),
                    crate::app::LogTone::Teal,
                );
            }
        }
    }

    /// Logs a path's map on a device after an edit.
    fn log_map(&mut self, device: usize, name: &str, channels: usize, slot: usize, multi: bool) {
        let Some(settings) = self.device_settings().get(device).copied().copied() else {
            return;
        };
        let summary = map_summary(&settings, slot, channels);
        let prefix = match self.paths.get(slot).and_then(Option::as_ref) {
            Some(path) if multi => format!("{name}: {}", path.name),
            _ => name.to_owned(),
        };
        self.push_log(
            format!("{prefix}: {}.", summary.text),
            if summary.silent {
                crate::app::LogTone::Amber
            } else {
                crate::app::LogTone::Teal
            },
        );
    }
}

/// A level as the log writes it: one decimal and the real minus.
fn level_text(db: f32) -> String {
    let tenths = (db * 10.0).round();
    if tenths == 0.0 {
        "0.0 dB".to_owned()
    } else if tenths < 0.0 {
        format!("−{:.1} dB", -tenths / 10.0)
    } else {
        format!("+{:.1} dB", tenths / 10.0)
    }
}

impl RigWerkApp {
    /// Whether the clock output is a native PipeWire node (in-graph extra
    /// outputs and RigWerk Out).
    pub(crate) fn pipewire_extra_outputs_enabled(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            crate::audio::is_native_id(&self.selected_output().id)
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }

    /// Whether extra outputs can be added for the current clock.
    pub(crate) fn extra_outputs_enabled(&self) -> bool {
        cfg!(any(target_os = "linux", target_os = "windows"))
    }

    /// Why add is disabled, or `None` when it is enabled.
    pub(crate) fn add_output_disabled_reason(&self) -> Option<&'static str> {
        if !self.extra_outputs_enabled() {
            Some("Extra output devices are not available on this platform")
        } else if self.extra_outputs.len() >= MAX_EXTRA_OUTPUTS {
            Some(ADD_DEVICE_HINT_FULL)
        } else {
            None
        }
    }

    /// Whether the clock output is an ASIO driver (extra outputs are still
    /// WASAPI-only).
    pub(crate) fn clock_is_asio(&self) -> bool {
        self.selected_output().asio_clsid.is_some()
    }

    /// Removes an extra output live; the bridge keeps running.
    pub(crate) fn remove_extra_output(&mut self, index: usize) {
        if index < self.extra_outputs.len() {
            self.extra_outputs.remove(index);
            // The origins are indexed by output, the clock first: removing
            // one shifts every later extra output down.
            let slot = index + 1;
            for offset in slot..MAX_EXTRA_OUTPUTS {
                self.output_trim_origins[offset] = self.output_trim_origins[offset + 1];
            }
            self.output_trim_origins[MAX_EXTRA_OUTPUTS] = OutputTrimOrigin::None;
            self.chain_selected = self
                .chain_selected
                .clamp_output(self.output_device_rows().len());
        }
    }

    /// Adds a hardware extra output by inventory index, or RigWerk Out.
    pub(crate) fn add_extra_output_device(&mut self, device_id: &str) {
        if self.add_output_disabled_reason().is_some() {
            return;
        }
        if device_id == VIRTUAL_OUTPUT_ID || device_id.starts_with("vbcable:") {
            if self.extra_outputs.iter().any(|extra| extra.virtual_output) {
                return;
            }
            #[cfg(target_os = "linux")]
            {
                if !self.pipewire_extra_outputs_enabled() {
                    return;
                }
                let mut extra_output = ExtraOutputPref::virtual_output();
                // A new device plays what the clock output plays, path by
                // path, instead of every path summed on its default map.
                extra_output.settings.path_map = self.clock_output.path_map;
                self.extra_outputs.push(extra_output);
            }
            #[cfg(target_os = "windows")]
            {
                let Some(cable) = self
                    .inventory
                    .outputs
                    .iter()
                    .find(|device| crate::audio::is_vb_cable_render(&device.name))
                    .cloned()
                else {
                    return;
                };
                self.extra_outputs.push(ExtraOutputPref {
                    id: cable.id,
                    backend: cable.backend,
                    name: VIRTUAL_OUTPUT_NAME.to_owned(),
                    virtual_output: true,
                    virtual_output_on: true,
                    // Plays what the clock output plays, path by path.
                    settings: crate::audio::OutputDeviceSettings {
                        path_map: self.clock_output.path_map,
                        ..Default::default()
                    },
                    ..ExtraOutputPref::default()
                });
            }
            #[cfg(not(any(target_os = "linux", target_os = "windows")))]
            {
                let _ = device_id;
                return;
            }
        } else {
            if self.extra_outputs.iter().any(|extra| extra.id == device_id)
                || self.selected_output().id == device_id
            {
                return;
            }
            let Some(device) = self
                .inventory
                .outputs
                .iter()
                .find(|device| device.id == device_id)
                .cloned()
            else {
                return;
            };
            #[cfg(target_os = "linux")]
            {
                let pw_clock = self.pipewire_extra_outputs_enabled();
                let is_pw = crate::audio::linux_node_name(&device.id).is_some();
                // PipeWire clock: only PW sinks (in-graph).  ALSA/cpal clock:
                // only generic (non-native) devices on their own streams.
                if pw_clock != is_pw {
                    return;
                }
            }
            #[cfg(target_os = "windows")]
            {
                // WASAPI shared-mode extra outputs only (no ASIO / DS / MME).
                if device.asio_clsid.is_some() || !device.backend.eq_ignore_ascii_case("WASAPI") {
                    return;
                }
            }
            let settings = crate::audio::OutputDeviceSettings {
                path_map: self.clock_output.path_map,
                ..Default::default()
            };
            self.extra_outputs.push(ExtraOutputPref {
                id: device.id,
                backend: device.backend,
                name: device.name,
                settings,
                ..ExtraOutputPref::default()
            });
        }
        let index = self.extra_outputs.len(); // clock + this extra output
        self.chain_selected = ChainSelection::OutputDevice(index);
    }
}
