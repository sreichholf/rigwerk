//! What survives a restart: preferences, saved patches and the choices
//! offered for rates and buffers.

use serde::de::{Deserializer, IgnoredAny};
use serde::Deserialize;

use super::Page;
use crate::audio::{CushionMode, EqSettings, PluginPref};

/// The layout version written with the preferences.  Saves from before the
/// version existed read as 0.  Raise it, and migrate in
/// [`load_preferences`], when a change cannot be covered by field defaults.
pub(crate) const PREFERENCES_SCHEMA: u32 = 1;

/// Launch preferences that survive restarts.  The bridge stays stopped unless
/// the person explicitly asks for it to start with the app.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct Preferences {
    /// See [`PREFERENCES_SCHEMA`]; missing (older saves) reads as 0.
    #[serde(default)]
    pub(crate) schema_version: u32,
    pub(crate) auto_start_bridge: bool,
    /// A saved patch that cannot be read is skipped, not fatal.
    #[serde(deserialize_with = "skip_unreadable")]
    pub(crate) sessions: Vec<Preset>,
    pub(crate) session_name: String,
    /// The page that was open when the app was last used.
    pub(crate) page: String,
    /// Which of the two EQs the Mixer page was editing: 0 pre, 1 post.
    pub(crate) eq_edit: u8,
    /// Whether the output list was showing the low-latency tab.
    pub(crate) output_tab_low_latency: bool,
    /// The patch (profile) that was selected in the Sessions list.
    pub(crate) selected_session: Option<usize>,
    pub(crate) asio_name: Option<String>,
    pub(crate) input_name: Option<String>,
    pub(crate) output_name: Option<String>,
    pub(crate) use_asio_input: bool,
    pub(crate) sample_rate: u32,
    pub(crate) buffer_size: usize,
    pub(crate) input_gain: f32,
    pub(crate) output_gain: f32,
    pub(crate) input_channels: [bool; 2],
    pub(crate) output_channels: [bool; 2],
    pub(crate) mono: bool,
    pub(crate) mute: bool,
    pub(crate) drift: bool,
    pub(crate) monitor: bool,
    /// REAL-style output (the setting kept its older name so saved
    /// preferences carry over).
    pub(crate) low_latency_output: bool,
    pub(crate) routes: [[bool; 4]; 2],
    /// Output side rate; 0 follows the input.
    pub(crate) output_rate: u32,
    pub(crate) output_buffer: usize,
    /// Cushion: 0 Safe, 1 Low latency, 2 Manual (size in `cushion_ms`).
    pub(crate) cushion_mode: u8,
    pub(crate) cushion_ms: f32,
    /// The EQ before the plugin inserts (saves from before there were two
    /// EQs load into this one).  Flat when missing.
    pub(crate) eq: EqSettings,
    /// The EQ after the plugin inserts.
    pub(crate) eq_post: EqSettings,
    /// The master insert chain (paths and settings, not live plugins).  An
    /// entry that cannot be read is skipped, not fatal.
    #[serde(deserialize_with = "skip_unreadable")]
    pub(crate) plugins: Vec<PluginPref>,
    /// Open a plugin's own interface directly instead of RigWerk's control
    /// panel, when the plugin has one that can be embedded.
    pub(crate) prefer_plugin_interface: bool,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            schema_version: PREFERENCES_SCHEMA,
            auto_start_bridge: false,
            sessions: Vec::new(),
            session_name: "My guitar rig".to_owned(),
            page: "live".to_owned(),
            eq_edit: 0,
            output_tab_low_latency: cfg!(target_os = "linux"),
            selected_session: None,
            asio_name: None,
            input_name: None,
            output_name: None,
            use_asio_input: true,
            sample_rate: 48_000,
            buffer_size: 128,
            input_gain: 0.0,
            output_gain: -1.5,
            input_channels: [true, true],
            output_channels: [true, true],
            mono: false,
            mute: false,
            drift: true,
            monitor: true,
            low_latency_output: true,
            routes: [[true, false, false, false], [false, true, false, false]],
            output_rate: 0,
            output_buffer: 128,
            cushion_mode: 0,
            cushion_ms: 10.0,
            eq: EqSettings::default(),
            eq_post: EqSettings::default(),
            plugins: Vec::new(),
            prefer_plugin_interface: false,
        }
    }
}

