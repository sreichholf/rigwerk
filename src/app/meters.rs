//! The smoothed levels, peaks and scopes the screens draw.
//!
//! One pass per frame writes all of it:
//! [`RigWerkApp::smooth_meters`](crate::app::RigWerkApp::smooth_meters),
//! from the snapshot while the engine runs and from the preview stand-in
//! otherwise, with the scope copied from the engine's lanes.  Nothing here is
//! saved, and a saved session never restores it.

use std::time::Instant;

use crate::audio::{MAX_EXTRA_OUTPUTS, MAX_OUTPUT_CHANNELS, MAX_PATHS, SCOPE_LEN};

/// The levels, peaks and scope traces for the console and the sheets.
///
/// The fields are written together by the one pass named in the module doc;
/// nothing here is persisted.  The tick is owned rather than read: [`Self::tick`]
/// is the only source of the interval the smoothing and the peak decay use,
/// so a caller cannot smooth against a different clock.
pub(crate) struct Meters {
    pub(crate) in_l: f32,
    pub(crate) in_r: f32,
    pub(crate) out_l: f32,
    pub(crate) out_r: f32,
    pub(crate) peak_in_l: f32,
    pub(crate) peak_in_r: f32,
    pub(crate) peak_out_l: f32,
    pub(crate) peak_out_r: f32,
    /// Smoothed meter and peak per clock-device channel, from
    /// [`Snapshot::out_channels`](crate::audio::Snapshot::out_channels).
    pub(crate) out_channels: [f32; MAX_OUTPUT_CHANNELS],
    pub(crate) peak_out_channels: [f32; MAX_OUTPUT_CHANNELS],
    /// Smoothed meter and peak per extra output channel, from
    /// [`Snapshot::extra_output_channels`](crate::audio::Snapshot::extra_output_channels).
    pub(crate) extra_output_channels: [[f32; MAX_OUTPUT_CHANNELS]; MAX_EXTRA_OUTPUTS],
    pub(crate) peak_extra_output_channels: [[f32; MAX_OUTPUT_CHANNELS]; MAX_EXTRA_OUTPUTS],
    /// Per path slot (L, R): after the path's chain, level, mute and solo,
    /// before the master.  Smoothed and peak-held like the other meters, and
    /// distinct from the gain the path applies (`path_gains`).
    pub(crate) path_levels: [[f32; 2]; MAX_PATHS],
    pub(crate) peak_path_levels: [[f32; 2]; MAX_PATHS],
    /// Decimated capture/playback waveform for the Live scope.
    pub(crate) scope_in: Vec<f32>,
    pub(crate) scope_out: Vec<f32>,
    /// When the last tick was taken.  Private so [`Self::tick`] is the only
    /// way the smoothing interval moves.
    last_tick: Instant,
}

impl Meters {
    pub(crate) fn new() -> Self {
        Self {
            in_l: 0.0,
            in_r: 0.0,
            out_l: 0.0,
            out_r: 0.0,
            peak_in_l: 0.0,
            peak_in_r: 0.0,
            peak_out_l: 0.0,
            peak_out_r: 0.0,
            out_channels: [0.0; MAX_OUTPUT_CHANNELS],
            peak_out_channels: [0.0; MAX_OUTPUT_CHANNELS],
            extra_output_channels: [[0.0; MAX_OUTPUT_CHANNELS]; MAX_EXTRA_OUTPUTS],
            peak_extra_output_channels: [[0.0; MAX_OUTPUT_CHANNELS]; MAX_EXTRA_OUTPUTS],
            path_levels: [[0.0; 2]; MAX_PATHS],
            peak_path_levels: [[0.0; 2]; MAX_PATHS],
            scope_in: vec![0.0; SCOPE_LEN],
            scope_out: vec![0.0; SCOPE_LEN],
            last_tick: Instant::now(),
        }
    }

    /// The seconds since the previous tick, clamped so a stalled frame does
    /// not make the meters jump or a fast one freeze them, and restarts the
    /// clock for the next pass.
    pub(crate) fn tick(&mut self) -> f32 {
        let dt = self.last_tick.elapsed().as_secs_f32().clamp(0.001, 0.1);
        self.last_tick = Instant::now();
        dt
    }
}

#[cfg(test)]
mod tests;
