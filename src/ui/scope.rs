//! Scope readouts shared with the scope sheet: amplitudes as dBFS and
//! compact sample counts.
//!
//! The traces themselves are drawn by the sheet (`ui/sheets/scope.rs`); the
//! curves are decimated samples from the capture and playback paths
//! (peak-hold bins), not a decorative oscillator.

pub(crate) fn amp_to_db(value: f32) -> f32 {
    if value <= 0.0001 {
        -60.0
    } else {
        (20.0 * value.log10()).clamp(-60.0, 0.0)
    }
}

pub(crate) fn format_compact(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}M", value as f32 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}k", value as f32 / 1_000.0)
    } else {
        format!("{value}")
    }
}

#[cfg(test)]
mod tests;