pub(crate) fn cushion_from_pref(value: u8) -> CushionMode {
    match value {
        1 => CushionMode::LowLatency,
        2 => CushionMode::Manual,
        _ => CushionMode::Safe,
    }
}

pub(crate) fn cushion_to_pref(mode: CushionMode) -> u8 {
    match mode {
        CushionMode::Safe => 0,
        CushionMode::LowLatency => 1,
        CushionMode::Manual => 2,
    }
}

/// Rates offered on either side of the bridge.
pub(crate) const RATES: [u32; 4] = [44_100, 48_000, 88_200, 96_000];

/// Buffer sizes offered on either side of the bridge, in samples.
pub(crate) const BUFFERS: [usize; 5] = [32, 64, 128, 256, 512];

pub(crate) fn rate_label(rate: u32) -> String {
    format!("{:.1} kHz", rate as f32 / 1000.0)
}

pub(crate) const PREFERENCES_KEY: &str = "rigwerk-preferences";

/// Pages are stored by name so the saved preferences stay readable and keep
/// working when pages are added or reordered.
pub(crate) fn page_key(page: Page) -> &'static str {
    match page {
        Page::Live => "live",
        Page::Mixer => "mixer",
        Page::Routing => "routing",
        Page::Devices => "devices",
        Page::Sessions => "sessions",
        Page::Plugins => "plugins",
        Page::Guide => "guide",
    }
}

pub(crate) fn page_from_key(key: &str) -> Page {
    match key.trim().to_lowercase().as_str() {
        "mixer" => Page::Mixer,
        "routing" => Page::Routing,
        "devices" => Page::Devices,
        "sessions" => Page::Sessions,
        "plugins" => Page::Plugins,
        "guide" => Page::Guide,
        _ => Page::Live,
    }
}

/// A saved patch.  Every field has a default, so a patch saved by an older or
/// newer version (or with a field missing) still loads.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct Preset {
    pub(crate) name: String,
    pub(crate) note: String,
    pub(crate) input: String,
    pub(crate) output: String,
    pub(crate) rate: u32,
    pub(crate) buffer: usize,
    /// Output side; 0 in patches saved before the sides were separate means
    /// "follow the input".
    pub(crate) output_rate: u32,
    pub(crate) output_buffer: usize,
    /// The EQs saved with the patch (pre and post inserts).  Patches from
    /// before an EQ existed have none and leave the current one alone.
    pub(crate) eq: Option<EqSettings>,
    pub(crate) eq_post: Option<EqSettings>,
}

impl Default for Preset {
    /// What a saved patch falls back to field by field.  Empty device names
    /// match no device, so loading such a patch keeps the current one.
    fn default() -> Self {
        Self {
            name: "Untitled patch".to_owned(),
            note: String::new(),
            input: String::new(),
            output: String::new(),
            rate: 48_000,
            buffer: 128,
            output_rate: 0,
            output_buffer: 0,
            eq: None,
            eq_post: None,
        }
    }
}

/// Keeps the first (newest) patch for each name, so older saves made before
/// names were matched collapse into one entry.
pub(crate) fn unique_presets(presets: &[Preset]) -> Vec<Preset> {
    let mut unique: Vec<Preset> = Vec::new();
    for preset in presets {
        let name = preset.name.trim();
        if !unique
            .iter()
            .any(|kept| kept.name.trim().eq_ignore_ascii_case(name))
        {
            unique.push(preset.clone());
        }
    }
    unique
}

/// Reads a list entry by entry, dropping the entries that do not parse, so
/// one damaged patch or insert does not cost the rest of the preferences.
fn skip_unreadable<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Entry<T> {
        Good(T),
        Bad(IgnoredAny),
    }
    let entries = Vec::<Entry<T>>::deserialize(deserializer)?;
    Ok(entries
        .into_iter()
        .filter_map(|entry| match entry {
            Entry::Good(value) => Some(value),
            Entry::Bad(_) => None,
        })
        .collect())
}

