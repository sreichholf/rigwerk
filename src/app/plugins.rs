//! Finding VST3, CLAP and LV2 bundles and adding them to a path's effect
//! chain.  Favourites, recent effects, where the next one is added, and how
//! the browser ranks its rows live here.  The screen draws them; this file
//! does not.

use crate::app::LogTone;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;

use crate::app::paths::EffectKey;
use crate::app::screen::Drawer;
use crate::app::RigWerkApp;
use crate::audio::clap::{self, ClapCandidate};
use crate::audio::lv2::{self, Lv2Candidate};
use crate::audio::vst3;
use crate::audio::{PluginFormat, PluginPref};

/// How many recently used effects are kept.
const RECENT_LIMIT: usize = 10;

/// How long a scan may run before the browser gives up on it.  A library of
/// several hundred bundles finishes in seconds even with cold caches, so a
/// scan still silent after this holds a bundle that will never answer, and
/// the user needs Rescan back.
///
/// This is the scan's own budget ([`scan_library`]) and not only the
/// browser's: VST3 and LV2 run in this process and CLAP runs in a child
/// bounded by what is left of it, so a hung `clap_entry.init` or an abort in
/// a bundle neither holds the process-wide module lock nor ends the app.  A
/// VST3 or LV2 bundle that hangs still leaves the scan thread stuck, and the
/// browser's deadline only unlatches the button.
const SCAN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// How long the browser keeps waiting past [`SCAN_TIMEOUT`].  The child is
/// killed at the deadline and its failure rows still have to reach the
/// channel; without the grace the browser would drop the receiver first and
/// show its generic "stopped waiting" notice instead of the rows.
const SCAN_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

/// Time kept back from the CLAP child's budget so this process can kill it,
/// read its result and send the rows before the scan's own deadline passes.
const CLAP_SAVE_RESERVE: std::time::Duration = std::time::Duration::from_secs(15);

/// One effect in the favourites or the recent list.  The path, the format,
/// the class and the name together name it: two classes in one bundle stay
/// distinct.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct EffectId {
    pub(crate) path: String,
    pub(crate) format: PluginFormat,
    #[serde(default)]
    pub(crate) class_id: Option<String>,
    pub(crate) name: String,
}

impl EffectId {
    pub(crate) fn from_candidate(candidate: &PluginCandidate) -> Self {
        Self {
            path: candidate.path.to_string_lossy().into_owned(),
            format: candidate.format,
            class_id: candidate.class_id.clone(),
            name: candidate.name.clone(),
        }
    }

    fn same(&self, other: &Self) -> bool {
        self.path == other.path
            && self.format == other.format
            && self.class_id == other.class_id
            && self.name.eq_ignore_ascii_case(&other.name)
    }

    pub(crate) fn matches(&self, candidate: &PluginCandidate) -> bool {
        self.same(&Self::from_candidate(candidate))
    }
}

/// Which slice of the library the browser is showing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum EffectFilter {
    #[default]
    All,
    Favourites,
    Recent,
    /// The first category tag the scan reported.
    Category(String),
}

/// Drops empty ids and duplicates.  Favourites keep the first of each.
/// Recent keeps the latest of each, and only the last [`RECENT_LIMIT`].
pub(crate) fn sanitize_effect_ids(ids: &mut Vec<EffectId>, recent: bool) {
    ids.retain(|id| !id.path.trim().is_empty() && !id.name.trim().is_empty());
    let mut kept = Vec::new();
    if recent {
        for id in ids.drain(..).rev() {
            if kept.iter().any(|have: &EffectId| have.same(&id)) {
                continue;
            }
            kept.push(id);
            if kept.len() == RECENT_LIMIT {
                break;
            }
        }
        kept.reverse();
    } else {
        for id in ids.drain(..) {
            if !kept.iter().any(|have: &EffectId| have.same(&id)) {
                kept.push(id);
            }
        }
    }
    *ids = kept;
}

/// The first category tag, before a `·`.  Empty when the scan gave none.
pub(crate) fn category_tag(category: &str) -> &str {
    category.split('·').next().unwrap_or("").trim()
}

/// Category chips after All, Favourites and Recent, in alphabetical order.
pub(crate) fn category_tags(library: &[PluginCandidate]) -> Vec<String> {
    let mut tags: Vec<String> = library
        .iter()
        .map(|candidate| category_tag(&candidate.category).to_owned())
        .filter(|tag| !tag.is_empty())
        .collect();
    tags.sort_by_key(|tag| tag.to_lowercase());
    tags.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    tags.retain(|tag| !matches!(tag.to_lowercase().as_str(), "all" | "favourites" | "recent"));
    tags
}

