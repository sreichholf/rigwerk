//! Routing cards (the starting points of the sum matrix).
//!
//! A card sets the input routing of the first paths (in display order) and
//! their map, level and polarity on every output device.  It never removes
//! a path or an insert: missing paths are added (empty, or for Blend and
//! Dual-amp as a copy of the first path), and paths past the card's count
//! are kept but routed nowhere.  Any edit afterwards reads as Custom
//! ([`RigWerkApp::current_card`] returns `None`).
//!
//! Blend and Dual-amp sum two paths on every channel; the per-device
//! latency alignment (on by default, `app::alignment`) lines them up, so a
//! latent amp sim on one of them does not comb filter.

use crate::app::RigWerkApp;
use crate::audio::{ChannelMap, PathMapSettings};
use crate::ui::theme::TEAL;

/// The input routing that keeps IN 1 on L and IN 2 on R.
const STEREO_IN: [[bool; 4]; 2] = [[true, false, false, false], [false, true, false, false]];
/// IN 1 on both L and R (a mono guitar).
const MONO_IN: [[bool; 4]; 2] = [[true, true, false, false], [false, false, false, false]];

/// One routing card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Card {
    /// One path, L/R on Out 1/2: today's routing.
    Single,
    /// Path 1 on the left channels, path 2 on the right; both on RigWerk
    /// Out.  No sum.
    DryWet,
    /// Clean path at −6 dB under a driven one, summed on every device.
    Blend,
    /// Two amp paths summed, the second with its polarity flipped.
    DualAmp,
    /// Path 1 on the clock device, path 2 on every follower.
    PerDevice,
}

impl Card {
    pub(crate) const ALL: [Card; 5] = [
        Card::Single,
        Card::DryWet,
        Card::Blend,
        Card::DualAmp,
        Card::PerDevice,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Card::Single => "Single path",
            Card::DryWet => "Dry L / Wet R",
            Card::Blend => "Blend clean",
            Card::DualAmp => "Dual-amp",
            Card::PerDevice => "Per-device mixes",
        }
    }

    /// Cards that sum paths on one channel (they rely on the latency
    /// alignment, on by default per device).  Their missing second path
    /// starts as a copy of the first.
    pub(crate) fn sums_paths(self) -> bool {
        matches!(self, Card::Blend | Card::DualAmp)
    }

    /// How many paths the card sets up.
    pub(crate) fn path_count(self) -> usize {
        match self {
            Card::Single => 1,
            _ => 2,
        }
    }

    fn input_routes(self) -> [[bool; 4]; 2] {
        match self {
            Card::Single | Card::PerDevice => STEREO_IN,
            Card::DryWet | Card::Blend | Card::DualAmp => MONO_IN,
        }
    }

    /// Card path `path`'s map, level and polarity on a device; `None` past
    /// the card's paths (routed nowhere).
    fn entry(self, path: usize, device: &DeviceShape) -> (ChannelMap, f32, bool) {
        let stereo = ChannelMap::default();
        let none = ChannelMap { left: 0, right: 0 };
        if path >= self.path_count() {
            return (none, 0.0, false);
        }
        match self {
            Card::Single => (stereo, 0.0, false),
            Card::DryWet if device.virtual_out => (stereo, 0.0, false),
            Card::DryWet => {
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
            Card::Blend => (stereo, if path == 0 { -6.0 } else { 0.0 }, false),
            Card::DualAmp => (stereo, 0.0, path == 1),
            Card::PerDevice => {
                let plays = if device.clock { path == 0 } else { path == 1 };
                (if plays { stereo } else { none }, 0.0, false)
            }
        }
    }
}

/// What a card needs to know about one output device.
struct DeviceShape {
    channels: usize,
    clock: bool,
    virtual_out: bool,
}

impl RigWerkApp {
    fn device_shapes(&self) -> Vec<DeviceShape> {
        self.output_device_rows()
            .iter()
            .map(|row| DeviceShape {
                channels: row.channels,
                clock: row.clock,
                virtual_out: row.virtual_out,
            })
            .collect()
    }

