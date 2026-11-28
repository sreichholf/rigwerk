//! Only the bridge clock output.  Extra outputs stay on shared cpal streams.

mod exclusive;
mod real;

pub use exclusive::{open_exclusive, ExclusiveOutput};
pub use real::{open, probe, RealOutput};

// Named in `probe`'s signature and in hardware tests; keep reachable.
#[allow(unused_imports)]
pub use real::RealInfo;
