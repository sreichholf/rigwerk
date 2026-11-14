//! Per-device output settings and the sum matrix: device level, mute,
//! and for every signal path a channel map, a level and a polarity on that
//! device.  Applied after the master (`output_gain` / `mute`).
//!
//! # Mapping rule (one path)
//!
//! For each device channel `c` (0-based), with `L` and `R` the path's mix
//! after the master:
//! - reached by L only: `L`; by R only: `R`; by both: `(L + R) / 2` (the
//!   average, not the sum);
//! - reached by neither: `(L + R) / 2` under [`UnusedChannels::MonoCopy`],
//!   silence under [`UnusedChannels::Silent`];
//! - `c >= MAX_OUTPUT_CHANNELS`: never reached, so treated as unused.
//!
//! A 1-channel device reads any non-empty map as both sides reaching
//! channel 1, so it gets `(L + R) / 2`; an empty map leaves it to
//! [`UnusedChannels`].
//!
//! With the default map (L → 1, R → 2) and `MonoCopy` this is exactly what
//! the output has always written: L and R on channels 1 and 2, their average
//! on every further channel, and the average on a 1-channel device.
//!
//! # The sum of several paths
//!
//! Every device stores one [`PathMapSettings`] per path slot (`MAX_PATHS`):
//! the path's map, its level (dB, ramped) and polarity there (a flip ramps
//! the path's gain through zero, so it never clicks), and an alignment trim
//! (see below).  The device level,
//! mute and the unused rule belong to the device.  The mapper maps each live
//! path's frame with its own map, scales it by its level and polarity on the
//! device and adds the paths up per channel (cells that meet on a channel
//! add, they are not averaged), then applies the device level, the safety
//! ceiling and the mute.
//!
//! A channel no live path reaches follows [`UnusedChannels`]: silent, or
//! under `MonoCopy` the sum of the mono copies `(L + R) / 2` of every live
//! path that reaches some channel of this device.  If no path reaches the
//! device at all, every live path counts, which for one path is the rule
//! above.  So a path routed nowhere on a device (an added, empty path) never
//! leaks into its spare channels, and one path behaves as it always did.
//! Muted and un-soloed paths still count for this: the routing, not what is
//! audible, decides which channels play.
//!
//! One path at 0 dB with normal polarity gives exactly its own mapped frame,
//! bit for bit, while the sample stays under the safety ceiling: unity gains
//! copy instead of multiplying.
//!
//! # Safety ceiling
//!
//! After the device level and before the mute, a sample-peak limiter
//! ([`super::limiter`]) holds any sample above −1.0 dBFS at that ceiling.
//! There is no lookahead, so alignment does not move.  Attack is immediate
//! and the gain returns to unity over a few tens of milliseconds.  A signal
//! that stays under the ceiling is not touched.  The reduction in dB (0 when
//! not limiting) is published beside the device meters.  The same limiter
//! runs for the clock device and every follower, including the virtual
//! publish device, because each of them is mapped here.
//!
//! # Metronome
//!
//! After the ceiling and before the mute, the metronome sample is added
//! to lanes whose click-mask bit is set, and only when that sample is not
//! exactly 0.  A zero sample writes nothing, so a negative zero on an
//! unmapped lane stays a negative zero.  Mask 0 adds the tick nowhere:
//! there is no fallback onto the first channels.  The shared extra lane
//! (channels past [`MAX_OUTPUT_CHANNELS`]) is never written.  Those lanes
//! then pass through their own limiter, so a tick that shares a channel
//! with loud music gains the sum down instead of flattening it, and that
//! reduction is published with the music limiter's.  That limiter runs on
//! the masked lanes between ticks too, so music it gained down comes back
//! on its release, not in one step.  A channel the tick missed keeps the
//! music limiter's gain.  The mute ramp then scales the music and the tick
//! together.  Peak meters include the tick, after the mute.
//!
//! The tick has no chain latency and no trim, so on each device it waits
//! the alignment delay a dry, untrimmed path would get there (see
//! "Alignment"), from a preallocated mono line in the mapper.  Clicks then
//! land with the device's aligned music, a trim on one path does not move
//! them, and an aligned device does not flam.  A change of that delay
//! crossfades like a path's tap while ticks are queued; while the line is
//! silent the tap moves at once, so the next tick plays once.  Once the
//! last tick has left every read position the line is reset and skipped,
//! and with the click limiter released a stopped metronome costs nothing
//! per frame.  A block with frames in which a device was not mapped (a
//! follower whose ring was not fed) resets it too, so ticks from before
//! the gap never play late.  A measurement ducks the tick after that line,
//! on its own ramp, so a tick already waiting does not play into it.
//!
//! # Alignment
//!
//! Plugins delay their path by their latency, so two paths that meet on
//! one device would comb filter.  The mixer writes every live path's frame
//! (after the master) into that path's delay line (one per path slot,
//! preallocated for [`MAX_ALIGN_DELAY`] frames), and each device mapper
//! reads every path through its own tap on that line: memory grows with
//! the paths, not with paths × devices.  The delay of path `p` on device
//! `d` is `max(latency) − latency(p) + trim(p, d)`, the maximum over the
//! paths `d` plays (muted and un-soloed ones included: the routing decides,
//! so muting never moves the latency), shifted so the smallest delay is 0
//! when a trim goes negative.  A path only one device plays keeps its own
//! latency there.  With the device's `align` switch off only the trims
//! apply.  A changed delay (a plugin reports a new latency, a path is
//! routed somewhere else, a trim moves) crossfades from the old tap to the
//! new one over a few ms (`delay::TAP_FADE_SECONDS`), it never jumps.  The
//! same mapping runs for the clock device and every follower, on the ring
//! path and the PipeWire direct path alike: a generic follower is mapped on
//! the clock callback and only its finished channels cross into its own
//! stream (see `follower`).  The mapper publishes what it applies
//! ([`DeviceControls::alignment`]) for the UI.
//!
//! An unaligned path with no trim reads its line at delay 0, which is the
//! frame just written: one path with no latency stays bit-identical.

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
    DEVICE_LEVEL_MAX_DB, DEVICE_LEVEL_MIN_DB, MAX_FOLLOWERS, MAX_OUTPUT_CHANNELS, PATH_TRIM_MAX_MS,
    VIRTUAL_OUT_ID, VIRTUAL_OUT_NAME,
};
pub(in crate::audio) use weights::level_gain;

#[cfg(test)]
pub(crate) use mapper::old_write_frame;

#[cfg(test)]
mod tests;
