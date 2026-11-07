//! Finding VST3 and CLAP bundles and adding them to the selected path's
//! insert chain.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;

use crate::app::paths::InsertKey;
use crate::app::RigWerkApp;
use crate::audio::clap::{self, ClapCandidate};
use crate::audio::{PluginFormat, PluginPref};
use crate::ui::theme::{AMBER, GREEN, RED, TEAL};

/// One plugin a scan found.  `class_id` is set when a bundle contains more
/// than one audio class (VST3), or holds the plugin id (CLAP), so the right
/// one can be opened later.
#[derive(Clone, Debug)]
pub(crate) struct PluginCandidate {
    pub(crate) path: PathBuf,
    pub(crate) format: PluginFormat,
    pub(crate) class_id: Option<String>,
    pub(crate) name: String,
    pub(crate) vendor: String,
    pub(crate) category: String,
}

#[derive(Default)]
pub(crate) struct PluginBrowser {
    pub(crate) library: Vec<PluginCandidate>,
    pub(crate) scanning: bool,
    pub(crate) receiver: Option<mpsc::Receiver<Vec<PluginCandidate>>>,
    pub(crate) search: String,
    pub(crate) path_input: String,
    pub(crate) notice: Option<(String, std::time::Instant)>,
}

impl PluginBrowser {
    pub(crate) fn start_scan(&mut self) {
        if self.scanning {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.scanning = true;
        self.receiver = Some(rx);
        let _ = thread::Builder::new()
            .name("rigwerk-plugin-scan".to_owned())
            .spawn(move || {
                let _ = tx.send(scan_library());
            });
    }
}

fn scan_library() -> Vec<PluginCandidate> {
    let mut roots = vst3_host::discovery::scan_standard_paths();
    if let Ok(extra) = std::env::var("RIGWERK_VST3_PATH") {
        for part in extra.split(';') {
            let part = part.trim();
            if !part.is_empty() {
                roots.push(PathBuf::from(part));
            }
        }
    }
    let paths = vst3_host::discovery::scan_directories(&roots).unwrap_or_default();
    let mut candidates = Vec::new();
    for path in paths {
        candidates.extend(candidates_from_bundle(&path));
    }
    for candidate in clap::scan_library() {
        candidates.push(candidate_from_clap(candidate));
    }
    candidates.sort_by_cached_key(|candidate| candidate.name.to_lowercase());
    candidates.dedup_by(|a, b| {
        a.path == b.path
            && a.format == b.format
            && a.class_id == b.class_id
            && a.name.eq_ignore_ascii_case(&b.name)
    });
    candidates
}

fn candidate_from_clap(candidate: ClapCandidate) -> PluginCandidate {
    PluginCandidate {
        path: candidate.path,
        format: PluginFormat::Clap,
        class_id: Some(candidate.plugin_id),
        name: candidate.name,
        vendor: candidate.vendor,
        category: candidate.category,
    }
}

/// The effects in one bundle: CLAP by its `.clap` extension, else VST3.
fn candidates_from_path(path: &Path) -> Vec<PluginCandidate> {
    if path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("clap"))
    {
        candidates_from_clap_bundle(path)
    } else {
        candidates_from_bundle(path)
    }
}

fn candidates_from_clap_bundle(path: &Path) -> Vec<PluginCandidate> {
    match clap::scan_bundle(path) {
        Ok(found) => found.into_iter().map(candidate_from_clap).collect(),
        Err(_) => Vec::new(),
    }
}

fn candidates_from_bundle(path: &Path) -> Vec<PluginCandidate> {
    // A bundle that only ships another architecture's binary cannot be
    // opened by this build, so leave it out instead of offering a dead row.
    if vst3_host::discovery::get_vst3_binary_path(path).is_err() {
        return Vec::new();
    }
    let fallback_name = path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("VST3 plugin")
        .to_owned();
    let mut found = Vec::new();
    if let Ok(Some(info)) = vst3_host::discovery::read_module_info(path) {
        for class in &info.classes {
            if !class.category.eq_ignore_ascii_case("Audio Module Class") {
                continue;
            }
            let is_instrument = class
                .sub_categories
                .iter()
                .any(|category| category.eq_ignore_ascii_case("Instrument"));
            if is_instrument {
                continue;
            }
            let category = if class.sub_categories.is_empty() {
                "VST3 effect".to_owned()
            } else {
                class.sub_categories.join(" · ")
            };
            found.push(PluginCandidate {
                path: path.to_path_buf(),
                format: PluginFormat::Vst3,
                class_id: Some(class.class_id.clone()),
                name: if class.name.trim().is_empty() {
                    fallback_name.clone()
                } else {
                    class.name.clone()
                },
                vendor: if class.vendor.trim().is_empty() {
                    info.factory.vendor.clone()
                } else {
                    class.vendor.clone()
                },
                category,
            });
        }
    }
    if found.is_empty() {
        found.push(PluginCandidate {
            path: path.to_path_buf(),
            format: PluginFormat::Vst3,
            class_id: None,
            name: fallback_name,
            vendor: String::new(),
            category: "VST3 effect".to_owned(),
        });
    }
    found
}