/// Where the next effect goes, as the header says it.
pub(crate) fn position_phrase(names: &[String], position: usize) -> String {
    if names.is_empty() || position == 0 {
        "at the start".to_owned()
    } else {
        let before = position.min(names.len()) - 1;
        format!("after {}", names[before])
    }
}

/// The library rows the browser shows, best match first.
///
/// A search matches the name, the maker, the format and the category.  A
/// name that starts with the query ranks above one that contains it, and
/// a favourite ranks above the rest of that tier.  Recent use is the
/// Recent chip, not another sort of the whole library.
#[allow(clippy::too_many_arguments)]
pub(crate) fn visible_effects<'a>(
    library: &'a [PluginCandidate],
    query: &str,
    filter: &EffectFilter,
    show_vst3: bool,
    show_clap: bool,
    show_lv2: bool,
    favourites: &[EffectId],
    recent: &[EffectId],
) -> Vec<&'a PluginCandidate> {
    let query = query.trim().to_lowercase();
    let mut rows: Vec<&PluginCandidate> = library
        .iter()
        .filter(|candidate| format_shown(candidate.format, show_vst3, show_clap, show_lv2))
        .filter(|candidate| filter_shown(candidate, filter, favourites, recent))
        .filter(|candidate| match_rank(candidate, &query).is_some())
        .collect();
    if matches!(filter, EffectFilter::Recent) {
        rows.sort_by(|left, right| {
            recent_rank(left, recent)
                .cmp(&recent_rank(right, recent))
                .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
        });
    } else {
        rows.sort_by(|left, right| {
            rank_key(left, &query, favourites).cmp(&rank_key(right, &query, favourites))
        });
    }
    rows
}

fn format_shown(format: PluginFormat, show_vst3: bool, show_clap: bool, show_lv2: bool) -> bool {
    match format {
        PluginFormat::Vst3 => show_vst3,
        PluginFormat::Clap => show_clap,
        PluginFormat::Lv2 => show_lv2,
    }
}

fn filter_shown(
    candidate: &PluginCandidate,
    filter: &EffectFilter,
    favourites: &[EffectId],
    recent: &[EffectId],
) -> bool {
    match filter {
        EffectFilter::All => true,
        EffectFilter::Favourites => favourites.iter().any(|id| id.matches(candidate)),
        EffectFilter::Recent => recent.iter().any(|id| id.matches(candidate)),
        EffectFilter::Category(tag) => category_tag(&candidate.category).eq_ignore_ascii_case(tag),
    }
}

/// `0` is a name prefix, `1` a name containing the query, `2` another
/// field.  `None` does not match.  An empty query matches everything as `1`.
fn match_rank(candidate: &PluginCandidate, query: &str) -> Option<u8> {
    if query.is_empty() {
        return Some(1);
    }
    let name = candidate.name.to_lowercase();
    if name.starts_with(query) {
        return Some(0);
    }
    if name.contains(query) {
        return Some(1);
    }
    let format = candidate.format.label().to_lowercase();
    if candidate.vendor.to_lowercase().contains(query)
        || candidate.category.to_lowercase().contains(query)
        || format.contains(query)
    {
        Some(2)
    } else {
        None
    }
}

/// Sort key: closer name match, then favourite, then name.
fn rank_key(candidate: &PluginCandidate, query: &str, favourites: &[EffectId]) -> (u8, u8, String) {
    let matched = match_rank(candidate, query).unwrap_or(3);
    let favourite = if favourites.iter().any(|id| id.matches(candidate)) {
        0
    } else {
        1
    };
    (matched, favourite, candidate.name.to_lowercase())
}

/// Newest recent effect first.  Missing ids sort last.
fn recent_rank(candidate: &PluginCandidate, recent: &[EffectId]) -> usize {
    recent
        .iter()
        .rev()
        .position(|id| id.matches(candidate))
        .unwrap_or(usize::MAX)
}

/// One plugin a scan found.  `class_id` is set when a bundle contains more
/// than one audio class (VST3), holds the plugin id (CLAP), or the plugin URI
/// (LV2), so the right one can be opened later.
#[derive(Clone, Debug)]
pub(crate) struct PluginCandidate {
    pub(crate) path: PathBuf,
    pub(crate) format: PluginFormat,
    pub(crate) class_id: Option<String>,
    pub(crate) name: String,
    pub(crate) vendor: String,
    pub(crate) category: String,
    /// The channel count the scan read without loading the binary: CLAP
    /// features or VST3 sub-categories saying "mono" or "stereo", or an LV2
    /// plugin's count of audio input ports.  `None` when those say nothing
    /// (or, for LV2, when there are more than two inputs).
    pub(crate) channels: Option<u32>,
    /// What went wrong reading this candidate, when the scan could still
    /// list it: a VST3 `moduleinfo.json` it could not read or parse, an LV2
    /// plugin that needs an unsupported feature, a non-optional CV port, or
    /// more audio ports than the host supplies, or a CLAP bundle whose
    /// module could not be loaded or panicked.
    pub(crate) scan_error: Option<String>,
}

