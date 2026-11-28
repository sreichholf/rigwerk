//! How much of a path's `L`, `R` and `(L + R) / 2` a device channel
//! carries.  Exactly one weight is 1, or none when the channel is silent,
//! so a settled map copies.

use super::super::path::MAX_PATHS;
use super::{ChannelMap, UnusedChannels, MAX_OUTPUT_CHANNELS};

pub(super) fn routed_paths(
    maps: &[ChannelMap; MAX_PATHS],
    live: [bool; MAX_PATHS],
    unused: UnusedChannels,
    channels: usize,
) -> [bool; MAX_PATHS] {
    let last = channels.min(MAX_OUTPUT_CHANNELS);
    std::array::from_fn(|path| {
        (0..last).any(|channel| {
            path_channel_weights(maps, live, unused, channels, channel, path) != [0.0; 3]
        })
    })
}

/// 0 dB is exactly 1, so the default level leaves a sample untouched.
pub(in crate::audio) fn level_gain(level_db: f32) -> f32 {
    if level_db == 0.0 {
        1.0
    } else {
        10.0f32.powf(level_db / 20.0)
    }
}

/// Exactly one weight is 1, or none, so a settled map copies.
pub(super) fn channel_weights(
    map: ChannelMap,
    unused: UnusedChannels,
    channels: usize,
    channel: usize,
) -> [f32; 3] {
    match map.sides_at(channels, channel) {
        (true, false) => [1.0, 0.0, 0.0],
        (false, true) => [0.0, 1.0, 0.0],
        (true, true) => [0.0, 0.0, 1.0],
        (false, false) => match unused {
            UnusedChannels::MonoCopy => [0.0, 0.0, 1.0],
            UnusedChannels::Silent => [0.0, 0.0, 0.0],
        },
    }
}

/// A channel no live path reaches is silent, or under `MonoCopy` the
/// mono copy of every path that reaches the device (or of every path,
/// when none does).  One live path is [`channel_weights`].
pub(super) fn path_channel_weights(
    maps: &[ChannelMap; MAX_PATHS],
    live: [bool; MAX_PATHS],
    unused: UnusedChannels,
    channels: usize,
    channel: usize,
    path: usize,
) -> [f32; 3] {
    if !live[path] {
        return [0.0; 3];
    }
    let live_paths = || (0..MAX_PATHS).filter(|&other| live[other]);
    let reached =
        live_paths().any(|other| maps[other].sides_at(channels, channel) != (false, false));
    if reached {
        return channel_weights(maps[path], UnusedChannels::Silent, channels, channel);
    }
    match unused {
        UnusedChannels::Silent => [0.0; 3],
        UnusedChannels::MonoCopy => {
            let mapped = |other: usize| maps[other].reaches_any(channels);
            if mapped(path) || !live_paths().any(mapped) {
                [0.0, 0.0, 1.0]
            } else {
                [0.0; 3]
            }
        }
    }
}
