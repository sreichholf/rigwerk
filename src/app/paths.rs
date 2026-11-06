//! Signal paths as the app holds them, and the actions the UI calls on them.
//!
//! The app keeps one [`SignalPath`] per live path slot in
//! [`RigWerkApp::paths`] (indexed by the engine's slot, which never moves
//! while the path lives) and pushes each one's [`PathMix`] to the engine
//! every frame, like the other mix settings.  The plugin chain of each
//! path lives in the engine.  How a path reaches each output device lives
//! with the device: `OutputDeviceSettings::path_map[slot]`.
//!
//! The Mixer and Plugins pages edit [`RigWerkApp::selected_path`]
//! ([`RigWerkApp::path`] / [`RigWerkApp::path_mut`]).
//!
//! Slots are engine-internal.  The UI lists paths in
//! [`RigWerkApp::path_order`] (the display order: paths are appended as
//! they are added), and that is also the order they are saved in: each
//! device's `path_map` is compacted to it.  Loading reuses the live slots
//! first, in order, then free ones.
//!
//! A removed path keeps its device entries while it fades out, so it fades
//! where it played; the next path in that slot overwrites them.

use eframe::egui::Color32;

use crate::app::settings::{default_path_name, FollowerPref, PathPref};
use crate::app::RigWerkApp;
use crate::audio::{
    path_level_gain, sanitize_path_level, OutputDeviceSettings, PathMapSettings, PathMix,
    PathState, MAX_PATHS,
};
use crate::ui::pages::mixer::chain::ChainSelection;
use crate::ui::theme::{AMBER, TEAL};

#[allow(dead_code)] // For the path pages (U1/U2).
/// The path colours, by [`SignalPath::colour`].
pub(crate) const PATH_COLOURS: [Color32; MAX_PATHS] = [
    Color32::from_rgb(56, 221, 191),
    Color32::from_rgb(199, 146, 255),
    Color32::from_rgb(255, 184, 91),
    Color32::from_rgb(255, 143, 177),
];

/// One live path's UI data (name, colour) and the settings pushed to the
/// engine.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SignalPath {
    pub(crate) name: String,
    /// Index into [`PATH_COLOURS`].
    pub(crate) colour: u8,
    pub(crate) mix: PathMix,
}

impl SignalPath {
    #[allow(dead_code)] // For the path pages (U1/U2).
    pub(crate) fn colour(&self) -> Color32 {
        PATH_COLOURS[self.colour as usize % MAX_PATHS]
    }

    pub(crate) fn from_pref(pref: &PathPref) -> Self {
        Self {
            name: pref.name.clone(),
            colour: pref.colour % MAX_PATHS as u8,
            mix: pref.mix(),
        }
    }

    fn to_pref(&self, plugins: Vec<crate::audio::PluginPref>) -> PathPref {
        PathPref::from_mix(self.name.clone(), self.colour, &self.mix, plugins)
    }
}

impl Default for SignalPath {
    fn default() -> Self {
        Self {
            name: default_path_name(0),
            colour: 0,
            mix: PathMix::default(),
        }
    }
}

/// One plugin insert of one path: what plugin editors and GUI windows are
/// keyed by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct InsertKey {
    pub(crate) path: usize,
    pub(crate) insert: usize,
}

#[allow(dead_code)] // For the path pages (U1/U2).
/// What the UI shows for one live path (sidebar entry, path chips, cards).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PathInfo {
    pub(crate) slot: usize,
    pub(crate) name: String,
    pub(crate) colour: Color32,
    pub(crate) colour_index: u8,
    /// Built-in stages that are on (pre EQ, gate, post EQ) plus inserts.
    pub(crate) stages: usize,
    /// Plugin inserts on the path.
    pub(crate) inserts: usize,
    /// Plugin latency of the chain, samples and ms at the running rate.
    pub(crate) latency_samples: u32,
    pub(crate) latency_ms: f32,
    pub(crate) level_db: f32,
    pub(crate) mute: bool,
    pub(crate) solo: bool,
    /// The path meters (last block's peaks after the chain, before the
    /// path level and the master).
    pub(crate) meters: (f32, f32),
    /// Whether the path's own map reaches some channel of some device.
    pub(crate) routed: bool,
    /// Whether the path sounds: not muted, and soloed if any path is.
    pub(crate) audible: bool,
    pub(crate) selected: bool,
}