/// The channel count a plugin's own tags declare: "mono" is 1, "stereo"
/// is 2, case-insensitive.  `None` for neither, or both.
fn declared_channels(tags: &[String]) -> Option<u32> {
    let has = |word: &str| tags.iter().any(|tag| tag.eq_ignore_ascii_case(word));
    match (has("mono"), has("stereo")) {
        (true, false) => Some(1),
        (false, true) => Some(2),
        _ => None,
    }
}

pub(crate) struct PluginBrowser {
    pub(crate) library: Vec<PluginCandidate>,
    /// A scan is running and not yet given up on.  True only while
    /// `receiver` is `Some`; [`PluginBrowser::stop_waiting`] is the only
    /// place that clears it.
    pub(crate) scanning: bool,
    pub(crate) receiver: Option<mpsc::Receiver<Vec<PluginCandidate>>>,
    /// When a running scan is given up on if no result has arrived.
    pub(crate) scan_deadline: Option<std::time::Instant>,
    pub(crate) search: String,
    pub(crate) path_input: String,
    pub(crate) notice: Option<(String, std::time::Instant)>,
    /// Path slot new effects are added to.
    pub(crate) path: usize,
    /// Index in that path's chain.  The length means after the last effect.
    pub(crate) position: usize,
    pub(crate) filter: EffectFilter,
    pub(crate) show_vst3: bool,
    pub(crate) show_clap: bool,
    pub(crate) show_lv2: bool,
    /// The row ↑/↓ moves and Enter adds.
    pub(crate) chosen: usize,
    /// The search field takes focus on the frame the browser opens.
    pub(crate) focus_search: bool,
    /// The chosen row should be scrolled into view.
    pub(crate) follow: bool,
    pub(crate) favourites: Vec<EffectId>,
    pub(crate) recent: Vec<EffectId>,
    /// The screenshot hook has already opened the column.
    pub(crate) screenshot_ready: bool,
}

impl Default for PluginBrowser {
    fn default() -> Self {
        Self {
            library: Vec::new(),
            scanning: false,
            receiver: None,
            scan_deadline: None,
            search: String::new(),
            path_input: String::new(),
            notice: None,
            path: 0,
            position: 0,
            filter: EffectFilter::All,
            show_vst3: true,
            show_clap: true,
            show_lv2: true,
            chosen: 0,
            focus_search: false,
            follow: false,
            favourites: Vec::new(),
            recent: Vec::new(),
            screenshot_ready: false,
        }
    }
}

/// The VST3, CLAP and LV2 folders a scan reads, for Setup.
pub(crate) fn plugin_scan_folders() -> Vec<String> {
    let mut folders = Vec::new();
    for path in crate::audio::vst3::search_roots() {
        push_folder(&mut folders, &path);
    }
    for path in crate::audio::clap::scan_roots() {
        push_folder(&mut folders, &path);
    }
    for path in lv2::scan_roots() {
        push_folder(&mut folders, &path);
    }
    folders
}

fn push_folder(folders: &mut Vec<String>, path: &std::path::Path) {
    let text = path.display().to_string();
    if !text.is_empty() && folders.iter().all(|have| have != &text) {
        folders.push(text);
    }
}

impl PluginBrowser {
    pub(crate) fn start_scan(&mut self) {
        self.start_scan_with(scan_library);
    }

    /// Starts `scan` on its own thread and arms the deadline.  The scan gets
    /// its own budget, computed before the thread starts, so each step can see
    /// how much time is left instead of assuming the browser's deadline covers
    /// it.  A test passes a body that does no I/O, so the suite never reads
    /// (and loads) the plugins installed on the machine; [`start_scan`] passes
    /// the real scan.
    fn start_scan_with(&mut self, scan: fn(std::time::Instant) -> Vec<PluginCandidate>) {
        if self.receiver.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.receiver = Some(rx);
        self.scanning = true;
        let deadline = std::time::Instant::now() + SCAN_TIMEOUT;
        self.scan_deadline = Some(deadline + SCAN_GRACE);
        let spawned = thread::Builder::new()
            .name("rigwerk-plugin-scan".to_owned())
            .spawn(move || {
                let _ = tx.send(scan(deadline));
            });
        if spawned.is_err() {
            self.stop_waiting();
            self.notice = Some((
                "Could not start the plugin scan.".to_owned(),
                std::time::Instant::now(),
            ));
        }
    }

