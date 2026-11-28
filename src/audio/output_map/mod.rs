//! Per-device map after the master.  A channel both sides reach gets
//! `(L + R) / 2`, not the sum.  A channel no live path reaches is silent,
//! or under `MonoCopy` the mono copy of every path that reaches the
//! device.  Paths add per channel.  Unity copies.  Mute does not move
//! alignment: routing counts muted paths.
//!
//! The safety ceiling is after the device level and before mute, with no
//! lookahead.  The metronome is after the ceiling; a zero tick writes
//! nothing, and the lanes it touched have their own limiter so a hot tick
//! does not duck the others.  The click-free copy, taken before the tick,
//! is what a take records.  The tick waits the delay a dry, untrimmed path
//! would get, so a trim does not move it.  A measurement ducks it after
//! that wait.
//!
//! Each device taps the shared path lines.  The delay is
//! `max(latency) - latency(p) + trim`, shifted so the smallest is 0.
//! With align off, only the trims apply.  A change crossfades.

mod align;
mod mapper;
mod types;
mod weights;

pub use align::AlignmentReport;
pub(crate) use align::MAX_ALIGN_DELAY;
pub(crate) use mapper::{
    frame_lanes, DeviceControls, DeviceFrame, DeviceMapper, PathLines, PathTiming,
    SLOTS as FRAME_LANES,
};
pub(crate) use types::low_bits;
pub use types::{
    ChannelMap, MapPreset, MixSide, OutputDeviceSettings, PathMapSettings, UnusedChannels,
    DEVICE_LEVEL_MAX_DB, DEVICE_LEVEL_MIN_DB, LOUDNESS_TARGET_DEFAULT_LUFS,
    LOUDNESS_TARGET_MAX_LUFS, LOUDNESS_TARGET_MIN_LUFS, LOUDNESS_TRIM_MAX_DB, MAX_EXTRA_OUTPUTS,
    MAX_OUTPUT_CHANNELS, PATH_TRIM_MAX_MS, VIRTUAL_OUTPUT_ID, VIRTUAL_OUTPUT_NAME,
};
pub(in crate::audio) use weights::level_gain;

#[cfg(test)]
pub(crate) use mapper::old_write_frame;

#[cfg(test)]
mod tests;