#[allow(dead_code)] // For the path pages (U1/U2).
impl PathInfo {
    /// "3 stages · 1.3 ms" (plus " · muted"), the sidebar sub-line.
    pub(crate) fn summary(&self) -> String {
        let stages = if self.stages == 1 {
            "1 stage".to_owned()
        } else {
            format!("{} stages", self.stages)
        };
        let mut text = format!("{stages} · {:.1} ms", self.latency_ms);
        if self.mute {
            text.push_str(" · muted");
        }
        text
    }
}

impl RigWerkApp {
    /// The live path slots in display order.  Never empty.
    pub(crate) fn path_slots(&self) -> Vec<usize> {
        let mut slots: Vec<usize> = self
            .path_order
            .iter()
            .copied()
            .filter(|&slot| slot < MAX_PATHS && self.paths[slot].is_some())
            .collect();
        // Robust against an order that missed a slot.
        for slot in 0..MAX_PATHS {
            if self.paths[slot].is_some() && !slots.contains(&slot) {
                slots.push(slot);
            }
        }
        slots
    }

    pub(crate) fn path_count(&self) -> usize {
        self.paths.iter().filter(|path| path.is_some()).count()
    }

    /// One flag per slot: whether a path lives there.
    pub(crate) fn live_path_mask(&self) -> [bool; MAX_PATHS] {
        std::array::from_fn(|slot| self.paths[slot].is_some())
    }

    /// Each slot's level, mute and solo as one gain (0 for no path), as the
    /// mixer applies it.
    pub(crate) fn path_gains(&self) -> [f32; MAX_PATHS] {
        let any_solo = self.paths.iter().flatten().any(|path| path.mix.solo);
        std::array::from_fn(|slot| match &self.paths[slot] {
            Some(path) if !path.mix.mute && (!any_solo || path.mix.solo) => {
                path_level_gain(path.mix.level_db)
            }
            _ => 0.0,
        })
    }

    /// The selected slot, or the first live one if the selection is stale.
    fn selected_slot(&self) -> usize {
        if self
            .paths
            .get(self.selected_path)
            .is_some_and(Option::is_some)
        {
            self.selected_path
        } else {
            self.path_slots().first().copied().unwrap_or(0)
        }
    }

    /// The path the Mixer and Plugins pages edit.
    pub(crate) fn path(&self) -> &SignalPath {
        static FALLBACK: std::sync::OnceLock<SignalPath> = std::sync::OnceLock::new();
        self.paths[self.selected_slot()]
            .as_ref()
            .unwrap_or_else(|| FALLBACK.get_or_init(SignalPath::default))
    }

    /// The path the Mixer and Plugins pages edit, for changing it.
    pub(crate) fn path_mut(&mut self) -> &mut SignalPath {
        let slot = self.selected_slot();
        self.selected_path = slot;
        self.paths[slot].get_or_insert_with(SignalPath::default)
    }

    /// Makes `slot` the path the Mixer and Plugins pages edit.  The Mixer's
    /// insert selection and the next insert position belong to a path, so
    /// they are reset when the path changes.
    pub(crate) fn select_path(&mut self, slot: usize) -> bool {
        if !self.paths.get(slot).is_some_and(Option::is_some) {
            return false;
        }
        if slot != self.selected_path {
            self.plugin_insert_at = None;
            if let ChainSelection::Plugin(index) = self.chain_selected {
                if index >= self.engine.plugin_views(slot).len() {
                    self.chain_selected = ChainSelection::Plugins;
                }
            }
        }
        self.selected_path = slot;
        true
    }

    #[allow(dead_code)] // For the path pages (U1/U2).
    /// What the UI shows for every live path, in display order.
    pub(crate) fn path_infos(&self) -> Vec<PathInfo> {
        let any_solo = self.paths.iter().flatten().any(|path| path.mix.solo);
        let rate = match self.snapshot.output_rate {
            0 => self.sample_rate.max(1),
            rate => rate,
        };
        let selected = self.selected_slot();
        let devices = self.device_settings().len();
        self.path_slots()
            .into_iter()
            .filter_map(|slot| {
                let path = self.paths[slot].as_ref()?;
                let inserts = self.engine.plugin_views(slot).len();
                let mix = &path.mix;
                let builtins = [
                    mix.eqs[0].is_active(),
                    mix.gate.enabled,
                    mix.eqs[1].is_active(),
                ]
                .into_iter()
                .filter(|on| *on)
                .count();
                let latency_samples = self.engine.path_latency_samples(slot);
                Some(PathInfo {
                    slot,
                    name: path.name.clone(),
                    colour: path.colour(),
                    colour_index: path.colour,
                    stages: builtins + inserts,
                    inserts,
                    latency_samples,
                    latency_ms: latency_samples as f32 * 1000.0 / rate as f32,
                    level_db: mix.level_db,
                    mute: mix.mute,
                    solo: mix.solo,
                    meters: self.engine.path_meters(slot),
                    routed: (0..devices).any(|device| self.path_reaches_device(slot, device)),
                    audible: !mix.mute && (!any_solo || mix.solo),
                    selected: slot == selected,
                })
            })
            .collect()
    }