    /// Stops waiting for the scan.  The only place `scanning`, `receiver`
    /// and `scan_deadline` are cleared, so no path can leave the browser
    /// waiting forever, and dropping the receiver is what discards a result
    /// the thread sends after that.  Safe to call with no scan running.
    pub(crate) fn stop_waiting(&mut self) {
        self.receiver = None;
        self.scanning = false;
        self.scan_deadline = None;
    }
}

/// The whole library, with `deadline` as the scan's own budget.
///
/// CLAP runs last, after the two in-process formats: it is the only part that
/// runs third-party code, and it is bounded by what is left of the budget once
/// VST3 and LV2 have had their turn.  The browser sorts the rows, so the order
/// here does not matter.
fn scan_library(deadline: std::time::Instant) -> Vec<PluginCandidate> {
    let mut candidates = Vec::new();
    for path in vst3::find_bundles(&vst3::search_roots()) {
        candidates.extend(candidates_from_bundle(&path));
    }
    for candidate in lv2::scan_library() {
        candidates.push(candidate_from_lv2(candidate));
    }
    let remaining = deadline
        .saturating_duration_since(std::time::Instant::now())
        .saturating_sub(CLAP_SAVE_RESERVE);
    for candidate in clap::scan_library_isolated(remaining) {
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
        // A bundle that could not be read has no plugin id; its row exists
        // only to show `scan_error`, so it must not look loadable.
        class_id: Some(candidate.plugin_id).filter(|id| !id.trim().is_empty()),
        name: candidate.name,
        vendor: candidate.vendor,
        category: candidate.category,
        channels: declared_channels(&candidate.features),
        scan_error: candidate.scan_error,
    }
}

fn candidate_from_lv2(candidate: Lv2Candidate) -> PluginCandidate {
    PluginCandidate {
        path: candidate.path,
        format: PluginFormat::Lv2,
        class_id: Some(candidate.plugin_uri),
        name: candidate.name,
        vendor: candidate.vendor,
        category: candidate.category,
        channels: candidate.channels,
        scan_error: candidate.scan_error,
    }
}

/// The effects in one bundle: CLAP by its `.clap` extension, LV2 by its
/// `.lv2` folder, else VST3.
fn candidates_from_path(path: &Path) -> Vec<PluginCandidate> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default();
    if extension.eq_ignore_ascii_case("clap") {
        candidates_from_clap_bundle(path)
    } else if extension.eq_ignore_ascii_case("lv2") {
        candidates_from_lv2_bundle(path)
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

fn candidates_from_lv2_bundle(path: &Path) -> Vec<PluginCandidate> {
    match lv2::scan_bundle(path) {
        Ok(found) => found.into_iter().map(candidate_from_lv2).collect(),
        Err(_) => Vec::new(),
    }
}

fn candidates_from_bundle(path: &Path) -> Vec<PluginCandidate> {
    // A bundle that only ships another architecture's binary cannot be
    // opened by this build, so leave it out instead of offering a dead row.
    if !vst3::has_loadable_binary(path) {
        return Vec::new();
    }
    let fallback_name = path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("VST3 plugin")
        .to_owned();
    let mut found = Vec::new();
    // Without `moduleinfo.json` the bundle is one row with no class id,
    // and the load takes its first audio class (`audio/vst3/scan.rs`).
    // A module that declares audio classes, all of them instruments, gets
    // no row: that fallback would load an instrument.
    let mut declares_audio = false;
    let (info, scan_error) = match vst3::read_module_info(path) {
        Ok(info) => (info, None),
        Err(error) => (None, Some(error)),
    };
    if let Some(info) = info {
        for class in &info.classes {
            if !class.category.eq_ignore_ascii_case("Audio Module Class") {
                continue;
            }
            declares_audio = true;
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
                channels: declared_channels(&class.sub_categories),
                scan_error: None,
            });
        }
    }
    if found.is_empty() && !declares_audio {
        found.push(PluginCandidate {
            path: path.to_path_buf(),
            format: PluginFormat::Vst3,
            class_id: None,
            name: fallback_name,
            vendor: String::new(),
            category: "VST3 effect".to_owned(),
            channels: None,
            scan_error,
        });
    }
    found
}

/// Why the browser cannot add or favourite this row, or `None` when it can.
/// A row with a `scan_error` exists only to report it; a CLAP row with no id
/// has nothing to open.  The VST3 fallback row is the one row without a class
/// id that still loads: its module names its audio class at load time.
fn row_problem(candidate: &PluginCandidate) -> Option<String> {
    if let Some(error) = &candidate.scan_error {
        return Some(error.clone());
    }
    (candidate.format == PluginFormat::Clap && candidate.class_id.is_none())
        .then(|| "That CLAP bundle lists no plugin to open.".to_owned())
}

