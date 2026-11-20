//! One saved signal path, as `Preferences` and `Session` store it.

use super::serde_util::skip_unreadable;
use crate::audio::{
    sanitize_send, CabSettings, EqSettings, GateSettings, LegacyNamSettings, NamBlockSettings,
    PathFeed, PathMix, PluginPref, MAX_NAM_BLOCKS, MAX_PATHS, SEND_OFF_DB,
};

/// One signal path: its name and colour (UI data), its chain settings, its
/// plugins, whether that effect chain is bypassed, and its level, mute and
/// solo.  How it reaches each device lives with the device
/// (`OutputDeviceSettings::path_map`, indexed by the path's position in the
/// saved list).  Its sends are indexed the same way.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct PathPref {
    pub(crate) name: String,
    /// Index into the path palette.
    pub(crate) colour: u8,
    /// The 2 × 4 input routing; only taps 1/2 (the path's L/R) are used.
    pub(crate) routes: [[bool; 4]; 2],
    pub(crate) mono: bool,
    /// Channel enable for the path's L and R.
    pub(crate) output_channels: [bool; 2],
    /// The EQ before the plugin effects.
    pub(crate) eq: EqSettings,
    /// The EQ after the plugin effects.
    pub(crate) eq_post: EqSettings,
    pub(crate) gate: GateSettings,
    /// The NAM blocks in signal order, with their captures.  Missing on
    /// older saves, and omitted while empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) nam_blocks: Vec<NamBlockSettings>,
    /// The single NAM stage older saves have.  [`Self::sanitize`] folds it
    /// into `nam_blocks` and clears it, so a save never writes it again.
    #[serde(default, skip_serializing_if = "LegacyNamSettings::is_unset")]
    pub(crate) amp: LegacyNamSettings,
    /// Built-in cabinet, including the impulse path.  Missing on older
    /// saves, and omitted while it is still the default.
    #[serde(default, skip_serializing_if = "CabSettings::is_unset")]
    pub(crate) cab: CabSettings,
    /// The effect chain.  An entry that cannot be read is skipped.
    #[serde(deserialize_with = "skip_unreadable")]
    pub(crate) plugins: Vec<PluginPref>,
    /// Skips every effect's processing without changing each effect's own
    /// bypass.  Older saves have no field and stay off.
    #[serde(default)]
    pub(crate) chain_bypass: bool,
    /// Path level, dB.
    pub(crate) level_db: f32,
    pub(crate) mute: bool,
    pub(crate) solo: bool,
    /// What the chain runs on.  Omitted for a path fed by the input, which
    /// is what older saves have.
    #[serde(default, skip_serializing_if = "is_input")]
    pub(crate) feed: PathFeed,
    /// The send to each path, dB, by that path's position in the saved
    /// list.  Omitted while every send is off.  [`Self::mix`] does not read
    /// it: slots are not positions, so the app maps it (`install_paths`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) sends: Vec<f32>,
}

fn is_input(feed: &PathFeed) -> bool {
    *feed == PathFeed::Input
}

impl Default for PathPref {
    fn default() -> Self {
        Self::from_mix(
            default_path_name(0),
            0,
            &PathMix::default(),
            Vec::new(),
            "",
            Vec::new(),
        )
    }
}

/// The name a path gets when nothing else names it: "Main" for the first,
/// "Path N" after that.
pub(crate) fn default_path_name(position: usize) -> String {
    if position == 0 {
        "Main".to_owned()
    } else {
        format!("Path {}", position + 1)
    }
}

impl PathPref {
    pub(crate) fn from_mix(
        name: String,
        colour: u8,
        mix: &PathMix,
        nam_blocks: Vec<NamBlockSettings>,
        cab_file: &str,
        plugins: Vec<PluginPref>,
    ) -> Self {
        Self {
            name,
            colour,
            routes: mix.routes,
            mono: mix.mono,
            output_channels: mix.output_enabled,
            eq: mix.eqs[0],
            eq_post: mix.eqs[1],
            gate: mix.gate,
            nam_blocks,
            amp: LegacyNamSettings::default(),
            cab: CabSettings::from_controls(mix.cab, cab_file.trim().to_owned()),
            plugins,
            chain_bypass: mix.chain_bypass,
            level_db: mix.level_db,
            mute: mix.mute,
            solo: mix.solo,
            feed: mix.feed,
            sends: Vec::new(),
        }
    }