    #[allow(dead_code)] // For the path pages (U1/U2).
    /// Whether another path fits (a slot is free in the app and the
    /// engine has finished any fade-out there).
    pub(crate) fn can_add_path(&self) -> bool {
        (0..MAX_PATHS).any(|slot| {
            self.paths[slot].is_none() && self.engine.path_state(slot) == PathState::Inactive
        })
    }

    /// The first palette colour no live path uses.
    fn free_colour(&self) -> u8 {
        (0..MAX_PATHS as u8)
            .find(|colour| {
                !self
                    .paths
                    .iter()
                    .flatten()
                    .any(|path| path.colour == *colour)
            })
            .unwrap_or(0)
    }

    fn name_taken(&self, name: &str) -> bool {
        self.paths
            .iter()
            .flatten()
            .any(|path| path.name.eq_ignore_ascii_case(name))
    }

    /// "Path N" for an added path, with N the first number from the new
    /// path count up that no live path uses.
    fn new_path_name(&self) -> String {
        let mut number = self.path_count() + 1;
        loop {
            let name = format!("Path {number}");
            if !self.name_taken(&name) {
                return name;
            }
            number += 1;
        }
    }

    /// The name for a copy of `name`: "Main" → "Main 2", "Main 2" →
    /// "Main 3", skipping names live paths use.
    fn copy_name(&self, name: &str) -> String {
        let (base, mut number) = match name.rsplit_once(' ') {
            Some((base, digits))
                if !base.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) =>
            {
                (base, digits.parse::<usize>().map_or(2, |n| n + 1))
            }
            _ => (name, 2),
        };
        loop {
            let candidate = format!("{base} {number}");
            if !self.name_taken(&candidate) {
                return candidate;
            }
            number += 1;
        }
    }

    /// Adds an empty path, live: default routing and EQs, no plugins,
    /// muted, routed to no device.  Selects it.  Returns its slot.
    pub(crate) fn add_path(&mut self) -> Option<usize> {
        let slot = self.engine.add_path()?;
        let path = SignalPath {
            name: self.new_path_name(),
            colour: self.free_colour(),
            mix: self.engine.shared.path(slot).mix(),
        };
        self.paths[slot] = Some(path);
        self.path_order.retain(|&other| other != slot);
        self.path_order.push(slot);
        for settings in self.device_settings_mut() {
            settings.path_map[slot] = PathMapSettings::unrouted();
        }
        self.push_devices_to_engine();
        self.select_path(slot);
        self.push_log(format!("Added path {}, muted.", self.path().name), TEAL);
        Some(slot)
    }

    /// Adds a muted copy of path `source`, live: its chain, its plugins
    /// with their current state (an open plugin window is synced first),
    /// and its map, level and polarity on every device.  Selects it.
    /// Returns its slot.
    pub(crate) fn duplicate_path(&mut self, source: usize) -> Option<usize> {
        let original = self.paths.get(source)?.clone()?;
        if let Some(key) = self.plugin_gui.as_ref().map(|gui| gui.key) {
            if key.path == source {
                // What the plugin's own window changed since its last sync.
                self.engine.finish_plugin_editor(key.path, key.insert);
            }
        }
        let slot = self.engine.duplicate_path(source)?;
        let path = SignalPath {
            name: self.copy_name(&original.name),
            colour: self.free_colour(),
            mix: PathMix {
                mute: true,
                solo: false,
                ..original.mix
            },
        };
        self.paths[slot] = Some(path);
        self.path_order.retain(|&other| other != slot);
        self.path_order.push(slot);
        for settings in self.device_settings_mut() {
            settings.path_map[slot] = settings.path_map[source];
        }
        self.push_devices_to_engine();
        self.select_path(slot);
        self.push_log(
            format!(
                "Duplicated {} as {}, muted.",
                original.name,
                self.path().name
            ),
            TEAL,
        );
        Some(slot)
    }

    #[allow(dead_code)] // For the path pages (U1/U2).
    /// Removes path `slot`: it fades out in the engine (where it played:
    /// its device entries stay until the slot is reused), and its editors
    /// close.  The last path cannot be removed.
    pub(crate) fn remove_path(&mut self, slot: usize) -> bool {
        if self.paths.get(slot).is_none_or(Option::is_none) || self.path_count() <= 1 {
            return false;
        }
        if !self.engine.remove_path(slot) {
            return false;
        }
        if self
            .plugin_gui
            .as_ref()
            .is_some_and(|gui| gui.key.path == slot)
        {
            self.plugin_gui = None;
        }
        if self.plugin_editor.is_some_and(|key| key.path == slot) {
            self.plugin_editor = None;
            self.plugin_param_filter.clear();
        }
        let removed = self.paths[slot].take();
        self.path_order.retain(|&other| other != slot);
        if self.selected_path == slot {
            let next = self.selected_slot();
            self.select_path(next);
        }
        if let Some(removed) = removed {
            self.push_log(format!("Removed path {}.", removed.name), AMBER);
        }
        true
    }

    #[allow(dead_code)] // For the path pages (U1/U2).
    pub(crate) fn set_path_level(&mut self, slot: usize, level_db: f32) {
        if let Some(path) = self.paths.get_mut(slot).and_then(Option::as_mut) {
            path.mix.level_db = sanitize_path_level(level_db);
        }
    }

    pub(crate) fn set_path_mute(&mut self, slot: usize, mute: bool) {
        if let Some(path) = self.paths.get_mut(slot).and_then(Option::as_mut) {
            path.mix.mute = mute;
        }
    }

    #[allow(dead_code)] // For the path pages (U1/U2).
    pub(crate) fn set_path_solo(&mut self, slot: usize, solo: bool) {
        if let Some(path) = self.paths.get_mut(slot).and_then(Option::as_mut) {
            path.mix.solo = solo;
        }
    }

    #[allow(dead_code)] // For the path pages (U1/U2).
    pub(crate) fn rename_path(&mut self, slot: usize, name: &str) {
        if let Some(path) = self.paths.get_mut(slot).and_then(Option::as_mut) {
            let name = name.trim();
            if !name.is_empty() {
                path.name = name.to_owned();
            }
        }
    }

    #[allow(dead_code)] // For the path pages (U1/U2).
    pub(crate) fn set_path_colour(&mut self, slot: usize, colour: u8) {
        if let Some(path) = self.paths.get_mut(slot).and_then(Option::as_mut) {
            path.colour = colour % MAX_PATHS as u8;
        }
    }

    /// Path `slot`'s input routing (2 × 4; taps 1/2 are the path's L/R).
    pub(crate) fn set_path_routes(&mut self, slot: usize, routes: [[bool; 4]; 2]) {
        if let Some(path) = self.paths.get_mut(slot).and_then(Option::as_mut) {
            path.mix.routes = routes;
        }
    }

    /// The output devices' settings (0 = clock device, then followers).
    pub(crate) fn device_settings(&self) -> Vec<&OutputDeviceSettings> {
        std::iter::once(&self.clock_device)
            .chain(self.followers.iter().map(|follower| &follower.settings))
            .collect()
    }

    pub(super) fn device_settings_mut(
        &mut self,
    ) -> impl Iterator<Item = &mut OutputDeviceSettings> {
        std::iter::once(&mut self.clock_device).chain(
            self.followers
                .iter_mut()
                .map(|follower| &mut follower.settings),
        )
    }

    /// Output device `device`'s channel count, as the device rows show it.
    pub(crate) fn device_channels(&self, device: usize) -> usize {
        self.output_device_rows()
            .get(device)
            .map_or(2, |row| row.channels)
    }

    #[allow(dead_code)] // For the path pages (U1/U2).
    /// Whether path `slot`'s own map reaches some channel of output device
    /// `device` (0 = clock device, then followers).  The mono copy on
    /// unreached channels does not count.
    pub(crate) fn path_reaches_device(&self, slot: usize, device: usize) -> bool {
        if slot >= MAX_PATHS || self.paths[slot].is_none() {
            return false;
        }
        let Some(settings) = self.device_settings().get(device).copied() else {
            return false;
        };
        settings.map(slot).reaches_any(self.device_channels(device))
    }

    #[allow(dead_code)] // For the path pages (U1/U2).
    /// Path `slot`'s map, level, polarity and trim on output device
    /// `device` (0 = clock device, then followers).
    pub(crate) fn device_path_map(&self, device: usize, slot: usize) -> Option<PathMapSettings> {
        let settings = self.device_settings().get(device).copied()?;
        settings.path_map.get(slot).copied()
    }

    #[allow(dead_code)] // For the path pages (U1/U2).
    /// Sets path `slot`'s map, level, polarity and trim on output device
    /// `device` (0 = clock device, then followers).  Live, crossfaded.
    pub(crate) fn set_device_path_map(
        &mut self,
        device: usize,
        slot: usize,
        entry: PathMapSettings,
    ) {
        if slot >= MAX_PATHS {
            return;
        }
        if let Some(settings) = self.device_settings_mut().nth(device) {
            settings.path_map[slot] = entry.sanitized();
        }
    }

    /// Pushes every live path's settings to the engine.  Called each frame.
    pub(crate) fn push_paths_to_engine(&self) {
        for (slot, path) in self.paths.iter().enumerate() {
            if let Some(path) = path {
                self.engine.set_path_mix(slot, &path.mix);
            }
        }
    }

    /// Pushes the clock device and the followers to the engine.
    pub(crate) fn push_devices_to_engine(&mut self) {
        self.engine.set_clock_device(&self.clock_device);
        self.push_followers_to_engine();
    }

    /// Replaces every path with `prefs` (a loaded session or patch) and the
    /// output devices with `clock` and `followers`, whose `path_map` is in
    /// the order of `prefs`.  The paths reuse the live slots first, in
    /// display order (so a live path just changes its settings, with the
    /// usual ramps), then free slots.  Live paths left over are removed with
    /// a fade, and keep their device entries until it ends.  Plugins are
    /// restored, not opened; a running bridge opens them on the next frame
    /// and the mixer keeps the path silent until then.
    pub(crate) fn install_paths(
        &mut self,
        prefs: &[PathPref],
        clock: OutputDeviceSettings,
        followers: Vec<FollowerPref>,
        selected: usize,
    ) {
        let prefs: Vec<PathPref> = if prefs.is_empty() {
            vec![PathPref::default()]
        } else {
            prefs.iter().take(MAX_PATHS).cloned().collect()
        };
        // Editors belong to the chains being replaced.
        self.finish_plugin_gui();
        self.plugin_editor = None;
        self.plugin_param_filter.clear();
        self.plugin_insert_at = None;

        // Live in the app, then live only in the engine (a fresh start),
        // then free, then still fading out.
        let mut candidates = self.path_slots();
        for wanted in [PathState::Active, PathState::Inactive, PathState::FadingOut] {
            for slot in 0..MAX_PATHS {
                if !candidates.contains(&slot)
                    && self.paths[slot].is_none()
                    && self.engine.path_state(slot) == wanted
                {
                    candidates.push(slot);
                }
            }
        }
        let targets: Vec<usize> = candidates.into_iter().take(prefs.len()).collect();
        // Slots that keep sounding for a while without a new path: those
        // about to be removed and those already fading out.
        let fading: [bool; MAX_PATHS] = std::array::from_fn(|slot| {
            !targets.contains(&slot)
                && (self.paths[slot].is_some()
                    || self.engine.path_state(slot) != PathState::Inactive)
        });

        let remap = |new: &OutputDeviceSettings, old: Option<&OutputDeviceSettings>| {
            let mut out = *new;
            for (slot, entry) in out.path_map.iter_mut().enumerate() {
                *entry = match targets.iter().position(|&target| target == slot) {
                    Some(position) => new.path_map[position],
                    // A fading path fades where it played; on a device it
                    // did not play on, it stays off.
                    None if fading[slot] => {
                        old.map_or(PathMapSettings::unrouted(), |old| old.path_map[slot])
                    }
                    None => PathMapSettings::default(),
                };
            }
            out
        };
        let old_followers: Vec<OutputDeviceSettings> = self
            .followers
            .iter()
            .map(|follower| follower.settings)
            .collect();
        self.clock_device = remap(&clock, Some(&self.clock_device));
        self.followers = followers
            .into_iter()
            .enumerate()
            .map(|(index, follower)| FollowerPref {
                settings: remap(&follower.settings, old_followers.get(index)),
                ..follower
            })
            .collect();

        for (pref, &slot) in prefs.iter().zip(&targets) {
            let path = SignalPath::from_pref(pref);
            self.engine
                .restore_path(slot, &path.mix, pref.plugins.clone());
            self.paths[slot] = Some(path);
        }
        for slot in 0..MAX_PATHS {
            if targets.contains(&slot) {
                continue;
            }
            if self.paths[slot].take().is_some()
                || self.engine.path_state(slot) == PathState::Active
            {
                self.engine.remove_path(slot);
            }
        }
        self.path_order = targets.clone();
        self.selected_path = targets[selected.min(targets.len() - 1)];
        if let ChainSelection::Plugin(_) = self.chain_selected {
            self.chain_selected = ChainSelection::Plugins;
        }
        self.push_paths_to_engine();
        self.push_devices_to_engine();
    }

    /// The live paths as they should be saved (fresh plugin state), and
    /// the slot order they are saved in (the display order).
    pub(crate) fn saved_paths(&self) -> (Vec<PathPref>, Vec<usize>) {
        let order = self.path_slots();
        let prefs = order
            .iter()
            .filter_map(|&slot| {
                let path = self.paths[slot].as_ref()?;
                Some(path.to_pref(self.engine.plugin_prefs(slot)))
            })
            .collect();
        (prefs, order)
    }

    /// The selected path as a position in the saved order.
    pub(crate) fn saved_selected_path(&self, order: &[usize]) -> usize {
        let selected = self.selected_slot();
        order.iter().position(|&slot| slot == selected).unwrap_or(0)
    }

    /// The clock device and the followers with `path_map` compacted to the
    /// saved path order.
    pub(crate) fn saved_devices(
        &self,
        order: &[usize],
    ) -> (OutputDeviceSettings, Vec<FollowerPref>) {
        let clock = self.clock_device.with_path_order(order);
        let followers = self
            .followers
            .iter()
            .map(|follower| FollowerPref {
                settings: follower.settings.with_path_order(order),
                ..follower.clone()
            })
            .collect();
        (clock, followers)
    }
}