impl RigWerkApp {
    pub(crate) fn begin_plugin_scan(&mut self) {
        self.plugin_browser.start_scan();
    }

    pub(crate) fn poll_plugin_scan(&mut self) {
        let Some(receiver) = &self.plugin_browser.receiver else {
            return;
        };
        match receiver.try_recv() {
            Ok(library) => {
                self.plugin_browser.library = library;
                self.plugin_browser.scanning = false;
                self.plugin_browser.receiver = None;
                self.push_log(
                    format!(
                        "Found {} plugin{} (VST3 and CLAP).",
                        self.plugin_browser.library.len(),
                        if self.plugin_browser.library.len() == 1 {
                            ""
                        } else {
                            "s"
                        }
                    ),
                    TEAL,
                );
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.plugin_browser.scanning = false;
                self.plugin_browser.receiver = None;
                self.plugin_browser.notice = Some((
                    "The plugin scan stopped unexpectedly.".to_owned(),
                    std::time::Instant::now(),
                ));
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
    }

    pub(crate) fn add_plugin_candidate(&mut self, candidate: &PluginCandidate) {
        let pref = PluginPref {
            path: candidate.path.to_string_lossy().into_owned(),
            format: candidate.format,
            class_id: candidate.class_id.clone(),
            name: candidate.name.clone(),
            vendor: candidate.vendor.clone(),
            category: candidate.category.clone(),
            bypass: false,
            mix: 1.0,
            params: Vec::new(),
            state: None,
        };
        self.install_plugin(pref);
    }

    pub(crate) fn add_plugin_from_path(&mut self) {
        let path = self.plugin_browser.path_input.trim().to_owned();
        if path.is_empty() {
            return;
        }
        let path = PathBuf::from(path);
        if !path.exists() {
            self.plugin_browser.notice = Some((
                format!("That path does not exist: {}", path.display()),
                std::time::Instant::now(),
            ));
            return;
        }
        let candidates = candidates_from_path(&path);
        if let Some(candidate) = candidates.first() {
            self.add_plugin_candidate(&candidate.clone());
            if candidates.len() > 1 {
                // A bundle with several classes: the user can pick another
                // one from the scanned library after a rescan.
                self.plugin_browser.notice = Some((
                    format!(
                        "Added {}. The bundle also contains {} more plugin class{}.",
                        candidate.name,
                        candidates.len() - 1,
                        if candidates.len() == 2 { "" } else { "es" }
                    ),
                    std::time::Instant::now(),
                ));
            }
            self.plugin_browser.path_input.clear();
        } else {
            self.plugin_browser.notice = Some((
                "That path does not contain an effect RigWerk can load.".to_owned(),
                std::time::Instant::now(),
            ));
        }
    }

    /// Adds the effect called `name` (the first one when `None`) from a
    /// VST3 or CLAP bundle to the selected path.  `false` when the bundle
    /// holds no such effect.  Used by the screenshot setup.
    pub(crate) fn add_plugin_from_bundle(&mut self, path: &Path, name: Option<&str>) -> bool {
        let candidates = candidates_from_path(path);
        let found = match name {
            Some(name) => candidates
                .iter()
                .find(|candidate| candidate.name.eq_ignore_ascii_case(name)),
            None => candidates.first(),
        };
        let Some(candidate) = found.cloned() else {
            return false;
        };
        let before = self.engine.plugin_views(self.selected_path).len();
        self.add_plugin_candidate(&candidate);
        self.engine.plugin_views(self.selected_path).len() > before
    }

    fn install_plugin(&mut self, pref: PluginPref) {
        let format = pref.format;
        let insert_at = self.plugin_insert_at.take();
        let path = self.selected_path;
        match self.engine.add_plugin(path, pref) {
            Ok(()) => {
                let mut index = self.engine.plugin_views(path).len().saturating_sub(1);
                if let Some(target) = insert_at {
                    while index > target {
                        self.engine.move_plugin(path, index, -1);
                        index -= 1;
                    }
                }
                self.chain_selected = crate::ui::pages::mixer::chain::ChainSelection::Plugin(index);
                // One path reads as before; with several, name the path.
                let target = if self.path_count() > 1 {
                    format!("path {}", self.path().name)
                } else {
                    "the master bus".to_owned()
                };
                self.push_log(
                    format!("{} insert added to {target}.", format.label()),
                    GREEN,
                );
                self.plugin_browser.notice = Some((
                    format!("Plugin added to {target}."),
                    std::time::Instant::now(),
                ));
            }
            Err(error) => {
                self.push_log(
                    format!("Could not add {} plugin: {error}", format.label()),
                    RED,
                );
                self.plugin_browser.notice = Some((error, std::time::Instant::now()));
            }
        }
    }

    /// The selected path's insert `index`.
    fn insert_key(&self, index: usize) -> InsertKey {
        InsertKey {
            path: self.selected_path,
            insert: index,
        }
    }

    /// Removes insert `index` of the selected path.
    pub(crate) fn remove_plugin(&mut self, index: usize) {
        let path = self.selected_path;
        // Editors are tied to a position in the chain: close the removed
        // insert's, and let a later insert's follow it as it slides up.
        let shift = |key: InsertKey| -> Option<InsertKey> {
            if key.path != path || key.insert < index {
                Some(key)
            } else if key.insert == index {
                None
            } else {
                Some(InsertKey {
                    insert: key.insert - 1,
                    ..key
                })
            }
        };
        if let Some(key) = self.plugin_gui.as_ref().map(|gui| gui.key) {
            match shift(key) {
                Some(moved) => {
                    if let Some(gui) = self.plugin_gui.as_mut() {
                        gui.key = moved;
                    }
                }
                None => self.plugin_gui = None,
            }
        }
        self.engine.remove_plugin(path, index);
        self.plugin_editor = self.plugin_editor.and_then(shift);
        if self.plugin_editor.is_none() {
            self.plugin_param_filter.clear();
        }
        self.push_log("Plugin insert removed.", AMBER);
    }

    pub(crate) fn move_plugin(&mut self, index: usize, delta: isize) {
        let path = self.selected_path;
        // Finished while its index still names the right insert.  Editors
        // of other paths are not affected.
        if self
            .plugin_gui
            .as_ref()
            .is_some_and(|gui| gui.key.path == path)
        {
            self.finish_plugin_gui();
        }
        self.engine.move_plugin(path, index, delta);
        if self.plugin_editor.is_some_and(|key| key.path == path) {
            self.plugin_editor = None;
            self.plugin_param_filter.clear();
        }
    }

    pub(crate) fn set_plugin_bypass(&mut self, index: usize, bypass: bool) {
        self.engine
            .set_plugin_bypass(self.selected_path, index, bypass);
    }

    pub(crate) fn set_plugin_mix(&mut self, index: usize, mix: f32) {
        self.engine.set_plugin_mix(self.selected_path, index, mix);
    }

    /// Opens the control panel for one insert of the selected path, loading
    /// a saved plugin first if the bridge has not been started yet.  When
    /// the person prefers plugin interfaces, a plugin that can embed its own
    /// view opens that instead.
    pub(crate) fn open_plugin_editor(&mut self, index: usize) {
        let key = self.insert_key(index);
        self.engine.ensure_plugin_loaded(key.path, index);
        if self.plugin_editor != Some(key) {
            // A different insert: the plugin editor and the filter belong to
            // the one that was open before.
            self.finish_plugin_gui();
            self.plugin_param_filter.clear();
        }
        if self.prefer_plugin_interface {
            let views = self.engine.plugin_views(key.path);
            let usable = views
                .get(index)
                .is_some_and(|view| view.has_editor && view.error.is_none());
            if usable && self.open_plugin_gui(key) {
                self.plugin_editor = None;
                return;
            }
        }
        self.plugin_editor = Some(key);
    }

    pub(crate) fn set_plugin_parameter(&mut self, key: InsertKey, id: u32, value: f64) {
        if let Err(error) = self
            .engine
            .set_plugin_parameter(key.path, key.insert, id, value)
        {
            self.plugin_browser.notice = Some((error, std::time::Instant::now()));
        }
    }

    pub(crate) fn reset_plugin_parameters(&mut self, key: InsertKey) {
        if let Err(error) = self.engine.reset_plugin_parameters(key.path, key.insert) {
            self.plugin_browser.notice = Some((error, std::time::Instant::now()));
        } else {
            self.push_log("Plugin controls reset to the plugin defaults.", TEAL);
        }
    }
}