    /// `sends` (indexed by slot) in the saved order `order`, as stored:
    /// empty while every send is off.
    pub(crate) fn sends_in_order(sends: &[f32; MAX_PATHS], order: &[usize]) -> Vec<f32> {
        let saved: Vec<f32> = order
            .iter()
            .map(|&slot| sends.get(slot).copied().unwrap_or(SEND_OFF_DB))
            .collect();
        if saved
            .iter()
            .all(|&level| sanitize_send(level) <= SEND_OFF_DB)
        {
            Vec::new()
        } else {
            saved
        }
    }

    /// The stored sends by slot, the path at saved position `n` living in
    /// `slots[n]`.  Positions past `slots` are dropped.
    pub(crate) fn sends_by_slot(&self, slots: &[usize]) -> [f32; MAX_PATHS] {
        let mut sends = [SEND_OFF_DB; MAX_PATHS];
        for (&level, &slot) in self.sends.iter().zip(slots) {
            if slot < MAX_PATHS {
                sends[slot] = sanitize_send(level);
            }
        }
        sends
    }

    /// The chain settings the engine takes, sanitized.
    pub(crate) fn mix(&self) -> PathMix {
        PathMix {
            routes: self.routes,
            mono: self.mono,
            output_enabled: self.output_channels,
            eqs: [self.eq, self.eq_post],
            gate: self.gate,
            cab: self.cab.controls(),
            level_db: self.level_db,
            mute: self.mute,
            solo: self.solo,
            chain_bypass: self.chain_bypass,
            feed: self.feed,
            sends: [SEND_OFF_DB; MAX_PATHS],
        }
        .sanitized()
    }

    /// EQs, gate, level and sends in range, and a return without a solo or
    /// sends; a blank name gets the default; the colour stays in the
    /// palette.  An older save's single NAM stage
    /// becomes the first block, once.
    pub(crate) fn sanitize(&mut self, position: usize) {
        if self.nam_blocks.is_empty() && !self.amp.is_unset() {
            self.nam_blocks
                .push(NamBlockSettings::from_legacy(&self.amp));
        }
        self.amp = LegacyNamSettings::default();
        self.nam_blocks.truncate(MAX_NAM_BLOCKS);
        for block in &mut self.nam_blocks {
            *block = block.sanitized();
        }
        let mix = self.mix();
        self.eq = mix.eqs[0];
        self.eq_post = mix.eqs[1];
        self.gate = mix.gate;
        self.cab = CabSettings::from_controls(mix.cab, self.cab.file.clone()).sanitized();
        self.level_db = mix.level_db;
        self.solo = mix.solo;
        if self.feed == PathFeed::Sends {
            self.sends.clear();
        }
        self.sends.truncate(MAX_PATHS);
        for level in &mut self.sends {
            *level = sanitize_send(*level);
        }
        if self.sends.iter().all(|&level| level <= SEND_OFF_DB) {
            self.sends.clear();
        }
        if self.name.trim().is_empty() {
            self.name = default_path_name(position);
        }
        self.colour %= MAX_PATHS as u8;
        for plugin in &mut self.plugins {
            if plugin.name.trim().is_empty() {
                plugin.name = std::path::Path::new(&plugin.path)
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "Plugin".to_owned());
            }
        }
    }
}

/// Keeps at most [`MAX_PATHS`] paths and sanitizes each.  At least one
/// path always remains.
pub(crate) fn sanitize_paths(paths: &mut Vec<PathPref>) {
    paths.truncate(MAX_PATHS);
    if paths.is_empty() {
        paths.push(PathPref::default());
    }
    for (position, path) in paths.iter_mut().enumerate() {
        path.sanitize(position);
    }
}