#[cfg(test)]
mod tests {
    use super::InsertKey;
    use crate::app::blank_app;
    use crate::audio::{
        ChannelMap, OutputDeviceSettings, PathMapSettings, PathState, PluginPref, MAX_PATHS,
    };

    #[test]
    fn add_duplicate_and_remove_paths_in_the_app_and_the_engine() {
        let mut app = blank_app();
        assert_eq!(app.path_slots(), vec![0]);
        app.path_mut().mix.mono = true;
        app.clock_device.path_map[0].level_db = -3.0;
        app.push_paths_to_engine();

        let added = app.add_path().expect("a free slot");
        assert_eq!(added, 1);
        assert_eq!(app.selected_path, 1);
        assert!(app.path().mix.mute, "added muted");
        assert_eq!(app.clock_device.path_map[1], PathMapSettings::unrouted());
        assert_eq!(app.engine.path_state(1), PathState::Active);

        let copy = app.duplicate_path(0).expect("a free slot");
        assert_eq!(copy, 2);
        let path = app.paths[2].clone().unwrap();
        assert_eq!(path.name, "Main 2");
        assert!(path.mix.mono && path.mix.mute && !path.mix.solo);
        assert_ne!(path.colour, app.paths[0].as_ref().unwrap().colour);
        assert_eq!(app.clock_device.path_map[2].level_db, -3.0);
        assert!(app.engine.shared.path(2).muted());

        let infos = app.path_infos();
        assert_eq!(infos.len(), 3);
        assert!(infos[2].selected && !infos[2].audible);
        assert_eq!(infos[0].summary(), "0 stages · 0.0 ms");

        assert!(app.remove_path(0));
        assert!(app.paths[0].is_none());
        assert_eq!(app.engine.path_state(0), PathState::FadingOut);
        // It fades out where it played.
        assert_eq!(app.clock_device.path_map[0].level_db, -3.0);
        // No bridge runs, so the next service finishes the removal.
        app.engine.service_plugins();
        assert_eq!(app.engine.path_state(0), PathState::Inactive);
        assert!(app.remove_path(1));
        assert!(!app.remove_path(2), "the last path stays");
        assert_eq!(app.path_slots(), vec![2]);
        assert_eq!(app.path().name, "Main 2");
    }