/// How many entries the stored lists held before unreadable ones were
/// dropped, so the loss can be reported.
#[derive(Default, Deserialize)]
#[serde(default)]
struct StoredCounts {
    sessions: Vec<IgnoredAny>,
    plugins: Vec<IgnoredAny>,
}

/// The outcome of reading the stored preferences.
pub(crate) struct LoadedPreferences {
    /// `None` when nothing was stored or the text could not be read at all.
    pub(crate) preferences: Option<Preferences>,
    /// `(key, original text)` to keep beside the preferences on the next
    /// save, set whenever the next save would lose something that was
    /// stored: an unreadable file, skipped entries, or a newer layout.
    pub(crate) backup: Option<(String, String)>,
    /// What went wrong, for the app log.
    pub(crate) notices: Vec<String>,
}

/// The key a copy of damaged preferences is kept under.  The time makes it
/// unique, so a later failure never replaces an earlier copy.
pub(crate) fn preferences_backup_key(unix_secs: u64) -> String {
    format!("{PREFERENCES_KEY}-backup-{unix_secs}")
}

/// Parses the stored preferences (eframe keeps them as RON), tolerating
/// damaged entries.  Never loses data silently: whatever the next save would
/// drop is returned as a backup together with a notice for the log.
pub(crate) fn load_preferences(raw: Option<&str>, backup_key: &str) -> LoadedPreferences {
    let Some(raw) = raw else {
        return LoadedPreferences {
            preferences: None,
            backup: None,
            notices: Vec::new(),
        };
    };
    let backup = Some((backup_key.to_owned(), raw.to_owned()));
    let mut preferences: Preferences = match ron::from_str(raw) {
        Ok(preferences) => preferences,
        Err(error) => {
            return LoadedPreferences {
                preferences: None,
                backup,
                notices: vec![format!(
                    "Saved settings could not be read ({error}); starting from defaults. \
                     The original is kept in the settings file as “{backup_key}”."
                )],
            };
        }
    };
    let mut notices = Vec::new();
    let counts: StoredCounts = ron::from_str(raw).unwrap_or_default();
    let skipped_sessions = counts
        .sessions
        .len()
        .saturating_sub(preferences.sessions.len());
    let skipped_plugins = counts
        .plugins
        .len()
        .saturating_sub(preferences.plugins.len());
    if skipped_sessions > 0 || skipped_plugins > 0 {
        notices.push(format!(
            "Skipped {skipped_sessions} saved patch(es) and {skipped_plugins} plugin insert(s) \
             that could not be read. The original settings are kept as “{backup_key}”."
        ));
    }
    if preferences.schema_version > PREFERENCES_SCHEMA {
        notices.push(format!(
            "Settings were saved by a newer RigWerk (layout {}); what this version does not \
             know is kept in the original, saved as “{backup_key}”.",
            preferences.schema_version
        ));
    }
    // Older layouts need no migration yet: the field defaults cover them.
    preferences.schema_version = PREFERENCES_SCHEMA;
    for plugin in &mut preferences.plugins {
        if plugin.name.trim().is_empty() {
            plugin.name = std::path::Path::new(&plugin.path)
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Plugin".to_owned());
        }
    }
    LoadedPreferences {
        preferences: Some(preferences),
        backup: if notices.is_empty() { None } else { backup },
        notices,
    }
}

