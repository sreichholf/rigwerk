//! Which sheet and drawer are open, and the title-bar drafts.
//!
//! The page (`Page`) is the console, the tuner or Setup.  The sheet and the
//! drawer sit over it.  *Setup* opens the Setup screen at a section; Done
//! and Esc come back to the console.

use std::time::{Duration, Instant};

use crate::app::chain::{neighbour_stage, ChainSelection, ChainStage};
use crate::app::RigWerkApp;

/// Bottom sheet: one thing at a time (`docs/ui-spec.md` §4.3, §6).  A path
/// is its engine slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Sheet {
    Diagnostics,
    /// *Pre EQ*, or *Post EQ* when `post`.
    Eq {
        path: usize,
        post: bool,
    },
    Gate {
        path: usize,
    },
    /// The effect at `index` of the path's chain.
    Effect {
        path: usize,
        index: usize,
    },
    Path {
        path: usize,
    },
    /// 0 is the clock output, 1.. the extra outputs in their order.
    Output {
        output: usize,
    },
    /// Input and output overlaid, the peaks and the sample count.
    Scope,
}

impl Sheet {
    /// The sheet that edits `stage` of path `path`.
    pub(crate) fn for_stage(path: usize, stage: ChainStage) -> Self {
        match stage {
            ChainStage::PreEq => Self::Eq { path, post: false },
            ChainStage::Gate => Self::Gate { path },
            ChainStage::Effect(index) => Self::Effect { path, index },
            ChainStage::PostEq => Self::Eq { path, post: true },
        }
    }

    /// The path and the chain stage this sheet edits, for the stage sheets.
    pub(crate) fn stage(self) -> Option<(usize, ChainStage)> {
        match self {
            Self::Eq { path, post: false } => Some((path, ChainStage::PreEq)),
            Self::Eq { path, post: true } => Some((path, ChainStage::PostEq)),
            Self::Gate { path } => Some((path, ChainStage::Gate)),
            Self::Effect { path, index } => Some((path, ChainStage::Effect(index))),
            _ => None,
        }
    }

    /// The path slot the sheet belongs to: a stage sheet's or the path
    /// sheet's.
    pub(crate) fn path(self) -> Option<usize> {
        match self {
            Self::Path { path } => Some(path),
            other => other.stage().map(|(path, _)| path),
        }
    }

    /// The sheet a `RIGWERK_SHEET` key names (screenshots and checks), on
    /// path `path`.  `diagnostics` is read at launch, before the paths.
    pub(crate) fn from_key(key: &str, path: usize) -> Option<Self> {
        Some(match key.trim().to_lowercase().as_str() {
            "eq" => Self::Eq { path, post: false },
            "gate" => Self::Gate { path },
            "effect" => Self::Effect { path, index: 0 },
            "path" => Self::Path { path },
            "output" => Self::Output { output: 0 },
            "scope" => Self::Scope,
            "diagnostics" => Self::Diagnostics,
            _ => return None,
        })
    }
}

/// Right-hand drawer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Drawer {
    Issues,
    /// The effect browser (`docs/ui-spec.md` §6.3).
    Browser,
}

/// Which part of Setup a status-bar click asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SetupSection {
    Devices,
    Latency,
    Clock,
}

/// Diagnostics sheet tabs (`docs/ui-spec.md` §6.5).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum DiagnosticsTab {
    #[default]
    Log,
    Performance,
    Latency,
    Scan,
    Status,
}

/// An inline name in the session menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SessionDraft {
    SaveAs(String),
    Rename(String),
}

/// Chrome that is not a saved page.
#[derive(Clone, Debug)]
pub(crate) struct Chrome {
    pub(crate) sheet: Option<Sheet>,
    pub(crate) drawer: Option<Drawer>,
    /// 0 means "40 % of the window, at least 300".  A drag stores the height.
    pub(crate) sheet_height: f32,
    pub(crate) diagnostics_tab: DiagnosticsTab,
    pub(crate) session_draft: Option<SessionDraft>,
    /// Ctrl+Shift+S opens the session menu on the next frame.
    pub(crate) open_session_menu: bool,
    /// The saved session a load must confirm, because the setup is edited.
    pub(crate) load_confirm: Option<usize>,
    /// When the newest log line arrived, so the status bar can drop it.
    pub(crate) log_at: Option<Instant>,
    /// The error title on the banner, until it clears or is dismissed.
    pub(crate) error_title: Option<String>,
    pub(crate) error_dismissed: bool,
    /// How many issues the status bar last drew, for the one pulse.
    pub(crate) issue_seen: usize,
    pub(crate) issue_pulse: Option<Instant>,
    pub(crate) setup_section: Option<SetupSection>,
    /// Filter text for Setup › Input.  Not persisted.
    pub(crate) input_device_filter: String,
    /// Filter text for Setup › Clock output.  Not persisted.
    pub(crate) output_device_filter: String,
    /// The path whose *Remove* was clicked once and waits for the second.
    pub(crate) remove_confirm: Option<usize>,
    /// A path the console should scroll into view (step 5).
    pub(crate) reveal_path: Option<usize>,
    /// The open stage should be scrolled into view in its chain.
    pub(crate) follow_stage: bool,
}

