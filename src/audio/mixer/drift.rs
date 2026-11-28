//! One copy of the drift-loop gains, so a tune cannot move one mixer only.
//!
//! The error is seconds of fill and the correction is a ratio.  Each mixer
//! keeps its own ring, priming and buffer-step handling.

/// The most drift steering may trim the conversion ratio.  500 ppm is under
/// one cent of pitch.  Real clock offsets between two interfaces are a few
/// tens of ppm, so the loop normally runs far inside this limit.
pub(crate) const DRIFT_LIMIT_PPM: f32 = 500.0;
/// The same limit in ratio units, for the integral term.
pub(crate) const DRIFT_LIMIT_RATIO: f32 = DRIFT_LIMIT_PPM * 1e-6;
/// Time constant of the low-pass on the ring-fill error.  The fill is read
/// once per output block and jumps by a whole input block whenever the input
/// delivers, so the raw reading carries a sawtooth of up to one input block
/// that says nothing about the clocks.
pub(crate) const DRIFT_FILTER_SECONDS: f32 = 0.5;
/// Proportional gain, in ratio per second of filtered fill error (60 ppm per
/// millisecond).  Low enough that the filtered block sawtooth moves the ratio
/// by tens of ppm, not across the whole range.
pub(crate) const DRIFT_KP: f32 = 0.06;
/// Integral gain, in ratio per second² of filtered fill error.  The integral
/// learns the steady clock offset, so once it has settled the ratio moves
/// only by a few ppm.
pub(crate) const DRIFT_KI: f32 = 0.0012;
/// A target step this large (seconds of ring) saturates the proportional
/// term on its own, so closing it may wind the integral to its limit.  The
/// extra output uses it to tell that wind-up from a clock that is really
/// pinned.
pub(crate) const DRIFT_STEP_SECONDS: f32 = DRIFT_LIMIT_RATIO / DRIFT_KP;

// The fill low-pass makes the closed loop cubic.  Routh–Hurwitz
// stays stable while `T < Kp / Ki`.  The loop runs once per block,
// so this only trips a retune that makes the model ring.
const _: () = assert!(DRIFT_FILTER_SECONDS < DRIFT_KP / DRIFT_KI);

// The resampler clips the ratio at `MAX_CORRECTION_PPM`, so this loop's limit
// must stay within it: raising the limit or tightening the clamp stops the
// build here instead of letting the resampler cut the correction short.
const _: () = assert!(DRIFT_LIMIT_PPM <= crate::audio::resampler::MAX_CORRECTION_PPM);