/// One effect the screenshot column can show when nothing was scanned.
fn screenshot_effect() -> PluginPref {
    PluginPref {
        path: "Limiter.vst3".to_owned(),
        format: PluginFormat::Vst3,
        class_id: Some("LIMITER".to_owned()),
        name: "Limiter Stereo".to_owned(),
        vendor: "Rig".to_owned(),
        category: "Dynamics".to_owned(),
        ..PluginPref::default()
    }
}

fn screenshot_candidate(
    name: &str,
    vendor: &str,
    category: &str,
    format: PluginFormat,
    channels: Option<u32>,
    class_id: Option<&str>,
    scan_error: Option<&str>,
) -> PluginCandidate {
    let path = match format {
        PluginFormat::Vst3 => format!("{name}.vst3"),
        PluginFormat::Clap => format!("{name}.clap"),
        PluginFormat::Lv2 => format!("{name}.lv2"),
    };
    PluginCandidate {
        path: path.into(),
        format,
        class_id: class_id.map(str::to_owned),
        name: name.to_owned(),
        vendor: vendor.to_owned(),
        category: category.to_owned(),
        channels,
        scan_error: scan_error.map(str::to_owned),
    }
}

/// Sample rows for a screenshot with no scanned library.  The first matches
/// [`screenshot_effect`], so the row can say which path already uses it.
fn screenshot_library() -> Vec<PluginCandidate> {
    let mut limiter = screenshot_candidate(
        "Limiter Stereo",
        "Rig",
        "Dynamics",
        PluginFormat::Vst3,
        Some(2),
        Some("LIMITER"),
        None,
    );
    limiter.path = "Limiter.vst3".into();
    vec![
        limiter,
        screenshot_candidate(
            "Room",
            "Rig",
            "Reverb",
            PluginFormat::Vst3,
            Some(2),
            Some("ROOM"),
            None,
        ),
        screenshot_candidate(
            "Amp",
            "Rig",
            "Distortion",
            PluginFormat::Clap,
            Some(1),
            Some("AMP"),
            None,
        ),
        screenshot_candidate(
            "Delay",
            "Rig",
            "Delay",
            PluginFormat::Vst3,
            None,
            None,
            Some("Could not read the bundle."),
        ),
    ]
}

impl RigWerkApp {
    /// Opens the effect browser for `path`, inserting at `position` (the
    /// chain length means after the last effect).  The search field is
    /// focused on the next frame.  This is the one place that opens the
    /// browser: its open state is `Drawer::Browser`, so the drawer is the
    /// only flag and every reader asks that.
    pub(crate) fn open_effect_browser(&mut self, path: usize, position: usize) {
        let _ = self.select_path(path);
        let count = self.engine.plugin_views(self.selected_path).len();
        self.chrome.drawer = Some(Drawer::Browser);
        self.plugin_browser.path = self.selected_path;
        self.plugin_browser.position = position.min(count);
        self.plugin_browser.focus_search = true;
        self.plugin_browser.chosen = 0;
        self.plugin_browser.follow = true;
        self.plugin_effect_at = Some(self.plugin_browser.position).filter(|at| *at < count);
    }

    /// The next place "change" offers, wrapping from the end to the start.
    pub(crate) fn cycle_browser_position(&mut self) {
        let count = self.engine.plugin_views(self.plugin_browser.path).len();
        self.plugin_browser.position = if count == 0 || self.plugin_browser.position >= count {
            0
        } else {
            self.plugin_browser.position + 1
        };
        self.plugin_effect_at = Some(self.plugin_browser.position).filter(|at| *at < count);
    }