impl Default for Chrome {
    fn default() -> Self {
        Self {
            sheet: None,
            drawer: None,
            sheet_height: 0.0,
            diagnostics_tab: DiagnosticsTab::Log,
            session_draft: None,
            open_session_menu: false,
            load_confirm: None,
            log_at: None,
            error_title: None,
            error_dismissed: false,
            issue_seen: 0,
            issue_pulse: None,
            setup_section: None,
            input_device_filter: String::new(),
            output_device_filter: String::new(),
            remove_confirm: None,
            reveal_path: None,
            follow_stage: false,
        }
    }
}

impl Chrome {
    /// Saved sheet height, then the screenshot hooks.
    pub(crate) fn at_launch(sheet_height: f32) -> Self {
        let mut chrome = Self {
            sheet_height,
            ..Self::default()
        };
        let page = std::env::var("RIGWERK_PAGE").unwrap_or_default();
        let page = page.trim().to_lowercase();
        let sheet = std::env::var("RIGWERK_SHEET").unwrap_or_default();
        if page == "diagnostics" || sheet.trim().eq_ignore_ascii_case("diagnostics") {
            chrome.sheet = Some(Sheet::Diagnostics);
        }
        let drawer = std::env::var("RIGWERK_DRAWER").unwrap_or_default();
        if drawer.trim().eq_ignore_ascii_case("issues") {
            chrome.drawer = Some(Drawer::Issues);
        }
        chrome
    }
}

/// One decimal below 10 ms, none from 10 (`docs/ui-spec.md` §2.2).
pub(crate) fn format_ms(ms: f32) -> String {
    if ms < 10.0 {
        format!("{ms:.1} ms")
    } else {
        format!("{ms:.0} ms")
    }
}

const LOG_LINE_FOR: Duration = Duration::from_secs(5);
const PULSE_FOR: Duration = Duration::from_millis(700);
/// The sheet is at least this tall, unless the console area is shorter.
pub(crate) const SHEET_MIN: f32 = 300.0;
/// Stored ceiling.  The drawn sheet stops at the console area, which is
/// shorter than this on any window we open.
pub(crate) const SHEET_MAX: f32 = 4096.0;

impl RigWerkApp {
    pub(crate) fn open_diagnostics(&mut self) {
        self.chrome.sheet = Some(Sheet::Diagnostics);
        self.chrome.drawer = None;
    }

    pub(crate) fn open_issues(&mut self) {
        self.close_drawer();
        self.chrome.drawer = Some(Drawer::Issues);
        self.chrome.sheet = None;
    }

    /// Setup, scrolled to `section`.  Done returns to the console.
    pub(crate) fn open_setup(&mut self, section: SetupSection) {
        self.chrome.setup_section = Some(section);
        self.page = crate::app::Page::Setup;
        self.chrome.drawer = None;
    }

    /// Leaves Setup for the home page.
    pub(crate) fn close_setup(&mut self) {
        if self.page == crate::app::Page::Setup {
            self.page = crate::app::Page::Console;
        }
        self.chrome.setup_section = None;
        self.chrome.input_device_filter.clear();
        self.chrome.output_device_filter.clear();
    }

    /// Closes the drawer.  The effect browser closes with it.
    pub(crate) fn close_drawer(&mut self) {
        self.chrome.drawer = None;
        self.plugin_browser.open = false;
    }

    pub(crate) fn close_sheet(&mut self) {
        self.chrome.sheet = None;
    }

    /// Opens `sheet` in place of the one that is open, and closes a drawer.
    /// A stage or path sheet selects its path and marks its stage, so the
    /// Mixer and the console show the same selection.  A path that is gone
    /// opens nothing.
    pub(crate) fn open_sheet(&mut self, sheet: Sheet) {
        if let Some(path) = sheet.path() {
            if !self.select_path(path) {
                return;
            }
        }
        if let Some((_, stage)) = sheet.stage() {
            self.select_chain(ChainSelection::from_stage(stage));
        }
        if let Some(path) = sheet.path() {
            self.chrome.reveal_path = Some(path);
        }
        self.chrome.follow_stage = true;
        self.chrome.sheet = Some(sheet);
        self.close_drawer();
    }

