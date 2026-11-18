//! The output devices under the master, as plain rows.
//!
//! Mixer, Routing, and Live Issues read the same list.  A row is names,
//! flags, and settings.  The colour of a device's state stays in `ui`.
//! [`DeviceLevel`] is one row's live meter, from the smoothed per-channel
//! meters the bridge updates each frame (`app/bridge.rs`).
//! [`ExtraOutputReading`] is what an extra output's own stream really runs at,
//! from the live snapshot.

use crate::app::RigWerkApp;
use crate::audio::{EngineState, OutputDeviceSettings, MAX_OUTPUT_CHANNELS};

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
#[cfg_attr(not(test), allow(dead_code))] // For the output strip and sheet.
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
    #[cfg_attr(not(test), allow(dead_code))] // For the output strip and sheet.
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
}

#[cfg(test)]
mod tests;