    /// Adds `candidate` at the browser's position.  The browser stays open
    /// and the next add lands after this effect.  `open_editor` opens the
    /// effect's editor (or its controls, when it has no editor).  `false`
    /// when the path is gone or the effect could not be added.
    pub(crate) fn add_listed_effect(
        &mut self,
        candidate: &PluginCandidate,
        open_editor: bool,
    ) -> bool {
        // A row that reports a scan error cannot load, so repeating the
        // reason beats replacing it with a load error from the id or path.
        if let Some(problem) = row_problem(candidate) {
            self.plugin_browser.notice = Some((problem, std::time::Instant::now()));
            return false;
        }
        let path = self.plugin_browser.path;
        if self
            .paths
            .get(path)
            .and_then(|slot| slot.as_ref())
            .is_none()
        {
            self.plugin_browser.notice = Some((
                "Could not add: that path is not available.".to_owned(),
                std::time::Instant::now(),
            ));
            return false;
        }
        self.selected_path = path;
        let before = self.engine.plugin_views(path).len();
        let target = self.plugin_browser.position.min(before);
        self.plugin_effect_at = Some(target);
        let Some(index) = self.add_plugin_candidate(candidate) else {
            return false;
        };
        self.plugin_browser.position = index + 1;
        let count = self.engine.plugin_views(path).len();
        self.plugin_effect_at = Some(self.plugin_browser.position).filter(|at| *at < count);
        self.remember_recent(candidate);
        let name = self
            .paths
            .get(path)
            .and_then(|slot| slot.as_ref())
            .map(|slot| slot.name.clone())
            .unwrap_or_else(|| "the path".to_owned());
        self.plugin_browser.notice = Some((
            format!("Added {} to {name}.", candidate.name),
            std::time::Instant::now(),
        ));
        self.chain_selected = crate::app::chain::ChainSelection::Plugin(index);
        if open_editor {
            self.open_plugin_editor(index);
        }
        true
    }

    pub(crate) fn toggle_favourite(&mut self, candidate: &PluginCandidate) {
        let id = EffectId::from_candidate(candidate);
        if let Some(index) = self
            .plugin_browser
            .favourites
            .iter()
            .position(|have| have.same(&id))
        {
            self.plugin_browser.favourites.remove(index);
            return;
        }
        // A favourite of a row that cannot load would never match again.
        if let Some(problem) = row_problem(candidate) {
            self.plugin_browser.notice = Some((problem, std::time::Instant::now()));
            return;
        }
        self.plugin_browser.favourites.push(id);
    }

    fn remember_recent(&mut self, candidate: &PluginCandidate) {
        let id = EffectId::from_candidate(candidate);
        self.plugin_browser.recent.retain(|have| !have.same(&id));
        self.plugin_browser.recent.push(id);
        sanitize_effect_ids(&mut self.plugin_browser.recent, true);
    }

    /// Puts a stand-in effect on path `slot` for a screenshot of the effect
    /// sheet, when the chain is empty.  Screenshot runs only.
    pub(crate) fn add_screenshot_effect(&mut self, slot: usize) {
        if self.screenshot.is_some() && self.engine.plugin_views(slot).is_empty() {
            let _ = self.engine.add_plugin(slot, screenshot_effect());
        }
    }

    /// Puts a stand-in effect on path `slot`, for a screenshot of a long
    /// chain.  Screenshot runs only.
    pub(crate) fn add_screenshot_effect_always(&mut self, slot: usize) {
        if self.screenshot.is_some() {
            let _ = self.engine.add_plugin(slot, screenshot_effect());
        }
    }

    /// Opens the browser drawer for a `RIGWERK_DRAWER=browser` screenshot.
    /// An empty library is filled with sample rows so the column is not
    /// blank when the machine has no scanned effects.
    pub(crate) fn open_browser_for_screenshot(&mut self) {
        if self.screenshot.is_none() {
            return;
        }
        let Ok(value) = std::env::var("RIGWERK_DRAWER") else {
            return;
        };
        if value.trim() != "browser" {
            return;
        }
        if !self.plugin_browser.screenshot_ready {
            let slot = self.selected_path;
            let count = self.engine.plugin_views(slot).len();
            self.open_effect_browser(slot, count);
            self.plugin_browser.screenshot_ready = true;
        }
        if self.plugin_browser.library.is_empty() && !self.plugin_browser.scanning {
            self.plugin_browser.library = screenshot_library();
            if let Some(first) = self.plugin_browser.library.first() {
                let id = EffectId::from_candidate(first);
                if !self
                    .plugin_browser
                    .favourites
                    .iter()
                    .any(|have| have.same(&id))
                {
                    self.plugin_browser.favourites.push(id);
                }
            }
        }
    }

    pub(crate) fn begin_plugin_scan(&mut self) {
        self.plugin_browser.start_scan();
    }