    /// The stage sheet ‹ (`forward` false) or › opens: the stage before or
    /// after this one in the path's chain, in signal order.  `None` at the
    /// ends, and for a sheet that is not a stage's.
    pub(crate) fn sheet_neighbour(&self, forward: bool) -> Option<Sheet> {
        let (path, stage) = self.chrome.sheet?.stage()?;
        let mix = &self.paths.get(path)?.as_ref()?.mix;
        let effects = self.engine.plugin_views(path).len();
        neighbour_stage(stage, effects, mix.gate.slot, forward)
            .map(|next| Sheet::for_stage(path, next))
    }

    /// Moves the open stage sheet one stage along its chain.  `false` when
    /// there is no stage that way.
    pub(crate) fn step_sheet(&mut self, forward: bool) -> bool {
        match self.sheet_neighbour(forward) {
            Some(next) => {
                self.open_sheet(next);
                true
            }
            None => false,
        }
    }

    /// Closes a sheet whose subject has gone: a removed path, an effect
    /// index past the chain, an extra output that was removed.  The host
    /// calls this before it draws.
    pub(crate) fn tidy_sheet(&mut self) {
        let Some(sheet) = self.chrome.sheet else {
            return;
        };
        let gone = match sheet {
            Sheet::Diagnostics | Sheet::Scope => false,
            Sheet::Output { output } => output > self.extra_outputs.len(),
            Sheet::Effect { path, index } => {
                self.paths.get(path).is_none_or(Option::is_none)
                    || index >= self.engine.plugin_views(path).len()
            }
            other => other
                .path()
                .is_none_or(|path| self.paths.get(path).is_none_or(Option::is_none)),
        };
        if gone {
            self.chrome.sheet = None;
        }
    }

    /// `RIGWERK_SHEET` for a screenshot, after the setup: the selected
    /// path's stage, its first effect (a stand-in when the chain is empty),
    /// the clock output or the scope.
    pub(crate) fn open_sheet_for_screenshot(&mut self) {
        let Ok(key) = std::env::var("RIGWERK_SHEET") else {
            return;
        };
        let Some(sheet) = Sheet::from_key(&key, self.selected_path) else {
            return;
        };
        if let Sheet::Effect { path, .. } = sheet {
            self.add_screenshot_effect(path);
        }
        self.open_sheet(sheet);
    }

    /// Esc closes the innermost overlay, then leaves the tuner.
    pub(crate) fn close_innermost(&mut self) -> bool {
        if self.calibration.open {
            self.calibration.open = false;
            return true;
        }
        if self.chrome.drawer.is_some() {
            self.close_drawer();
            return true;
        }
        if self.chrome.sheet.is_some() {
            self.chrome.sheet = None;
            return true;
        }
        if self.page == crate::app::Page::Tuner {
            self.page = crate::app::Page::Console;
            return true;
        }
        false
    }

    pub(crate) fn note_log_arrived(&mut self) {
        self.chrome.log_at = Some(Instant::now());
    }

    /// The newest log line, for five seconds after it arrives.
    pub(crate) fn recent_log_line(&self, now: Instant) -> Option<&str> {
        let at = self.chrome.log_at?;
        if now.saturating_duration_since(at) > LOG_LINE_FOR {
            return None;
        }
        self.log.last().map(|entry| entry.text.as_str())
    }

    pub(crate) fn note_issue_count(&mut self, count: usize, now: Instant) {
        if count > self.chrome.issue_seen {
            self.chrome.issue_pulse = Some(now);
        }
        self.chrome.issue_seen = count;
    }

    pub(crate) fn issue_count_pulsing(&self, now: Instant) -> bool {
        self.chrome
            .issue_pulse
            .is_some_and(|at| now.saturating_duration_since(at) < PULSE_FOR)
    }

    pub(crate) fn dismiss_error_banner(&mut self) {
        self.chrome.error_dismissed = true;
    }

    /// The height to draw.  A saved height wins; otherwise 40 % of
    /// `available`, the console area between the title bar and the status
    /// bar.  Never below [`SHEET_MIN`] unless that area is shorter, and
    /// never taller than the area, so a drag can take the space above the
    /// sheet.
    pub(crate) fn sheet_draw_height(&self, available: f32) -> f32 {
        let height = if self.chrome.sheet_height > 0.0 {
            self.chrome.sheet_height
        } else {
            available * 0.4
        };
        let low = available.min(SHEET_MIN);
        let high = available.clamp(low, SHEET_MAX);
        height.clamp(low, high)
    }
}

#[cfg(test)]
mod tests;
