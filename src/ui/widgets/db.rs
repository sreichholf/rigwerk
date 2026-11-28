//! One decibel readout, with the real minus, and `−∞` at the floor.

/// A NaN or anything at or below this reads `−∞`.  That is this readout's
/// own floor, not the scale the meters draw.
const FLOOR_DB: f32 = -120.0;
const SILENCE: &str = "−∞";

/// Rounds half away from zero, so `2.25` reads `+2.3`.
pub(crate) fn format_db(db: f32, decimals: usize) -> String {
    if db.is_nan() || db <= FLOOR_DB {
        return SILENCE.to_owned();
    }
    // A positive infinity is a level above any range the app has.
    let db = if db.is_finite() { db } else { -FLOOR_DB };
    let scale = 10f32.powi(decimals as i32);
    let scaled = (db * scale).round();
    if scaled == 0.0 {
        return format!("{:.decimals$} dB", 0.0f32);
    }
    let magnitude = scaled.abs() / scale;
    let sign = if scaled < 0.0 { '−' } else { '+' };
    format!("{sign}{magnitude:.decimals$} dB")
}

#[cfg(test)]
mod tests;