    pub(crate) fn poll_plugin_scan(&mut self) {
        let received = self
            .plugin_browser
            .receiver
            .as_ref()
            .map(mpsc::Receiver::try_recv);
        match received {
            // No scan in flight.  Clearing here too keeps Rescan from ever
            // latching, whatever state the fields were left in.
            None => self.plugin_browser.stop_waiting(),
            // A result that has arrived is applied even after the deadline: a
            // slow scan on a network or antivirus-scanned drive is healthy,
            // and its rows are worth more than a fresh start.
            Some(Ok(library)) => {
                self.plugin_browser.library = library;
                self.plugin_browser.stop_waiting();
                self.push_log(
                    format!(
                        "Found {} plugin{} (VST3, CLAP and LV2).",
                        self.plugin_browser.library.len(),
                        if self.plugin_browser.library.len() == 1 {
                            ""
                        } else {
                            "s"
                        }
                    ),
                    LogTone::Teal,
                );
            }
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                self.plugin_browser.stop_waiting();
                self.plugin_browser.notice = Some((
                    "The plugin scan stopped unexpectedly.".to_owned(),
                    std::time::Instant::now(),
                ));
            }
            // Only a scan that is still silent is abandoned; the deadline is
            // what lets the user scan again after a bundle never returns.
            Some(Err(mpsc::TryRecvError::Empty)) => {
                let deadline_passed = self
                    .plugin_browser
                    .scan_deadline
                    .is_some_and(|deadline| std::time::Instant::now() >= deadline);
                if deadline_passed {
                    self.plugin_browser.stop_waiting();
                    self.plugin_browser.notice = Some((
                        "The plugin scan stopped waiting; a bundle is not answering.".to_owned(),
                        std::time::Instant::now(),
                    ));
                }
            }
        }
    }

    pub(crate) fn add_plugin_candidate(&mut self, candidate: &PluginCandidate) -> Option<usize> {
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
        self.install_plugin(pref)
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
            if self.chrome.drawer == Some(Drawer::Browser) {
                let extra = candidates.len().saturating_sub(1);
                if self.add_listed_effect(&candidate.clone(), false) && extra > 0 {
                    self.plugin_browser.notice = Some((
                        format!(
                            "Added {}. The bundle also contains {extra} more effect class{}.",
                            candidate.name,
                            if extra == 1 { "" } else { "es" }
                        ),
                        std::time::Instant::now(),
                    ));
                }
                self.plugin_browser.path_input.clear();
                return;
            }
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
    /// VST3, CLAP or LV2 bundle to the selected path.  `false` when the
    /// bundle holds no such effect.  Used by the screenshot setup.
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

    fn install_plugin(&mut self, pref: PluginPref) -> Option<usize> {
        let format = pref.format;
        let effect_at = self.plugin_effect_at.take();
        let path = self.selected_path;
        match self.engine.add_plugin(path, pref) {
            Ok(()) => {
                let mut index = self.engine.plugin_views(path).len().saturating_sub(1);
                if let Some(target) = effect_at {
                    while index > target {
                        self.engine.move_plugin(path, index, -1);
                        index -= 1;
                    }
                }
                self.chain_selected = crate::app::chain::ChainSelection::Plugin(index);
                // One path reads as before; with several, name the path.
                let target = if self.path_count() > 1 {
                    format!("path {}", self.path().name)
                } else {
                    "the master bus".to_owned()
                };
                self.push_log(
                    format!("{} effect added to {target}.", format.label()),
                    LogTone::Green,
                );
                self.plugin_browser.notice = Some((
                    format!("Plugin added to {target}."),
                    std::time::Instant::now(),
                ));
                Some(index)
            }
            Err(error) => {
                self.plugin_effect_at = effect_at;
                self.push_log(
                    format!("Could not add {} plugin: {error}", format.label()),
                    LogTone::Red,
                );
                self.plugin_browser.notice = Some((error, std::time::Instant::now()));
                None
            }
        }
    }

    /// The selected path's effect `index`.
    fn effect_key(&self, index: usize) -> EffectKey {
        EffectKey {
            path: self.selected_path,
            effect: index,
        }
    }

    /// A double-click on effect `index` of path `slot`: its editor or its
    /// controls, as the `effect_double_click` preference says.  The
    /// controls are the effect sheet.
    pub(crate) fn open_effect(&mut self, slot: usize, index: usize) {
        match self.effect_double_click {
            crate::app::settings::EffectOpen::Editor => {
                if self.select_path(slot) {
                    self.open_plugin_editor(index);
                }
            }
            crate::app::settings::EffectOpen::Controls => {
                self.open_sheet(crate::app::screen::Sheet::Effect { path: slot, index });
            }
        }
    }

    /// Moves effect `index` of path `slot` by `delta` places, the others
    /// closing up behind it (a drag, or Alt+↑ / Alt+↓).  An open effect
    /// sheet and the selection follow the effect.
    pub(crate) fn move_effect(&mut self, slot: usize, index: usize, delta: isize) {
        if !self.select_path(slot) {
            return;
        }
        let count = self.engine.plugin_views(slot).len();
        if index >= count {
            return;
        }
        let target = (index as isize + delta).clamp(0, count as isize - 1) as usize;
        if target == index {
            return;
        }
        // The chain swaps neighbours, so a longer move is one step at a time.
        let step = if target > index { 1 } else { -1 };
        let mut at = index;
        while at != target {
            self.move_plugin(at, step);
            at = (at as isize + step) as usize;
        }
        let follow = |at: usize| {
            if at == index {
                target
            } else if index < at && at <= target {
                at - 1
            } else if target <= at && at < index {
                at + 1
            } else {
                at
            }
        };
        if let crate::app::chain::ChainSelection::Plugin(at) = self.chain_selected {
            self.chain_selected = crate::app::chain::ChainSelection::Plugin(follow(at));
        }
        if let Some(crate::app::screen::Sheet::Effect { path, index: at }) = self.chrome.sheet {
            if path == slot {
                self.chrome.sheet = Some(crate::app::screen::Sheet::Effect {
                    path,
                    index: follow(at),
                });
            }
        }
    }

    /// Removes effect `index` of path `slot`.  Its sheet closes; a later
    /// effect's sheet follows it up.
    pub(crate) fn remove_effect(&mut self, slot: usize, index: usize) {
        if !self.select_path(slot) || index >= self.engine.plugin_views(slot).len() {
            return;
        }
        self.remove_plugin(index);
        let count = self.engine.plugin_views(slot).len();
        self.chain_selected = self.chain_selected.after_removal(index, count);
        if let Some(crate::app::screen::Sheet::Effect { path, index: at }) = self.chrome.sheet {
            if path == slot {
                self.chrome.sheet = match at.cmp(&index) {
                    std::cmp::Ordering::Equal => None,
                    std::cmp::Ordering::Greater => Some(crate::app::screen::Sheet::Effect {
                        path,
                        index: at - 1,
                    }),
                    std::cmp::Ordering::Less => self.chrome.sheet,
                };
            }
        }
    }

    /// Removes effect `index` of the selected path.
    pub(crate) fn remove_plugin(&mut self, index: usize) {
        let path = self.selected_path;
        // Editors are tied to a position in the chain: close the removed
        // effect's, and let a later effect's follow it as it slides up.
        let shift = |key: EffectKey| -> Option<EffectKey> {
            if key.path != path || key.effect < index {
                Some(key)
            } else if key.effect == index {
                None
            } else {
                Some(EffectKey {
                    effect: key.effect - 1,
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
        self.push_log("Effect removed.", LogTone::Amber);
    }

    pub(crate) fn move_plugin(&mut self, index: usize, delta: isize) {
        let path = self.selected_path;
        // Finished while its index still names the right effect.  Editors
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

    pub(crate) fn set_plugin_bypass(&mut self, path: usize, index: usize, bypass: bool) {
        self.engine.set_plugin_bypass(path, index, bypass);
    }

    pub(crate) fn set_plugin_mix(&mut self, index: usize, mix: f32) {
        self.engine.set_plugin_mix(self.selected_path, index, mix);
    }

    /// Opens the plugin's own editor for one effect of the selected path,
    /// loading a saved plugin first if the bridge has not been started yet.
    /// A plugin with no editor, or one whose editor cannot open here, opens
    /// the effect sheet's controls instead.
    pub(crate) fn open_plugin_editor(&mut self, index: usize) {
        let key = self.effect_key(index);
        self.engine.ensure_plugin_loaded(key.path, index);
        let usable = self
            .engine
            .plugin_views(key.path)
            .get(index)
            .is_some_and(|view| view.has_editor && view.error.is_none());
        if usable && self.open_plugin_gui(key) {
            self.plugin_editor = None;
            return;
        }
        if self.plugin_editor != Some(key) {
            // A different effect: the filter belongs to the one opened before.
            self.plugin_param_filter.clear();
        }
        self.plugin_editor = Some(key);
        self.open_sheet(crate::app::screen::Sheet::Effect {
            path: key.path,
            index: key.effect,
        });
    }

    pub(crate) fn set_plugin_parameter(&mut self, key: EffectKey, id: u32, value: f64) {
        if let Err(error) = self
            .engine
            .set_plugin_parameter(key.path, key.effect, id, value)
        {
            self.plugin_browser.notice = Some((error, std::time::Instant::now()));
        }
    }

    pub(crate) fn reset_plugin_parameters(&mut self, key: EffectKey) {
        if let Err(error) = self.engine.reset_plugin_parameters(key.path, key.effect) {
            self.plugin_browser.notice = Some((error, std::time::Instant::now()));
        } else {
            self.push_log(
                "Plugin controls reset to the plugin defaults.",
                LogTone::Teal,
            );
        }
    }
}

#[cfg(test)]
mod tests;