    #[test]
    fn paths_save_in_display_order_with_compacted_device_maps() {
        let mut app = blank_app();
        app.add_path();
        app.add_path();
        let map = ChannelMap {
            left: 0b0100,
            right: 0b1000,
        };
        app.clock_device.path_map[2].map = map;
        app.remove_path(1);
        app.engine.service_plugins();
        // A path added now takes the free slot 1 but is listed last.
        let last = app.add_path().expect("slot 1 is free");
        assert_eq!(last, 1);
        app.clock_device.path_map[1].level_db = -2.0;
        assert_eq!(app.path_slots(), vec![0, 2, 1]);
        assert_eq!(app.path_infos()[2].slot, 1);
        assert_eq!(app.paths[1].as_ref().unwrap().name, "Path 4");
        let (paths, order) = app.saved_paths();
        assert_eq!(order, vec![0, 2, 1]);
        assert_eq!(paths[1].name, "Path 3");
        assert_eq!(app.saved_selected_path(&order), 2);
        let (clock, _) = app.saved_devices(&order);
        assert_eq!(clock.path_map[1].map, map);
        assert_eq!(clock.path_map[2].level_db, -2.0);

        // Installing two of them reuses the live slots in display order;
        // the third fades out.
        app.install_paths(&paths[..2], clock, Vec::new(), 1);
        assert_eq!(app.path_slots(), vec![0, 2]);
        assert_eq!(app.selected_path, 2);
        assert_eq!(app.clock_device.path_map[2].map, map);
        assert_eq!(app.engine.path_state(1), PathState::FadingOut);
        // The fading path keeps its entry until it is silent.
        assert_eq!(app.clock_device.path_map[1].level_db, -2.0);
        assert!(app.path_count() < MAX_PATHS);
    }

