//! A layout never removes a path or an effect.  Missing paths are added
//! (empty, or a copy of the first for Blend and Dual-amp); paths past
//! its count stay but route nowhere.  Any later edit reads as Custom.
//! A return keeps its own maps.  Blend and Dual-amp sum on every channel
//! and rely on alignment, so a latent amp does not comb-filter.

use crate::app::LogTone;
use crate::app::RigWerkApp;
use crate::audio::{ChannelMap, PathMapSettings};
/// The input routing that keeps IN 1 on L and IN 2 on R.
const STEREO_IN: [[bool; 4]; 2] = [[true, false, false, false], [false, true, false, false]];
/// IN 1 on both L and R (a mono guitar).
const MONO_IN: [[bool; 4]; 2] = [[true, true, false, false], [false, false, false, false]];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PathLayout {
    Single,
    DryWet,
    Blend,
    DualAmp,
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

    pub(crate) fn sums_paths(self) -> bool {
        matches!(self, PathLayout::Blend | PathLayout::DualAmp)
    }

    pub(crate) fn path_count(self) -> usize {
        match self {
            PathLayout::Single => 1,
            _ => 2,
        }
    }

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

pub(crate) fn custom_label(last: Option<PathLayout>) -> String {
    match last {
        Some(layout) => format!("Custom · edited from {}", layout.label()),
        None => "Custom".to_owned(),
    }
}

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

    /// Trims stay.  `false` when a path it needs could not be added.
    pub(crate) fn apply_layout(&mut self, layout: PathLayout) -> bool {
        let selected = self.selected_path;
        while self.sender_slots().len() < layout.path_count() {
            // A copy of the first path fed by the input; with none, an
            // empty one (a copy of a return would be another return).
            let added = match self.sender_slots().first() {
                Some(&first) if layout.sums_paths() => self.duplicate_path(first),
                _ => self.add_path(),
            };
            let Some(slot) = added else {
                return false;
            };
            self.set_path_mute(slot, false);
        }
        self.select_path(selected);
        let slots = self.sender_slots();
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

    pub(crate) fn current_layout(&self) -> Option<PathLayout> {
        let slots = self.sender_slots();
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
