//! Nothing here is saved.  [`Meters::tick`] is the only clock.

use std::time::Instant;

use crate::audio::{MAX_EXTRA_OUTPUTS, MAX_OUTPUT_CHANNELS, MAX_PATHS, SCOPE_LEN};

/// [`Self::tick`] is the only source of the smoothing interval.
pub(crate) struct Meters {
    pub(crate) in_l: f32,
    pub(crate) in_r: f32,
    pub(crate) out_l: f32,
    pub(crate) out_r: f32,
    pub(crate) peak_in_l: f32,
    pub(crate) peak_in_r: f32,
    pub(crate) peak_out_l: f32,
    pub(crate) peak_out_r: f32,
    pub(crate) out_channels: [f32; MAX_OUTPUT_CHANNELS],
    pub(crate) peak_out_channels: [f32; MAX_OUTPUT_CHANNELS],
    pub(crate) extra_output_channels: [[f32; MAX_OUTPUT_CHANNELS]; MAX_EXTRA_OUTPUTS],
    pub(crate) peak_extra_output_channels: [[f32; MAX_OUTPUT_CHANNELS]; MAX_EXTRA_OUTPUTS],
    /// After the chain, level, mute and solo, before the master.
    /// Distinct from the path gain.
    pub(crate) path_levels: [[f32; 2]; MAX_PATHS],
    pub(crate) peak_path_levels: [[f32; 2]; MAX_PATHS],
    pub(crate) scope_in: Vec<f32>,
    pub(crate) scope_out: Vec<f32>,
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

    /// Clamped so a stalled frame does not jump the meters or a fast
    /// one freeze them.
    pub(crate) fn tick(&mut self) -> f32 {
        let dt = self.last_tick.elapsed().as_secs_f32().clamp(0.001, 0.1);
        self.last_tick = Instant::now();
        dt
    }
}

#[cfg(test)]
mod tests;
