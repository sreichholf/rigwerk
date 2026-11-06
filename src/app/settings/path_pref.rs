//! One saved signal path, as `Preferences` and `Preset` store it.

use super::serde_util::skip_unreadable;
use crate::audio::{EqSettings, GateSettings, PathMix, PluginPref, MAX_PATHS};

/// One signal path: its name and colour (UI data), its chain settings, its
/// plugins and its level, mute and solo.  How it reaches each device lives
/// with the device (`OutputDeviceSettings::path_map`, indexed by the path's
/// position in the saved list).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct PathPref {
    pub(crate) name: String,
    /// Index into the path palette (`app::paths::PATH_COLOURS`).
    pub(crate) colour: u8,
    /// The 2 × 4 input routing; only taps 1/2 (the path's L/R) are used.
    pub(crate) routes: [[bool; 4]; 2],
    pub(crate) mono: bool,
    /// Channel enable for the path's L and R.
    pub(crate) output_channels: [bool; 2],
    /// The EQ before the plugin inserts.
    pub(crate) eq: EqSettings,
    /// The EQ after the plugin inserts.
    pub(crate) eq_post: EqSettings,
    pub(crate) gate: GateSettings,
    /// The insert chain.  An entry that cannot be read is skipped.
    #[serde(deserialize_with = "skip_unreadable")]
    pub(crate) plugins: Vec<PluginPref>,
    /// Path level, dB.
    pub(crate) level_db: f32,
    pub(crate) mute: bool,
    pub(crate) solo: bool,
}

impl Default for PathPref {
    fn default() -> Self {
        Self::from_mix(default_path_name(0), 0, &PathMix::default(), Vec::new())
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
            plugins,
            level_db: mix.level_db,
            mute: mix.mute,
            solo: mix.solo,
        }
    }

    /// The chain settings the engine takes, sanitized.
    pub(crate) fn mix(&self) -> PathMix {
        PathMix {
            routes: self.routes,
            mono: self.mono,
            output_enabled: self.output_channels,
            eqs: [self.eq, self.eq_post],
            gate: self.gate,
            level_db: self.level_db,
            mute: self.mute,
            solo: self.solo,
        }
        .sanitized()
    }

    /// EQs, gate and level in range; a blank name gets the default; the
    /// colour stays in the palette.
    pub(crate) fn sanitize(&mut self, position: usize) {
        let mix = self.mix();
        self.eq = mix.eqs[0];
        self.eq_post = mix.eqs[1];
        self.gate = mix.gate;
        self.level_db = mix.level_db;
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
