//! Routing layouts (the starting points of the sum matrix).
//!
//! A layout sets the input routing of the first paths (in display order) and
//! their map, level and polarity on every output device.  It never removes
//! a path or an effect: missing paths are added (empty, or for Blend and
//! Dual-amp as a copy of the first path), and paths past the layout's count
//! are kept but routed nowhere.  Any edit afterwards reads as Custom
//! ([`RigWerkApp::current_layout`] returns `None`).
//!
//! Blend and Dual-amp sum two paths on every channel; the per-device
//! latency alignment (on by default, `app::alignment`) lines them up, so a
//! latent amp sim on one of them does not comb filter.

use crate::app::LogTone;
use crate::app::RigWerkApp;
use crate::audio::{ChannelMap, PathMapSettings};
/// The input routing that keeps IN 1 on L and IN 2 on R.
const STEREO_IN: [[bool; 4]; 2] = [[true, false, false, false], [false, true, false, false]];
/// IN 1 on both L and R (a mono guitar).
const MONO_IN: [[bool; 4]; 2] = [[true, true, false, false], [false, false, false, false]];

/// One routing layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PathLayout {
    /// One path, L/R on Out 1/2: today's routing.
    Single,
    /// Path 1 on the left channels, path 2 on the right; both on RigWerk
    /// Out.  No sum.
    DryWet,
    /// Clean path at −6 dB under a driven one, summed on every device.
    Blend,
    /// Two amp paths summed, the second with its polarity flipped.
    DualAmp,
    /// Path 1 on the clock output, path 2 on every extra output.
    PerDevice,
}

impl PathLayout {
    pub(crate) const ALL: [PathLayout; 5] = [
        PathLayout::Single,
        PathLayout::DryWet,
        PathLayout::Blend,
        PathLayout::DualAmp,
        PathLayout::PerDevice,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            PathLayout::Single => "Single path",
            PathLayout::DryWet => "Dry L / Wet R",
            PathLayout::Blend => "Blend clean",
            PathLayout::DualAmp => "Dual-amp",
            PathLayout::PerDevice => "Per-device mixes",
        }
    }

    /// Layouts that sum paths on one channel (they rely on the latency
    /// alignment, on by default per device).  Their missing second path
    /// starts as a copy of the first.
    pub(crate) fn sums_paths(self) -> bool {
        matches!(self, PathLayout::Blend | PathLayout::DualAmp)
    }

    /// How many paths the layout sets up.
    pub(crate) fn path_count(self) -> usize {
        match self {
            PathLayout::Single => 1,
            _ => 2,
        }
    }

    /// The one-line summary beside the name in a menu.
    pub(crate) fn caption(self) -> &'static str {
        match self {
            PathLayout::Single => "today · one chain",
            PathLayout::DryWet => "split · no sum",
            PathLayout::Blend => "sum · clean under drive",
            PathLayout::DualAmp => "sum · two amps",
            PathLayout::PerDevice => "each device own path",
        }
    }

    fn input_routes(self) -> [[bool; 4]; 2] {
        match self {
            PathLayout::Single | PathLayout::PerDevice => STEREO_IN,
            PathLayout::DryWet | PathLayout::Blend | PathLayout::DualAmp => MONO_IN,
        }
    }

    /// Layout path `path`'s map, level and polarity on a device; `None` past
    /// the layout's paths (routed nowhere).
    fn entry(self, path: usize, device: &DeviceShape) -> (ChannelMap, f32, bool) {
        let stereo = ChannelMap::default();
        let none = ChannelMap { left: 0, right: 0 };
        if path >= self.path_count() {
            return (none, 0.0, false);
        }
        match self {
            PathLayout::Single => (stereo, 0.0, false),
            PathLayout::DryWet if device.virtual_output => (stereo, 0.0, false),
            PathLayout::DryWet => {
                let (left, right) = if device.channels >= 4 {
                    (0b0101, 0b1010)
                } else {
                    (0b01, 0b10)
                };
                if path == 0 {
                    (ChannelMap { left, right: 0 }, 0.0, false)
                } else {
                    (ChannelMap { left: 0, right }, 0.0, false)
                }
            }
            PathLayout::Blend => (stereo, if path == 0 { -6.0 } else { 0.0 }, false),
            PathLayout::DualAmp => (stereo, 0.0, path == 1),
            PathLayout::PerDevice => {
                let plays = if device.clock { path == 0 } else { path == 1 };
                (if plays { stereo } else { none }, 0.0, false)
            }
        }
    }
}

/// The tag shown when the routing matches no layout.
pub(crate) fn custom_label(last: Option<PathLayout>) -> String {
    match last {
        Some(layout) => format!("Custom · edited from {}", layout.label()),
        None => "Custom".to_owned(),
    }
}

/// What a layout needs to know about one output device.
struct DeviceShape {
    channels: usize,
    clock: bool,
    virtual_output: bool,
}

impl RigWerkApp {
    fn device_shapes(&self) -> Vec<DeviceShape> {
        self.output_device_rows()
            .iter()
            .map(|row| DeviceShape {
                channels: row.channels,
                clock: row.clock,
                virtual_output: row.virtual_output,
            })
            .collect()
    }

    /// Applies `layout`: input routing of its paths and every path's map,
    /// level and polarity on every device (trims stay).  Adds the paths it
    /// is missing, playing (unmuted); never removes a path or an effect.
    /// `false` if a path it needs could not be added.
    pub(crate) fn apply_layout(&mut self, layout: PathLayout) -> bool {
        let selected = self.selected_path;
        while self.path_count() < layout.path_count() {
            let first = self.path_slots()[0];
            let added = if layout.sums_paths() {
                self.duplicate_path(first)
            } else {
                self.add_path()
            };
            let Some(slot) = added else {
                return false;
            };
            self.set_path_mute(slot, false);
        }
        self.select_path(selected);
        let slots = self.path_slots();
        let shapes = self.device_shapes();
        for (position, &slot) in slots.iter().enumerate() {
            if position < layout.path_count() {
                self.set_path_routes(slot, layout.input_routes());
            }
        }
        for (settings, shape) in self.device_settings_mut().zip(&shapes) {
            for (position, &slot) in slots.iter().enumerate() {
                let (map, level_db, invert) = layout.entry(position, shape);
                settings.path_map[slot] = PathMapSettings {
                    map,
                    level_db,
                    invert,
                    ..settings.path_map[slot]
                };
            }
        }
        self.push_paths_to_engine();
        self.push_devices_to_engine();
        self.routing_last_layout = Some(layout);
        self.push_log(
            format!("Routing layout: {}.", layout.label()),
            LogTone::Teal,
        );
        true
    }

    /// The layout the routing matches exactly, or `None` (Custom).
    pub(crate) fn current_layout(&self) -> Option<PathLayout> {
        let slots = self.path_slots();
        let shapes = self.device_shapes();
        let devices = self.device_settings();
        PathLayout::ALL.into_iter().find(|&layout| {
            slots.len() >= layout.path_count()
                && slots.iter().enumerate().all(|(position, &slot)| {
                    let routes_match = position >= layout.path_count()
                        || self.paths[slot]
                            .as_ref()
                            .is_some_and(|path| path.mix.routes == layout.input_routes());
                    routes_match
                        && devices.iter().zip(&shapes).all(|(settings, shape)| {
                            let entry = settings.path_map[slot];
                            layout.entry(position, shape)
                                == (entry.map, entry.level_db, entry.invert)
                        })
                })
        })
    }
}

#[cfg(test)]
mod tests;