    #[test]
    fn a_removed_path_fades_where_it_played() {
        use crate::audio::OutputMixer;
        let mut app = blank_app();
        let copy = app.duplicate_path(0).unwrap();
        app.set_path_mute(copy, false);
        // Path 0 on Out 1, the copy on Out 2 only, inverted.
        app.clock_device.path_map[0].map = ChannelMap { left: 1, right: 0 };
        app.clock_device.path_map[copy] = PathMapSettings {
            map: ChannelMap { left: 0, right: 2 },
            invert: true,
            ..PathMapSettings::default()
        };
        app.push_paths_to_engine();
        app.push_devices_to_engine();
        let shared = std::sync::Arc::clone(&app.engine.shared);
        let mut mixer = OutputMixer::new(48_000, 2);
        mixer.prepare(256);
        let run = |mixer: &mut OutputMixer| {
            let mut out = Vec::new();
            mixer.render_direct_devices(&shared, &[0.5; 256], &[0.5; 256], |_, _, mapped| {
                out.push([mapped.sample(0), mapped.sample(1)]);
            });
            out
        };
        let steady = run(&mut mixer);
        assert_eq!(steady[255], [0.5, -0.5]);
        assert!(app.remove_path(copy));
        app.push_devices_to_engine();
        let mut out = run(&mut mixer);
        out.extend(run(&mut mixer));
        // Out 1 never picks the fading copy up, and Out 2 only fades.
        assert!(out.iter().all(|frame| frame[0] == 0.5), "{:?}", out[100]);
        assert!(out.iter().all(|frame| frame[1] <= 0.0 && frame[1] >= -0.5));
        assert_eq!(out.last().unwrap()[1], 0.0);
        assert_eq!(app.engine.path_state(copy), PathState::Inactive);
    }

