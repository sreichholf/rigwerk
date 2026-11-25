//! The drift loop's constants, shared by the clock output and every extra
//! output.
//!
//! [`OutputMixer`](super::OutputMixer) and the
//! [`extra_output`](super::super::extra_output) mixer each run their own PI
//! loop, on their own ring, in the same units: the error is
//! seconds of fill, the correction is a ratio.  The limit, the low-pass and
//! the gains are the same loop and live here once, so a tune cannot move one
//! copy only.  What differs stays with each mixer: the ring it reads, the
//! priming state, and the extra output's step that a new bridge buffer takes.

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

// The fill low-pass makes the closed loop cubic: T·s³ + s² + Kp·s + Ki with
// T = `DRIFT_FILTER_SECONDS`.  Routh–Hurwitz keeps every root left of the
// imaginary axis while `T < Kp / Ki`, and the model rings past that.  The
// loop itself runs once per block, so this is the continuous condition as a
// coarse tripwire and nothing more: not a discrete-time stability proof, not
// a margin, not a settling-time claim, only that a retune has not driven the
// model to ring outright.
const _: () = assert!(DRIFT_FILTER_SECONDS < DRIFT_KP / DRIFT_KI);

// The resampler clips the ratio at `MAX_CORRECTION_PPM`, so this loop's limit
// must stay within it: raising the limit or tightening the clamp stops the
// build here instead of letting the resampler cut the correction short.
const _: () = assert!(DRIFT_LIMIT_PPM <= crate::audio::resampler::MAX_CORRECTION_PPM);
