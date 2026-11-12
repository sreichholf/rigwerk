//! The output devices under the master, as plain rows.
//!
//! Mixer, Routing, and Live Issues read the same list.  A row is names,
//! flags, and settings.  The colour of a device's state stays in `ui`.

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
    /// The device whose callback runs the chain.
    pub(crate) clock: bool,
    /// RigWerk Out virtual source.
    pub(crate) virtual_out: bool,
    /// Index into `followers` when this is not the clock device.
    pub(crate) follower_index: Option<usize>,
    /// Device id missing from the current inventory.
    pub(crate) missing: bool,
    pub(crate) publish: bool,
    pub(crate) settings: OutputDeviceSettings,
    /// Safety-limiter gain reduction, dB.  0 when the device is not
    /// limiting, and when the bridge is not running.
    pub(crate) reduction_db: f32,
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

impl RigWerkApp {
    /// The output devices under the master, clock device first, then followers.
    pub(crate) fn output_device_rows(&self) -> Vec<OutputDeviceRow> {
        let output = self.selected_output();
        let reported = if self.engine.state == EngineState::Running {
            self.snapshot.output_channels
        } else {
            0
        };
        let running = self.engine.state == EngineState::Running;
        let mut rows = vec![OutputDeviceRow {
            name: output.name.clone(),
            backend: output.backend.clone(),
            channels: device_channel_count(reported, output.channels),
            clock: true,
            virtual_out: false,
            follower_index: None,
            missing: false,
            publish: false,
            settings: self.clock_device,
            reduction_db: if running {
                self.snapshot.out_reduction_db
            } else {
                0.0
            },
        }];
        for (index, follower) in self.followers.iter().enumerate() {
            let (channels, missing) = if follower.virtual_out {
                (2, false)
            } else {
                let found = self
                    .inventory
                    .outputs
                    .iter()
                    .find(|device| device.id == follower.id);
                let channels = if self.engine.state == EngineState::Running {
                    let count = self
                        .snapshot
                        .follower_channel_counts
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
                name: if follower.name.is_empty() {
                    follower.id.clone()
                } else {
                    follower.name.clone()
                },
                backend: follower.backend.clone(),
                channels,
                clock: false,
                virtual_out: follower.virtual_out,
                follower_index: Some(index),
                missing,
                publish: follower.publish,
                settings: follower.settings,
                reduction_db: if running {
                    self.snapshot
                        .follower_reduction_db
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
}

#[cfg(test)]
mod tests;
