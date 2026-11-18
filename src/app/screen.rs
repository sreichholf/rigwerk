//! Which sheet and drawer are open, and the title-bar drafts.
//!
//! The page (`Page`) is a classic screen or Setup.  The sheet and the
//! drawer sit over it.  *Setup* opens the Setup screen at a section.

use std::time::{Duration, Instant};

use crate::app::RigWerkApp;

/// Bottom sheet.  Diagnostics is the one this step draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Sheet {
    Diagnostics,
}

/// Right-hand drawer.  The effect browser arrives with its own step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Drawer {
    Issues,
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

/// `48 kHz` or `44.1 kHz`.
pub(crate) fn format_khz(rate: u32) -> String {
    if rate.is_multiple_of(1000) {
        format!("{} kHz", rate / 1000)
    } else {
        format!("{:.1} kHz", rate as f32 / 1000.0)
    }
}

const LOG_LINE_FOR: Duration = Duration::from_secs(5);
const PULSE_FOR: Duration = Duration::from_millis(700);
/// The sheet is at least this tall, and never more than this.
pub(crate) const SHEET_MIN: f32 = 300.0;
pub(crate) const SHEET_MAX: f32 = 900.0;

impl RigWerkApp {
    pub(crate) fn open_diagnostics(&mut self) {
        self.chrome.sheet = Some(Sheet::Diagnostics);
        self.chrome.drawer = None;
    }

    pub(crate) fn open_issues(&mut self) {
        self.chrome.drawer = Some(Drawer::Issues);
        self.chrome.sheet = None;
    }

    /// Setup, scrolled to `section`.  Done returns to Live, which is home
    /// until the console exists.
    pub(crate) fn open_setup(&mut self, section: SetupSection) {
        self.chrome.setup_section = Some(section);
        self.page = crate::app::Page::Setup;
        self.chrome.drawer = None;
    }

    /// Leaves Setup for the home page.
    pub(crate) fn close_setup(&mut self) {
        if self.page == crate::app::Page::Setup {
            self.page = crate::app::Page::Live;
        }
        self.chrome.setup_section = None;
    }

    pub(crate) fn close_drawer(&mut self) {
        self.chrome.drawer = None;
    }

    pub(crate) fn close_sheet(&mut self) {
        self.chrome.sheet = None;
    }

    /// Esc closes the innermost overlay, then leaves the tuner.
    pub(crate) fn close_innermost(&mut self) -> bool {
        if self.calibration.open {
            self.calibration.open = false;
            return true;
        }
        if self.chrome.drawer.is_some() {
            self.chrome.drawer = None;
            return true;
        }
        if self.chrome.sheet.is_some() {
            self.chrome.sheet = None;
            return true;
        }
        if self.page == crate::app::Page::Tuner {
            self.page = crate::app::Page::Live;
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

    /// The height to draw, from the saved value or 40 % of `available`.
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