    #[test]
    fn names_stay_unique() {
        let mut app = blank_app();
        let copy = app.duplicate_path(0).unwrap();
        assert_eq!(app.paths[copy].as_ref().unwrap().name, "Main 2");
        let copy_of_copy = app.duplicate_path(copy).unwrap();
        assert_eq!(app.paths[copy_of_copy].as_ref().unwrap().name, "Main 3");
        app.rename_path(copy, "Path 4");
        let added = app.add_path().unwrap();
        assert_eq!(app.paths[added].as_ref().unwrap().name, "Path 5");
    }

    #[test]
    fn selecting_another_path_resets_its_insert_selection() {
        use crate::ui::pages::mixer::chain::ChainSelection;
        let mut app = blank_app();
        app.engine
            .restore_plugins(0, vec![insert("A.vst3"), insert("B.vst3")]);
        let second = app.add_path().unwrap();
        assert!(app.select_path(0));
        app.chain_selected = ChainSelection::Plugin(1);
        app.plugin_insert_at = Some(1);
        assert!(app.select_path(second));
        assert_eq!(app.chain_selected, ChainSelection::Plugins);
        assert_eq!(app.plugin_insert_at, None);
        assert!(!app.select_path(3), "no path there");
        assert_eq!(app.selected_path, second);
    }