    /// Applies `card`: input routing of its paths and every path's map,
    /// level and polarity on every device (trims stay).  Adds the paths it
    /// is missing, playing (unmuted); never removes a path or an insert.
    /// `false` if a path it needs could not be added.
    pub(crate) fn apply_card(&mut self, card: Card) -> bool {
        let selected = self.selected_path;
        while self.path_count() < card.path_count() {
            let first = self.path_slots()[0];
            let added = if card.sums_paths() {
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
            if position < card.path_count() {
                self.set_path_routes(slot, card.input_routes());
            }
        }
        for (settings, shape) in self.device_settings_mut().zip(&shapes) {
            for (position, &slot) in slots.iter().enumerate() {
                let (map, level_db, invert) = card.entry(position, shape);
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
        self.routing_last_card = Some(card);
        self.push_log(format!("Routing card: {}.", card.label()), TEAL);
        true
    }

    /// The card the routing matches exactly, or `None` (Custom).
    pub(crate) fn current_card(&self) -> Option<Card> {
        let slots = self.path_slots();
        let shapes = self.device_shapes();
        let devices = self.device_settings();
        Card::ALL.into_iter().find(|&card| {
            slots.len() >= card.path_count()
                && slots.iter().enumerate().all(|(position, &slot)| {
                    let routes_match = position >= card.path_count()
                        || self.paths[slot]
                            .as_ref()
                            .is_some_and(|path| path.mix.routes == card.input_routes());
                    routes_match
                        && devices.iter().zip(&shapes).all(|(settings, shape)| {
                            let entry = settings.path_map[slot];
                            card.entry(position, shape) == (entry.map, entry.level_db, entry.invert)
                        })
                })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::Card;
    use crate::app::alignment::RoutingWarningKind;
    use crate::app::blank_app;
    use crate::app::settings::FollowerPref;
    use crate::audio::ChannelMap;

    #[test]
    fn cards_set_routing_add_paths_and_read_back() {
        let mut app = blank_app();
        app.followers.push(FollowerPref::virtual_out());
        assert_eq!(app.current_card(), Some(Card::Single));

        assert!(app.apply_card(Card::DryWet));
        assert_eq!(app.path_count(), 2);
        let second = app.path_slots()[1];
        assert!(
            !app.paths[second].as_ref().unwrap().mix.mute,
            "added playing"
        );
        assert_eq!(app.current_card(), Some(Card::DryWet));
        assert_eq!(
            app.clock_device.map(0),
            ChannelMap {
                left: 0b01,
                right: 0
            }
        );
        assert_eq!(
            app.clock_device.map(second),
            ChannelMap {
                left: 0,
                right: 0b10
            }
        );
        assert_eq!(app.followers[0].settings.map(second), ChannelMap::default());
        // Aligned devices never warn.
        assert!(app.routing_warnings(0).is_empty());
        assert!(app.routing_warnings(1).is_empty());
        // Unaligned with the same latency and trim: still lined up.
        app.set_device_align(0, false);
        app.set_device_align(1, false);
        assert!(app.routing_warnings(1).is_empty());
        // A trim on the second path: the stream device sums two paths that
        // arrive apart (a comb filter) ...
        for device in 0..2 {
            let mut entry = app.device_path_map(device, second).unwrap();
            entry.trim_ms = 1.0;
            app.set_device_path_map(device, second, entry);
        }
        let warnings = app.routing_warnings(1);
        assert_eq!(warnings[0].kind, RoutingWarningKind::Sum);
        assert_eq!(warnings[0].paths, vec![0, second]);
        // ... and on the clock device they sit on different channels.
        assert_eq!(app.routing_warnings(0)[0].kind, RoutingWarningKind::Shift);
        for device in 0..2 {
            app.set_device_align(device, true);
            let mut entry = app.device_path_map(device, second).unwrap();
            entry.trim_ms = 0.0;
            app.set_device_path_map(device, second, entry);
        }

        assert!(app.apply_card(Card::PerDevice));
        assert_eq!(app.current_card(), Some(Card::PerDevice));
        assert!(app.routing_warnings(0).is_empty());
        assert!(app.path_reaches_device(0, 0) && !app.path_reaches_device(0, 1));
        assert!(app.path_reaches_device(second, 1));
        assert!(app.path_infos().iter().all(|info| info.routed));

        // Single keeps the second path (and its inserts) but routes it nowhere.
        assert!(app.apply_card(Card::Single));
        assert_eq!(app.path_count(), 2);
        assert_eq!(app.current_card(), Some(Card::Single));
        assert!(!app.path_infos()[1].routed);

        // Any edit afterwards reads as Custom.
        app.clock_device.path_map[0].level_db = -1.0;
        assert_eq!(app.current_card(), None);
    }

    #[test]
    fn blend_and_dual_amp_copy_the_first_path() {
        let mut app = blank_app();
        app.path_mut().mix.mono = true;
        assert!(Card::Blend.sums_paths() && Card::DualAmp.sums_paths());
        assert!(!Card::DryWet.sums_paths());
        assert!(app.apply_card(Card::DualAmp));
        let second = app.path_slots()[1];
        assert!(app.paths[second].as_ref().unwrap().mix.mono, "a copy");
        assert!(app.clock_device.path_map[second].invert);
        assert_eq!(app.current_card(), Some(Card::DualAmp));
        assert!(app.apply_card(Card::Blend));
        assert_eq!(app.path_count(), 2);
        assert_eq!(app.clock_device.path_map[0].level_db, -6.0);
        assert!(!app.clock_device.path_map[second].invert);
        assert_eq!(app.current_card(), Some(Card::Blend));
    }
}
