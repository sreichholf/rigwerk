//! Shared scope readouts.  The traces are peak-hold bins from capture and
//! playback, drawn by the sheet, not a decorative oscillator.

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