    #[test]
    fn preview_meters_sum_every_path() {
        let mut app = blank_app();
        let copy = app.duplicate_path(0).unwrap();
        app.set_path_mute(copy, false);
        app.clock_device.path_map[copy].invert = true;
        app.clock_device.path_map[copy].level_db = -60.0;
        let gains = app.path_gains();
        assert_eq!(gains[0], 1.0);
        let frame = app
            .clock_device
            .settled_sum(app.live_path_mask(), gains, 2, 0.5, -0.25);
        assert_eq!(frame[..2], [0.5, -0.25], "a −∞ dB path adds nothing");
        app.clock_device.path_map[copy].level_db = 0.0;
        let frame =
            app.clock_device
                .settled_sum(app.live_path_mask(), app.path_gains(), 2, 0.5, -0.25);
        assert_eq!(frame[..2], [0.0, 0.0], "the flipped copy cancels");
        app.set_path_solo(copy, true);
        assert_eq!(app.path_gains()[0], 0.0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_new_follower_copies_the_clock_devices_paths() {
        let mut app = blank_app();
        // Followers need a PipeWire clock device.
        app.inventory.outputs.push(crate::audio::DeviceItem {
            id: "pipewire:alsa_output.usb".to_owned(),
            name: "Interface".to_owned(),
            backend: "PipeWire".to_owned(),
            channels: 2,
            demo: false,
            preferred: false,
            asio_clsid: None,
        });
        app.output_idx = 0;
        let second = app.add_path().unwrap();
        app.add_follower_device(crate::audio::VIRTUAL_OUT_ID);
        let follower = app.followers.last().expect("added");
        assert_eq!(follower.settings.path_map, app.clock_device.path_map);
        assert_eq!(
            follower.settings.path_map[second],
            PathMapSettings::unrouted(),
            "the empty path stays off the new device"
        );
    }

    fn insert(path: &str) -> PluginPref {
        PluginPref {
            path: path.to_owned(),
            name: path.to_owned(),
            ..PluginPref::default()
        }
    }

    #[test]
    fn a_preset_saves_and_applies_every_path() {
        let mut app = blank_app();
        app.session_name = "Two paths".to_owned();
        app.engine.restore_plugins(0, vec![insert("A.vst3")]);
        let copy = app.duplicate_path(0).expect("a free slot");
        assert_eq!(app.engine.plugin_prefs(copy)[0].path, "A.vst3");
        app.engine.restore_plugins(copy, vec![insert("B.vst3")]);
        app.set_path_level(copy, -5.0);
        app.set_path_mute(copy, false);
        app.clock_device.path_map[copy].invert = true;
        app.save_preset();
        let saved = app.sessions[0].clone();
        assert_eq!(saved.paths.len(), 2);
        assert_eq!(saved.paths[1].name, "Main 2");
        assert_eq!(saved.paths[1].plugins[0].path, "B.vst3");
        assert_eq!(saved.paths[1].level_db, -5.0);
        assert!(saved.clock_device.path_map[1].invert);

        // Back to one path, then apply: both paths come back, live.
        assert!(app.remove_path(copy));
        app.engine.service_plugins();
        app.clock_device = OutputDeviceSettings::default();
        app.apply_preset(0);
        assert_eq!(app.path_slots(), vec![0, 1]);
        assert_eq!(app.engine.path_state(1), PathState::Active);
        assert_eq!(app.engine.plugin_prefs(0)[0].path, "A.vst3");
        assert_eq!(app.engine.plugin_prefs(1)[0].path, "B.vst3");
        assert_eq!(app.engine.shared.path(1).level_db(), -5.0);
        assert!(!app.engine.shared.path(1).muted());
        assert!(app.clock_device.path_map[1].invert);
        assert_eq!(app.engine.shared.clock_device.path_gain(1), -1.0);

        // A patch with one path removes the second, with a fade.
        app.sessions.push(crate::app::settings::Preset {
            name: "One".to_owned(),
            ..crate::app::settings::Preset::default()
        });
        app.apply_preset(1);
        assert_eq!(app.path_slots(), vec![0]);
        assert_eq!(app.engine.path_state(1), PathState::FadingOut);
        assert!(app.engine.plugin_prefs(0).is_empty());
    }

    #[test]
    fn plugin_editors_are_keyed_by_path_and_insert() {
        let mut app = blank_app();
        app.engine
            .restore_plugins(0, vec![insert("A.vst3"), insert("B.vst3")]);
        let second = app.add_path().expect("a free slot");
        app.engine.restore_plugins(second, vec![insert("C.vst3")]);
        app.plugin_editor = Some(InsertKey { path: 0, insert: 1 });
        // The selected path is the new one: its removal leaves path 0's
        // editor alone.
        app.remove_plugin(0);
        assert_eq!(app.plugin_editor, Some(InsertKey { path: 0, insert: 1 }));
        assert!(app.engine.plugin_views(second).is_empty());
        assert_eq!(app.engine.plugin_views(0).len(), 2);
        // On path 0, the editor follows its insert as it slides up.
        assert!(app.select_path(0));
        app.remove_plugin(0);
        assert_eq!(app.plugin_editor, Some(InsertKey { path: 0, insert: 0 }));
        // Removing the path closes its editor.
        assert!(app.remove_path(0));
        assert_eq!(app.plugin_editor, None);
        assert_eq!(app.selected_path, second);
    }
}
