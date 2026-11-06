//! WASAPI clock output: REAL-style shared minimum step, and exclusive mode.
//!
//! Followers stay on shared cpal streams; only the bridge clock device uses
//! these paths.

mod exclusive;
mod real;

pub use exclusive::{open_exclusive, ExclusiveOutput};
pub use real::{open, probe, RealOutput};

// Named in `probe`'s signature and in hardware tests; keep reachable.
#[allow(unused_imports)]
pub use real::RealInfo;
