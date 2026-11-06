//! Routing cards (the starting points of the sum matrix) and the routing
//! warnings the Signal Routing page shows.
//!
//! A card sets the input routing of the first paths (in display order) and
//! their map, level and polarity on every output device.  It never removes
//! a path or an insert: missing paths are added (empty, or for Blend and
//! Dual-amp as a copy of the first path), and paths past the card's count
//! are kept but routed nowhere.  Any edit afterwards reads as Custom
//! ([`RigWerkApp::current_card`] returns `None`).
//!
//! Warnings: until the alignment engine (P3) exists every path is
//! unaligned, so two paths sounding on one channel of a device comb filter
//! ([`RoutingWarningKind::Sum`], red), and two paths on different channels
//! of one device shift the image ([`RoutingWarningKind::Shift`], amber).

// The Signal Routing page (U2) builds on all of this.
#![allow(dead_code)]

use crate::app::RigWerkApp;
use crate::audio::{ChannelMap, PathMapSettings, MAX_PATHS};
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

    /// Cards that sum paths on one channel: they comb filter until the
    /// alignment engine (P3) exists, so the UI offers them disabled.
    pub(crate) fn needs_alignment(self) -> bool {
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

/// Why the Signal Routing page warns about one device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RoutingWarningKind {
    /// Unaligned paths add up on a channel: comb filter (red).
    Sum,
    /// Unaligned paths play on different channels of one device: the image
    /// shifts (amber).
    Shift,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RoutingWarning {
    pub(crate) kind: RoutingWarningKind,
    /// The path slots involved, in display order.
    pub(crate) paths: Vec<usize>,
    /// The device channels (0-based) involved: those carrying several paths
    /// for [`RoutingWarningKind::Sum`], every playing one for `Shift`.
    pub(crate) channels: Vec<usize>,
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
            let added = if card.needs_alignment() {
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

    /// What the Signal Routing page should warn about on output device
    /// `device` (0 = clock device, then followers).  Every live path counts,
    /// muted or not: the routing decides, as for alignment.  Until P3 every
    /// path is unaligned.  At most one warning: `Sum` wins over `Shift`.
    pub(crate) fn routing_warnings(&self, device: usize) -> Vec<RoutingWarning> {
        let Some(settings) = self.device_settings().get(device).copied() else {
            return Vec::new();
        };
        let channels = self.device_channels(device);
        let live = self.live_path_mask();
        let order = self.path_slots();
        let mut sounding = [false; MAX_PATHS];
        let mut summed = [false; MAX_PATHS];
        let mut sum_channels = Vec::new();
        let mut playing_channels = Vec::new();
        for channel in 0..channels {
            let on = settings.paths_on_channel(live, channels, channel);
            let count = on.iter().filter(|on| **on).count();
            if count > 0 {
                playing_channels.push(channel);
            }
            if count > 1 {
                sum_channels.push(channel);
            }
            for path in 0..MAX_PATHS {
                sounding[path] |= on[path];
                summed[path] |= on[path] && count > 1;
            }
        }
        let in_order = |flags: [bool; MAX_PATHS]| -> Vec<usize> {
            order.iter().copied().filter(|&slot| flags[slot]).collect()
        };
        if !sum_channels.is_empty() {
            return vec![RoutingWarning {
                kind: RoutingWarningKind::Sum,
                paths: in_order(summed),
                channels: sum_channels,
            }];
        }
        let paths = in_order(sounding);
        if paths.len() > 1 {
            return vec![RoutingWarning {
                kind: RoutingWarningKind::Shift,
                paths,
                channels: playing_channels,
            }];
        }
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{Card, RoutingWarningKind};
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
        // The stream device sums both: a comb filter until P3.
        let warnings = app.routing_warnings(1);
        assert_eq!(warnings[0].kind, RoutingWarningKind::Sum);
        assert_eq!(warnings[0].paths, vec![0, second]);
        // On the clock device they sit on different channels.
        assert_eq!(app.routing_warnings(0)[0].kind, RoutingWarningKind::Shift);

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
        assert!(Card::Blend.needs_alignment() && Card::DualAmp.needs_alignment());
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
