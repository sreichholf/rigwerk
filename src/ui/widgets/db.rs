//! One decibel readout: signed, with the unit, the real minus sign, and
//! `−∞` at the floor.  A level, gain or trim prints through [`format_db`].

/// A NaN or anything at or below this reads `−∞`.  That is this readout's
/// own floor, not the scale the meters draw.
const FLOOR_DB: f32 = -120.0;
const SILENCE: &str = "−∞";

/// `+3.0 dB`, `−3.0 dB`, `0.0 dB`, `−∞`.  `decimals` is the printed
/// precision; the sign and the unit do not vary.  Values round half away
/// from zero at that precision, so `2.25` reads `+2.3` and a `−30.5`
/// chain threshold reads `−31`.
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