/// Writes the preferences, first keeping any backup the load asked for so
/// the original text is never overwritten unseen.  The backup is written
/// once, then cleared.
pub(crate) fn store_preferences(
    storage: &mut dyn eframe::Storage,
    preferences: &Preferences,
    backup: &mut Option<(String, String)>,
) {
    if let Some((key, raw)) = backup.take() {
        storage.set_string(&key, raw);
    }
    eframe::set_value(storage, PREFERENCES_KEY, preferences);
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::audio::PluginFormat;

    const BACKUP: &str = "rigwerk-preferences-backup-1";

    /// eframe's file storage in memory: the same key/string interface, and
    /// `set_value` writes RON into it exactly as the app's autosave does.
    #[derive(Default)]
    struct MemoryStorage(HashMap<String, String>);

    impl eframe::Storage for MemoryStorage {
        fn get_string(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
        fn set_string(&mut self, key: &str, value: String) {
            self.0.insert(key.to_owned(), value);
        }
        fn remove_string(&mut self, key: &str) {
            self.0.remove(key);
        }
        fn flush(&mut self) {}
    }

    fn preset(name: &str) -> Preset {
        Preset {
            name: name.to_owned(),
            note: "ASIO → WASAPI".to_owned(),
            input: "Focusrite USB ASIO".to_owned(),
            output: "Speakers".to_owned(),
            rate: 96_000,
            buffer: 64,
            output_rate: 48_000,
            output_buffer: 256,
            eq: Some(EqSettings::default()),
            eq_post: None,
        }
    }

    fn plugin(path: &str, format: PluginFormat) -> PluginPref {
        PluginPref {
            path: path.to_owned(),
            format,
            class_id: Some("ABCD".to_owned()),
            name: "Amp".to_owned(),
            vendor: "Vendor".to_owned(),
            category: "Fx".to_owned(),
            bypass: true,
            mix: 0.5,
            params: ron::from_str("[(id: 7, value: 0.25)]").expect("params"),
        }
    }

    fn stored(preferences: &Preferences) -> String {
        let mut storage = MemoryStorage::default();
        store_preferences(&mut storage, preferences, &mut None);
        eframe::Storage::get_string(&storage, PREFERENCES_KEY).expect("stored")
    }

    #[test]
    fn round_trip_keeps_everything() {
        let preferences = Preferences {
            sessions: vec![preset("Clean"), preset("Lead")],
            plugins: vec![
                plugin("C:/Plugins/Amp.vst3", PluginFormat::Vst3),
                plugin("C:/Plugins/Verb.clap", PluginFormat::Clap),
            ],
            sample_rate: 96_000,
            session_name: "Gig".to_owned(),
            ..Preferences::default()
        };
        let loaded = load_preferences(Some(&stored(&preferences)), BACKUP);
        assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
        assert!(loaded.backup.is_none());
        let back = loaded.preferences.expect("readable");
        assert_eq!(back.schema_version, PREFERENCES_SCHEMA);
        assert_eq!(back.sample_rate, 96_000);
        assert_eq!(back.session_name, "Gig");
        assert_eq!(back.sessions.len(), 2);
        assert_eq!(back.sessions[1].name, "Lead");
        assert_eq!(back.sessions[0].output_buffer, 256);
        assert_eq!(back.sessions[0].eq, Some(EqSettings::default()));
        assert!(back.sessions[0].eq_post.is_none());
        assert_eq!(back.plugins.len(), 2);
        assert_eq!(back.plugins[1].format, PluginFormat::Clap);
        assert_eq!(back.plugins[1].class_id.as_deref(), Some("ABCD"));
        assert!(back.plugins[1].bypass);
        assert_eq!(back.plugins[1].mix, 0.5);
        assert_eq!(back.plugins[1].params[0].id, 7);
    }

    #[test]
    fn preset_missing_fields_still_loads() {
        // Written before `note`, `buffer` and the output side existed, and
        // before the schema version: used to fail the whole preferences.
        let raw = r#"(auto_start_bridge:true,sessions:[(name:"Old",input:"In",output:"Out",rate:44100)],session_name:"Mine",sample_rate:44100)"#;
        let loaded = load_preferences(Some(raw), BACKUP);
        assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
        let preferences = loaded.preferences.expect("readable");
        assert!(preferences.auto_start_bridge);
        assert_eq!(preferences.schema_version, PREFERENCES_SCHEMA);
        assert_eq!(preferences.sessions.len(), 1);
        let old = &preferences.sessions[0];
        assert_eq!(old.name, "Old");
        assert_eq!(old.rate, 44_100);
        assert_eq!(old.buffer, 128);
        assert_eq!(old.note, "");
        assert_eq!(old.output_rate, 0);
    }

    #[test]
    fn corrupt_preset_is_skipped_and_the_rest_kept() {
        let raw = r#"(sessions:[(name:"Good one",rate:48000,buffer:64),(name:12,rate:"fast"),"junk",(name:"Good two")],session_name:"Mine",buffer_size:256,plugins:[(path:"C:/A.vst3",name:"A")])"#;
        let loaded = load_preferences(Some(raw), BACKUP);
        let preferences = loaded.preferences.expect("readable");
        let names: Vec<_> = preferences
            .sessions
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(names, ["Good one", "Good two"]);
        assert_eq!(preferences.sessions[0].buffer, 64);
        assert_eq!(preferences.session_name, "Mine");
        assert_eq!(preferences.buffer_size, 256);
        assert_eq!(preferences.plugins.len(), 1);
        assert_eq!(loaded.notices.len(), 1);
        assert!(
            loaded.notices[0].contains("Skipped 2 saved patch"),
            "{}",
            loaded.notices[0]
        );
        // The skipped entries would vanish on the next save: keep the original.
        assert_eq!(loaded.backup, Some((BACKUP.to_owned(), raw.to_owned())));
    }

    #[test]
    fn corrupt_plugin_entry_is_skipped_and_missing_name_filled_in() {
        let raw = r#"(sessions:[(name:"Clean")],plugins:[(path:"C:/Plugins/Amp.vst3",format:clap,name:"Amp",mix:0.75),(name:"No path"),(path:"C:/B.vst3",name:"B",mix:"loud"),(path:"C:/Plugins/Verb.vst3")])"#;
        let loaded = load_preferences(Some(raw), BACKUP);
        let preferences = loaded.preferences.expect("readable");
        assert_eq!(preferences.sessions.len(), 1);
        assert_eq!(preferences.plugins.len(), 2);
        assert_eq!(preferences.plugins[0].name, "Amp");
        assert_eq!(preferences.plugins[0].format, PluginFormat::Clap);
        assert_eq!(preferences.plugins[0].mix, 0.75);
        assert_eq!(preferences.plugins[1].name, "Verb");
        assert_eq!(preferences.plugins[1].mix, 1.0);
        assert!(
            loaded.notices[0].contains("2 plugin insert"),
            "{}",
            loaded.notices[0]
        );
        assert!(loaded.backup.is_some());
    }

    #[test]
    fn unreadable_preferences_are_kept_not_overwritten() {
        let raw = r#"(sessions:[(name:"Clean")],sample_rate:"#;
        let mut storage = MemoryStorage::default();
        eframe::Storage::set_string(&mut storage, PREFERENCES_KEY, raw.to_owned());

        let mut loaded = load_preferences(
            eframe::Storage::get_string(&storage, PREFERENCES_KEY).as_deref(),
            BACKUP,
        );
        assert!(loaded.preferences.is_none());
        assert_eq!(loaded.notices.len(), 1);
        assert!(loaded.notices[0].contains(BACKUP));

        // The app's autosave with the defaults it fell back to.
        store_preferences(&mut storage, &Preferences::default(), &mut loaded.backup);
        assert_eq!(storage.0.get(BACKUP).map(String::as_str), Some(raw));
        let saved = storage.0.get(PREFERENCES_KEY).expect("saved");
        assert!(load_preferences(Some(saved), BACKUP).preferences.is_some());

        // Written once: later saves do not touch the copy again.
        assert!(loaded.backup.is_none());
        storage
            .0
            .insert(BACKUP.to_owned(), "moved by the user".to_owned());
        store_preferences(&mut storage, &Preferences::default(), &mut loaded.backup);
        assert_eq!(storage.0[BACKUP], "moved by the user");
    }

    #[test]
    fn newer_layout_keeps_a_copy() {
        let raw = r#"(schema_version:99,session_name:"Future",some_new_field:[1,2])"#;
        let loaded = load_preferences(Some(raw), BACKUP);
        let preferences = loaded.preferences.expect("readable");
        assert_eq!(preferences.session_name, "Future");
        assert_eq!(preferences.schema_version, PREFERENCES_SCHEMA);
        assert_eq!(loaded.notices.len(), 1);
        assert!(loaded.backup.is_some());
    }

    #[test]
    fn nothing_stored_is_quiet() {
        let loaded = load_preferences(None, BACKUP);
        assert!(loaded.preferences.is_none());
        assert!(loaded.backup.is_none());
        assert!(loaded.notices.is_empty());
    }
}
