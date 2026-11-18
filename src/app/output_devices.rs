//! The output devices under the master, as plain rows.
//!
//! Mixer, Routing, and Live Issues read the same list.  A row is names,
//! flags, and settings.  The colour of a device's state stays in `ui`.
//! [`DeviceLevel`] is one row's live meter, from the smoothed per-channel
//! meters the bridge updates each frame (`app/bridge.rs`).
//! [`ExtraOutputReading`] is what an extra output's own stream really runs at,
//! from the live snapshot.

use crate::app::latency::output_mode_phrase;
use crate::app::screen::{format_ms, Sheet};
use crate::app::RigWerkApp;
use crate::audio::{
    low_bits, ChannelMap, EngineState, MapPreset, OutputDeviceSettings, DEVICE_LEVEL_MAX_DB,
    DEVICE_LEVEL_MIN_DB, MAX_OUTPUT_CHANNELS, MAX_PATHS,
};

/// One output device as the Mixer lists it.
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
}

/// One output device's live level, loudest channel, as the Live strip shows it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DeviceLevel {
    pub(crate) name: String,
    /// Smoothed and peak-held linear amplitude, as `meter_row` takes them.
    pub(crate) meter: f32,
    pub(crate) peak: f32,
    /// The device's own mute.  The master mute already zeroes the meters.
    pub(crate) mute: bool,
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

/// Channels the Mixer offers for a click mask.  A native PipeWire node
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
            });
        }
        rows
    }

    /// Each output device's live level, in [`Self::output_device_rows`] order.
    pub(crate) fn output_device_levels(&self) -> Vec<DeviceLevel> {
        self.output_device_rows()
            .into_iter()
            .map(|row| {
                let (meters, peaks): (&[f32], &[f32]) = match row.extra_output_index {
                    None => (&self.meter_out_channels, &self.peak_out_channels),
                    Some(index) => (
                        self.meter_extra_output_channels
                            .get(index)
                            .map_or(&[], |meters| meters),
                        self.peak_extra_output_channels
                            .get(index)
                            .map_or(&[], |peaks| peaks),
                    ),
                };
                DeviceLevel {
                    meter: loudest(meters, row.channels),
                    peak: loudest(peaks, row.channels),
                    mute: row.settings.mute,
                    name: row.name,
                }
            })
            .collect()
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
