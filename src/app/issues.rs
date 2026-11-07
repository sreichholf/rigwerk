//! Live Issues: the judgments the Live page shows in one strip (see
//! `docs/mockups/live-issues-plan.md`).  Readings stay on the cards; a
//! problem becomes one row here, with a severity, where it is and one
//! action that only runs on a click.
//!
//! An error means the audio itself is about to break (a block nearly
//! late); anything that only colours the sound, such as a comb filter, is
//! a warning.
//!
//! Producers so far are the signal paths':
//! - unaligned paths that add up on one device channel (the Signal Routing
//!   `Sum` warning, `app::alignment`): a warning,
//! - a device playing a path whose chain latency is past what the
//!   alignment covers ([`MAX_ALIGN`]),
//! - an unmuted path that no unmuted device plays, also when it is the only
//!   path (a muted one is parked on purpose, e.g. right after Add path, and
//!   does not warn),
//! - the paths' DSP time not fitting the block (the graph quantum on the
//!   PipeWire duplex path, the output block otherwise), with hysteresis so
//!   the row does not flicker: the smoothed share raises the warning, the
//!   decaying peak share the error.  Both shares come from the mixer's
//!   total per block (`AudioEngine::total_dsp_time`), never from adding up
//!   the paths' peaks, which may be seconds apart; the per-path times only
//!   name the heaviest path.
//!
//! The output's own:
//! - a PipeWire quantum change that failed (`AudioEngine::quantum_note`),
//!   an output period that does not line up with the graph block, or a
//!   graph block still too large while Low latency runs (`QuantumInfo`),
//! - an output driver with no reading while a low-latency mode is chosen,
//! - underruns in this bridge session ([`UnderrunWatch`]): a warning that
//!   clears after [`UNDERRUN_QUIET`] without a new one, and an error once
//!   [`UNDERRUN_BURST`] of them fall within [`UNDERRUN_BURST_WINDOW`], held
//!   until the output goes quiet.
//!
//! The bridge's own:
//! - a bridge stopped because a device was disconnected: an error row once
//!   the `device_alert` window is dismissed, so the strip says why it
//!   emptied instead of vanishing.  The window stays the hard stop; the row
//!   points at the Devices page and clears with the next bridge start.
//!
//! The state behind the rows ([`IssueState`]) moves once per update tick in
//! [`RigWerkApp::tick_issues`], whether or not Live is drawn, and
//! [`RigWerkApp::live_issues`] only reads it.  The tick keeps the issues it
//! computed, and the strip draws those ([`RigWerkApp::issue_view`]), so
//! they are judged once per tick, not again for each frame drawn.  The DSP
//! latches and the underrun watch start over with each bridge start and
//! stop.  A new error
//! (a title not among the last tick's errors) expands a collapsed strip;
//! warnings do not.
//!
//! What the person does with the strip is app state too, never persisted:
//! - the filter (All, Warnings, Errors).  Choosing one expands the strip.
//!   It falls back to All when nothing is left to show, and when a new
//!   error opens the strip while it shows only warnings.
//! - dismissed warnings, by their stable [`IssueKey`] (the kind and the
//!   device or path slot, never the title, which carries numbers).  One
//!   stays hidden until a tick no longer has it as a warning (it cleared,
//!   or it became an error), or until the bridge session changes.  Errors
//!   cannot be dismissed: audio is breaking, so they stay until fixed.
//!   Showing the dismissed ones again is one click.
//! - the card a row relates to ([`IssueCard`]) rings for
//!   [`CARD_HIGHLIGHT_FOR`] after "Show on <card>"; nothing scrolls or
//!   moves, and the time is passed in so the pulse can be tested.
//!
//! The cards keep the readings (DRIVER, BUFFER, MODE); none of these
//! judgments is drawn there.  The fixes the output rows offer are the
//! controls the cards already have: Check driver, the output buffer, Low
//! latency, the WASAPI mode and the cushion.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::app::alignment::RoutingWarningKind;
use crate::app::paths::PathInfo;
use crate::app::settings::BUFFERS;
use crate::app::RigWerkApp;
use crate::audio::{CushionMode, EngineState, MAX_ALIGN};
#[cfg(target_os = "linux")]
use crate::audio::{QuantumInfo, PIPEWIRE_LOW_QUANTUM};
use crate::ui::pages::mixer::output_devices::OutputDeviceRow;

/// The smoothed DSP share of the block that raises the warning, and the
/// share it has to drop under to clear it.
pub(crate) const DSP_WARN_SHARE: f32 = 0.70;
pub(crate) const DSP_WARN_CLEAR: f32 = 0.60;
/// The peak DSP share that raises the error (the block is nearly late),
/// and the share it has to drop under to clear it.
pub(crate) const DSP_ERROR_SHARE: f32 = 0.95;
pub(crate) const DSP_ERROR_CLEAR: f32 = 0.85;

/// How long the output has to play without a new underrun before its
/// issue clears.
pub(crate) const UNDERRUN_QUIET: Duration = Duration::from_secs(10);
/// This many underruns within [`UNDERRUN_BURST_WINDOW`] turn the warning
/// into an error: the audio is breaking up, not clicking once.
pub(crate) const UNDERRUN_BURST: usize = 3;
pub(crate) const UNDERRUN_BURST_WINDOW: Duration = Duration::from_secs(5);

/// How long "Show on <card>" rings the related card.
pub(crate) const CARD_HIGHLIGHT_FOR: Duration = Duration::from_millis(1500);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum IssueSeverity {
    Error,
    Warning,
}

/// The DSP overload latches, kept between frames for the hysteresis.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct DspAlarm {
    pub(crate) warning: bool,
    pub(crate) error: bool,
}

impl DspAlarm {
    /// The latches after one reading of the smoothed (`avg`) and peak
    /// share of the block.
    pub(crate) fn next(self, avg: f32, peak: f32) -> Self {
        Self {
            warning: if self.warning {
                avg >= DSP_WARN_CLEAR
            } else {
                avg >= DSP_WARN_SHARE
            },
            error: if self.error {
                peak >= DSP_ERROR_CLEAR
            } else {
                peak >= DSP_ERROR_SHARE
            },
        }
    }

    pub(crate) fn severity(self) -> Option<IssueSeverity> {
        if self.error {
            Some(IssueSeverity::Error)
        } else if self.warning {
            Some(IssueSeverity::Warning)
        } else {
            None
        }
    }
}

/// A run of underruns that has not yet gone quiet for [`UNDERRUN_QUIET`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct UnderrunEpisode {
    /// Underruns since the run began.
    pub(crate) count: u64,
    /// Time since the latest one, as of the last update.
    pub(crate) since_last: Duration,
    /// Whether a burst was seen in this run.  Held until the run ends.
    pub(crate) burst: bool,
    last: Instant,
}

/// Follows the session's underrun count from frame to frame.  Time is
/// passed in, so the clearing and the burst rule can be tested.
#[derive(Clone, Debug, Default)]
pub(crate) struct UnderrunWatch {
    /// The session count at the last update.
    seen: u64,
    /// When the latest underruns arrived, oldest first, at most
    /// [`UNDERRUN_BURST`] of them.
    recent: VecDeque<Instant>,
    episode: Option<UnderrunEpisode>,
}

impl UnderrunWatch {
    /// Takes the session's underrun count at `now`.  A count lower than the
    /// last one is a new session and starts over.
    pub(crate) fn update(&mut self, count: u64, now: Instant) {
        if count < self.seen {
            *self = Self::default();
        }
        let new = count - self.seen;
        self.seen = count;
        if new > 0 {
            for _ in 0..new.min(UNDERRUN_BURST as u64) {
                if self.recent.len() == UNDERRUN_BURST {
                    self.recent.pop_front();
                }
                self.recent.push_back(now);
            }
            let burst = self.recent.len() == UNDERRUN_BURST
                && self.recent.front().is_some_and(|first| {
                    now.saturating_duration_since(*first) <= UNDERRUN_BURST_WINDOW
                });
            let episode = self.episode.get_or_insert(UnderrunEpisode {
                count: 0,
                since_last: Duration::ZERO,
                burst: false,
                last: now,
            });
            episode.count += new;
            episode.last = now;
            episode.burst |= burst;
        }
        if let Some(episode) = &mut self.episode {
            episode.since_last = now.saturating_duration_since(episode.last);
            if episode.since_last >= UNDERRUN_QUIET {
                self.episode = None;
                self.recent.clear();
            }
        }
    }

    /// The current run of underruns, if the output has not gone quiet.
    pub(crate) fn episode(&self) -> Option<&UnderrunEpisode> {
        self.episode.as_ref()
    }

    /// Underruns in this bridge session.
    pub(crate) fn session_count(&self) -> u64 {
        self.seen
    }
}

/// Which rows the strip lists.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum IssueFilter {
    #[default]
    All,
    Warnings,
    Errors,
}

impl IssueFilter {
    pub(crate) const ALL: [IssueFilter; 3] =
        [IssueFilter::All, IssueFilter::Warnings, IssueFilter::Errors];

    pub(crate) fn label(self) -> &'static str {
        match self {
            IssueFilter::All => "All",
            IssueFilter::Warnings => "Warnings",
            IssueFilter::Errors => "Errors",
        }
    }

    pub(crate) fn admits(self, severity: IssueSeverity) -> bool {
        match self {
            IssueFilter::All => true,
            IssueFilter::Warnings => severity == IssueSeverity::Warning,
            IssueFilter::Errors => severity == IssueSeverity::Error,
        }
    }

    /// The muted line for a filter with no rows.
    pub(crate) fn empty_note(self) -> &'static str {
        match self {
            IssueFilter::All => "No issues",
            IssueFilter::Warnings => "No warnings",
            IssueFilter::Errors => "No errors",
        }
    }
}

/// What an issue is, apart from its wording: the kind and the device or
/// path slot it is about.  Dismissing goes by this, since titles carry
/// counts and names that change while the problem stays the same.  One
/// row that can say different things gets a key for each, so dismissing
/// one does not hide another.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IssueKey {
    DeviceLost,
    /// Output device row.
    UnalignedSum(usize),
    /// Output device row.
    PastAlignment(usize),
    /// Path slot.
    RoutedNowhere(usize),
    Dsp,
    /// PipeWire did not take the quantum change.
    QuantumNotTaken,
    /// The graph block cannot be read (PipeWire, so Linux only).
    #[cfg(target_os = "linux")]
    QuantumUnknown,
    /// The output period does not line up with the graph block.
    #[cfg(target_os = "linux")]
    QuantumMisfit,
    /// Low latency runs on a graph block that is not low.
    #[cfg(target_os = "linux")]
    QuantumNotLow,
    Driver,
    Underruns,
}

/// The Live card an issue is about, for "Show on <card>".  Routing and
/// DSP rows have none: their button opens another page.  An Input variant
/// comes with the first producer that is about the input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IssueCard {
    Bridge,
    Output,
}

impl IssueCard {
    pub(crate) fn label(self) -> &'static str {
        match self {
            IssueCard::Bridge => "Bridge",
            IssueCard::Output => "Output",
        }
    }
}

/// A card ringing after "Show on <card>", in its issue's severity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CardHighlight {
    pub(crate) card: IssueCard,
    pub(crate) severity: IssueSeverity,
    pub(crate) since: Instant,
}

/// How strongly a highlight `elapsed` old rings, 1 at the start: two
/// pulses that fade out, 0 from [`CARD_HIGHLIGHT_FOR`] on.
pub(crate) fn highlight_strength(elapsed: Duration) -> f32 {
    if elapsed >= CARD_HIGHLIGHT_FOR {
        return 0.0;
    }
    let t = elapsed.as_secs_f32() / CARD_HIGHLIGHT_FOR.as_secs_f32();
    (1.0 - t) * (0.6 + 0.4 * (t * std::f32::consts::TAU * 2.0).cos())
}

/// The rows the strip can show: every issue except dismissed warnings.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct IssueView {
    /// Errors first, as [`RigWerkApp::live_issues`] orders them.
    pub(crate) visible: Vec<LiveIssue>,
    /// Current issues that are dismissed.
    pub(crate) hidden: usize,
}

impl IssueView {
    /// Nothing to report and nothing hidden: the strip is not drawn.
    pub(crate) fn is_empty(&self) -> bool {
        self.visible.is_empty() && self.hidden == 0
    }

    /// The visible rows `filter` lets through.
    pub(crate) fn filtered(&self, filter: IssueFilter) -> impl Iterator<Item = &LiveIssue> {
        self.visible
            .iter()
            .filter(move |issue| filter.admits(issue.severity))
    }

    pub(crate) fn count(&self, filter: IssueFilter) -> usize {
        self.filtered(filter).count()
    }
}

/// The issue state the strip keeps between frames (not persisted).
#[derive(Debug, Default)]
pub(crate) struct IssueState {
    pub(crate) dsp: DspAlarm,
    pub(crate) underruns: UnderrunWatch,
    /// The alert of a bridge stopped on a disconnected device, until the
    /// next bridge start.
    pub(crate) device_lost: Option<String>,
    /// The error titles of the last tick, to tell a new error from one the
    /// person already collapsed away.
    seen_errors: Vec<String>,
    /// Which rows the strip lists.
    pub(crate) filter: IssueFilter,
    /// Warnings the person dismissed, until they clear or the session ends.
    dismissed: Vec<IssueKey>,
    /// The card "Show on <card>" rings, until [`CARD_HIGHLIGHT_FOR`] passed.
    highlight: Option<CardHighlight>,
    /// The issues of the last tick, errors first: what the strip shows.
    current: Vec<LiveIssue>,
}

impl IssueState {
    /// A bridge started or stopped: the DSP latches, the underrun count and
    /// the dismissed warnings belong to the session that ended.
    pub(crate) fn new_session(&mut self) {
        self.dsp = DspAlarm::default();
        self.underruns = UnderrunWatch::default();
        self.dismissed.clear();
    }

    /// Whether `issue` is a dismissed warning.  Errors always show.
    pub(crate) fn hides(&self, issue: &LiveIssue) -> bool {
        issue.severity == IssueSeverity::Warning && self.dismissed.contains(&issue.key)
    }

    /// Once per tick: forgets dismissals whose warning is gone (cleared, or
    /// now an error), so it shows when it comes back, and returns the
    /// filter to All when the strip has nothing left to show.
    fn settle(&mut self, issues: &[LiveIssue], now: Instant) {
        self.dismissed.retain(|key| {
            issues
                .iter()
                .any(|issue| issue.key == *key && issue.severity == IssueSeverity::Warning)
        });
        if issues.iter().all(|issue| self.hides(issue)) {
            self.filter = IssueFilter::All;
        }
        if self
            .highlight
            .is_some_and(|lit| now.saturating_duration_since(lit.since) >= CARD_HIGHLIGHT_FOR)
        {
            self.highlight = None;
        }
    }
}

/// What an issue's button does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IssueAction {
    /// Turn the alignment on for output device `device`.
    AlignDevice(usize),
    /// Open Signal Routing with output device `device` selected.
    ShowRouting(usize),
    /// Open Signal Routing (no device in particular).
    OpenRouting,
    OpenDiagnostics,
    /// Open the Devices page (reconnect or pick another device).
    OpenDevices,
    /// Run the stand-alone output driver check.
    CheckDriver,
    /// Set the clock output's buffer to this many samples.
    SetOutputBuffer(usize),
    /// Turn the Low latency output off (PipeWire / ALSA).
    LowLatencyOff,
    /// Put the WASAPI clock output back to Shared.
    SharedOutput,
    /// Set the input-to-output cushion to Safe.
    SafeCushion,
}

impl IssueAction {
    pub(crate) fn label(self) -> String {
        match self {
            IssueAction::AlignDevice(_) => "Align paths".to_owned(),
            IssueAction::ShowRouting(_) | IssueAction::OpenRouting => "Show in Routing".to_owned(),
            IssueAction::OpenDiagnostics => "Open diagnostics".to_owned(),
            IssueAction::OpenDevices => "Open Devices".to_owned(),
            IssueAction::CheckDriver => "Check driver".to_owned(),
            IssueAction::SetOutputBuffer(frames) => format!("Set buffer to {frames}"),
            IssueAction::LowLatencyOff => "Turn Low latency off".to_owned(),
            IssueAction::SharedOutput => "Use Shared output".to_owned(),
            IssueAction::SafeCushion => "Use Safe cushion".to_owned(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LiveIssue {
    /// What it is, apart from the wording (dismissing goes by this).
    pub(crate) key: IssueKey,
    pub(crate) severity: IssueSeverity,
    pub(crate) title: String,
    pub(crate) detail: String,
    /// Where it is, e.g. `ROUTING · Scarlett 2i2`.
    pub(crate) place: String,
    /// The Live card it is about, if any.
    pub(crate) card: Option<IssueCard>,
    pub(crate) action: IssueAction,
}

impl LiveIssue {
    /// The button's accessible name: its label and the issue it acts on.
    pub(crate) fn action_name(&self) -> String {
        format!("{}: {}", self.action.label(), self.title)
    }

    /// Only warnings can be dismissed.
    pub(crate) fn dismissable(&self) -> bool {
        self.severity == IssueSeverity::Warning
    }

    /// The Dismiss button's accessible name.
    pub(crate) fn dismiss_name(&self) -> String {
        format!("Dismiss: {}", self.title)
    }

    /// "Show on Output", for an issue about a card.
    pub(crate) fn show_label(&self) -> Option<String> {
        self.card.map(|card| format!("Show on {}", card.label()))
    }

    /// The "Show on <card>" button's accessible name.
    pub(crate) fn show_name(&self) -> Option<String> {
        self.show_label()
            .map(|label| format!("{label}: {}", self.title))
    }
}

impl RigWerkApp {
    /// Moves the issue state once per update tick: the underrun watch and
    /// the DSP latches from the engine, then the dismissals, the filter and
    /// the card highlight settle, and a collapsed strip opens for a new
    /// error.
    pub(crate) fn tick_issues(&mut self, now: Instant) {
        if self.engine.state == EngineState::Running {
            self.issue_state
                .underruns
                .update(self.snapshot.underruns, now);
        }
        let total = self.engine.total_dsp_time().unwrap_or_default();
        self.issue_state.dsp = self
            .issue_state
            .dsp
            .next(total.avg_share(), total.peak_share());
        let issues = self.live_issues();
        self.issue_state.settle(&issues, now);
        self.open_on_new_error(&issues);
        self.issue_state.current = issues;
    }

    /// Opens a collapsed strip when an error appears that the last tick did
    /// not have.  A warning, or an error the person collapsed while it was
    /// showing, leaves the strip as it is.  The tick does the same over the
    /// issues it already has.
    #[cfg(test)]
    pub(crate) fn expand_on_new_error(&mut self) {
        let issues = self.live_issues();
        self.open_on_new_error(&issues);
    }

    /// Takes the issues the strip shows from the state as it is now, as a
    /// tick would, without moving the watches or latches.  For tests that
    /// set the state by hand.
    #[cfg(test)]
    pub(crate) fn refresh_issues(&mut self) {
        self.issue_state.current = self.live_issues();
    }

    /// [`Self::expand_on_new_error`] over this tick's issues.  Errors are
    /// never dismissed, so every one counts; a strip filtered to warnings
    /// goes back to All so the new error is in view.
    fn open_on_new_error(&mut self, issues: &[LiveIssue]) {
        let errors: Vec<String> = issues
            .iter()
            .filter(|issue| issue.severity == IssueSeverity::Error)
            .map(|issue| issue.title.clone())
            .collect();
        if errors
            .iter()
            .any(|title| !self.issue_state.seen_errors.contains(title))
        {
            self.live_issues_collapsed = false;
            if !self.issue_state.filter.admits(IssueSeverity::Error) {
                self.issue_state.filter = IssueFilter::All;
            }
        }
        self.issue_state.seen_errors = errors;
    }

    /// The rows the strip can show and how many are dismissed, from the
    /// last tick's issues.
    pub(crate) fn issue_view(&self) -> IssueView {
        let issues = &self.issue_state.current;
        let total = issues.len();
        let visible: Vec<LiveIssue> = issues
            .iter()
            .filter(|issue| !self.issue_state.hides(issue))
            .cloned()
            .collect();
        IssueView {
            hidden: total - visible.len(),
            visible,
        }
    }

    /// Lists only the rows `filter` lets through, and opens the strip.
    pub(crate) fn set_issue_filter(&mut self, filter: IssueFilter) {
        self.issue_state.filter = filter;
        self.live_issues_collapsed = false;
    }

    /// Hides the warning with `key` until it clears and comes back, or the
    /// bridge session changes.  An error is not hidden.  Goes by the last
    /// tick's issues, the ones the strip shows.
    pub(crate) fn dismiss_issue(&mut self, key: IssueKey) {
        let warning = self
            .issue_state
            .current
            .iter()
            .any(|issue| issue.key == key && issue.dismissable());
        if warning && !self.issue_state.dismissed.contains(&key) {
            self.issue_state.dismissed.push(key);
        }
    }

    /// Shows the dismissed warnings again, in an open strip.
    pub(crate) fn restore_dismissed_issues(&mut self) {
        self.issue_state.dismissed.clear();
        self.live_issues_collapsed = false;
    }

    /// Rings `card` in `severity`'s colour from `now` on.
    pub(crate) fn show_issue_card(
        &mut self,
        card: IssueCard,
        severity: IssueSeverity,
        now: Instant,
    ) {
        self.issue_state.highlight = Some(CardHighlight {
            card,
            severity,
            since: now,
        });
    }

    /// How strongly `card` rings at `now`, and in which severity, while a
    /// highlight on it lasts.
    pub(crate) fn card_highlight(
        &self,
        card: IssueCard,
        now: Instant,
    ) -> Option<(IssueSeverity, f32)> {
        let lit = self.issue_state.highlight.filter(|lit| lit.card == card)?;
        let strength = highlight_strength(now.saturating_duration_since(lit.since));
        (strength > 0.0).then_some((lit.severity, strength))
    }

    /// Every current issue, errors first.  Only reads: the state behind
    /// the rows moves in [`Self::tick_issues`].
    pub(crate) fn live_issues(&self) -> Vec<LiveIssue> {
        let infos = self.path_infos();
        let rows = self.output_device_rows();
        let mut issues = Vec::new();
        self.device_lost_issue(&mut issues);
        self.unaligned_sum_issues(&rows, &mut issues);
        self.clamped_issues(&infos, &rows, &mut issues);
        self.unheard_path_issues(&infos, &mut issues);
        self.dsp_issue(&infos, &mut issues);
        self.quantum_issue(&mut issues);
        self.driver_issue(&mut issues);
        self.underrun_issue(&mut issues);
        issues.sort_by_key(|issue| issue.severity);
        issues
    }

    /// A bridge stopped on a disconnected device.  Not while the alert
    /// window is up: that is the hard stop, and the row is what stays once
    /// it is dismissed.
    fn device_lost_issue(&self, issues: &mut Vec<LiveIssue>) {
        let Some(alert) = &self.issue_state.device_lost else {
            return;
        };
        if self.device_alert.is_some() {
            return;
        }
        issues.push(LiveIssue {
            key: IssueKey::DeviceLost,
            severity: IssueSeverity::Error,
            title: "The bridge stopped: a device was disconnected".to_owned(),
            detail: alert.clone(),
            place: "BRIDGE · Stopped".to_owned(),
            card: Some(IssueCard::Bridge),
            action: IssueAction::OpenDevices,
        });
    }

    /// (a) Paths that add up unaligned on a device.
    fn unaligned_sum_issues(&self, rows: &[OutputDeviceRow], issues: &mut Vec<LiveIssue>) {
        if self.path_count() < 2 {
            return;
        }
        for (device, row) in rows.iter().enumerate() {
            let Some(warning) = self
                .routing_warnings(device)
                .into_iter()
                .find(|warning| warning.kind == RoutingWarningKind::Sum)
            else {
                continue;
            };
            let names: Vec<String> = warning
                .paths
                .iter()
                .filter_map(|&slot| self.paths[slot].as_ref().map(|path| path.name.clone()))
                .collect();
            // Only the paths in the sum: others on the device do not comb.
            let arrivals: Vec<f32> = self
                .device_alignment(device)
                .map(|alignment| {
                    warning
                        .paths
                        .iter()
                        .filter_map(|&slot| alignment.arrival_ms[slot])
                        .collect()
                })
                .unwrap_or_default();
            let spread = arrivals.iter().copied().fold(f32::MIN, f32::max)
                - arrivals.iter().copied().fold(f32::MAX, f32::min);
            let spread = if arrivals.is_empty() { 0.0 } else { spread };
            let align = self.device_align(device);
            issues.push(LiveIssue {
                key: IssueKey::UnalignedSum(device),
                severity: IssueSeverity::Warning,
                title: format!("Paths add up unaligned on {}", row.name),
                detail: format!(
                    "{} meet on {} up to {spread:.1} ms apart: a comb filter (thin, hollow sound). {}",
                    names.join(" and "),
                    crate::ui::pages::routing::channel_list(&warning.channels),
                    if align {
                        "Their trims pull them apart."
                    } else {
                        "Alignment is off on this device."
                    }
                ),
                place: format!("ROUTING · {}", row.name),
                card: None,
                action: if align {
                    IssueAction::ShowRouting(device)
                } else {
                    IssueAction::AlignDevice(device)
                },
            });
        }
    }

    /// A device with alignment on that plays a path longer than the
    /// alignment covers: that path arrives late against the others.
    fn clamped_issues(
        &self,
        infos: &[PathInfo],
        rows: &[OutputDeviceRow],
        issues: &mut Vec<LiveIssue>,
    ) {
        if self.path_count() < 2 {
            return;
        }
        let rate = self.engine.plugin_rate().max(1);
        for (device, row) in rows.iter().enumerate() {
            let Some(alignment) = self.device_alignment(device) else {
                continue;
            };
            if !alignment.align || !alignment.clamped || alignment.path_count() < 2 {
                continue;
            }
            let names: Vec<&str> = infos
                .iter()
                .filter(|info| {
                    alignment.per_path[info.slot].is_some() && info.latency_samples > MAX_ALIGN
                })
                .map(|info| info.name.as_str())
                .collect();
            issues.push(LiveIssue {
                key: IssueKey::PastAlignment(device),
                severity: IssueSeverity::Warning,
                title: format!("{} is past the alignment limit", names.join(" and ")),
                detail: format!(
                    "Its chain is longer than the {:.0} ms ({MAX_ALIGN} frames) the alignment covers on {}, so it arrives late against the other paths.",
                    MAX_ALIGN as f32 * 1000.0 / rate as f32,
                    row.name
                ),
                place: format!("ROUTING · {}", row.name),
                card: None,
                action: IssueAction::ShowRouting(device),
            });
        }
    }

    /// (b) An unmuted path that no unmuted device plays: its chain runs
    /// for nothing.  A muted path is parked on purpose and does not warn.
    /// Also checked with one path: then nothing is heard at all.
    fn unheard_path_issues(&self, infos: &[PathInfo], issues: &mut Vec<LiveIssue>) {
        let devices = self.device_settings();
        for info in infos {
            if info.mute {
                continue;
            }
            let heard = devices.iter().enumerate().any(|(device, settings)| {
                !settings.mute && self.path_reaches_device(info.slot, device)
            });
            if heard {
                continue;
            }
            let (title, detail) = if info.routed {
                (
                    format!("{} is routed nowhere audible", info.name),
                    "Every device it plays on is muted.".to_owned(),
                )
            } else {
                (
                    format!("{} is routed nowhere", info.name),
                    "Its chain runs, but no device channel plays it. Route it, or mute it."
                        .to_owned(),
                )
            };
            issues.push(LiveIssue {
                key: IssueKey::RoutedNowhere(info.slot),
                severity: IssueSeverity::Warning,
                title,
                detail,
                place: format!("ROUTING · {}", info.name),
                card: None,
                action: IssueAction::OpenRouting,
            });
        }
    }

    /// (c) The paths' DSP time against the block they have to fit in: the
    /// mixer's total per block, so peaks of different blocks never add up.
    fn dsp_issue(&self, infos: &[PathInfo], issues: &mut Vec<LiveIssue>) {
        let Some(severity) = self.issue_state.dsp.severity() else {
            return;
        };
        let total = self.engine.total_dsp_time().unwrap_or_default();
        let block = total.block_ns as f32;
        let (avg_share, peak_share) = (total.avg_share(), total.peak_share());
        let mut heaviest: Option<(String, f32)> = None;
        for info in infos {
            let Some(time) = self.engine.path_dsp_time(info.slot) else {
                continue;
            };
            let most = time.avg_ns as f32;
            if heaviest.as_ref().is_none_or(|(_, top)| most > *top) {
                heaviest = Some((info.name.clone(), most));
            }
        }
        let quantum = if self.snapshot.direct {
            "graph quantum"
        } else {
            "output block"
        };
        let (name, most) = heaviest.unwrap_or_default();
        let shares = format!(
            "{:.0}% of the {:.1} ms {quantum} on average, {:.0}% at peak",
            avg_share * 100.0,
            block / 1e6,
            peak_share * 100.0
        );
        let detail = if infos.len() > 1 {
            format!(
                "{shares}; {name} takes {:.2} ms. Remove an insert or raise the buffer.",
                most / 1e6
            )
        } else {
            format!("{shares}. Remove an insert or raise the buffer.")
        };
        issues.push(LiveIssue {
            key: IssueKey::Dsp,
            severity,
            title: if infos.len() > 1 {
                format!("Paths overload the {quantum}")
            } else {
                format!("The chain overloads the {quantum}")
            },
            detail,
            place: format!("DSP · {name}"),
            card: None,
            action: IssueAction::OpenDiagnostics,
        });
    }

    /// (d) The PipeWire graph block against the output: a quantum change
    /// that failed, then the period / block fit, then a block still too
    /// large while Low latency runs.  One row at most: it is one problem.
    fn quantum_issue(&self, issues: &mut Vec<LiveIssue>) {
        if let Some(note) = self.engine.quantum_note() {
            issues.push(LiveIssue {
                key: IssueKey::QuantumNotTaken,
                severity: IssueSeverity::Warning,
                title: "PipeWire did not take the quantum change".to_owned(),
                detail: note.to_owned(),
                place: "OUTPUT · Low latency".to_owned(),
                card: Some(IssueCard::Output),
                action: IssueAction::OpenDiagnostics,
            });
        } else {
            #[cfg(target_os = "linux")]
            if let Some(issue) = self
                .quantum
                .as_ref()
                .and_then(|info| self.quantum_info_issue(info))
            {
                issues.push(issue);
            }
        }
    }

    /// The row for one quantum comparison, if it is worth one.  A large
    /// graph block with Low latency off is what the person chose, and before
    /// the bridge runs the block is still the idle desktop one; neither is a
    /// problem.
    #[cfg(target_os = "linux")]
    pub(crate) fn quantum_info_issue(&self, info: &QuantumInfo) -> Option<LiveIssue> {
        let quantum = info.quantum;
        if !info.available || quantum == 0 {
            return self.low_latency_output.then(|| LiveIssue {
                key: IssueKey::QuantumUnknown,
                severity: IssueSeverity::Warning,
                title: "PipeWire's graph block is unknown".to_owned(),
                detail: info.note.clone(),
                place: "OUTPUT · Low latency".to_owned(),
                card: Some(IssueCard::Output),
                action: IssueAction::OpenDiagnostics,
            });
        }
        let period = info.alsa_period;
        if period > 0 && !info.fits {
            let fitting =
                BUFFERS.iter().rev().copied().find(|&frames| {
                    frames as u32 <= quantum && quantum.is_multiple_of(frames as u32)
                });
            let action = match fitting {
                // The buffer is what the output asked for: changing it helps.
                Some(frames) if period as usize == self.output_buffer => {
                    IssueAction::SetOutputBuffer(frames)
                }
                _ if self.low_latency_output => IssueAction::LowLatencyOff,
                _ => IssueAction::OpenDiagnostics,
            };
            let detail = if period > quantum {
                format!(
                    "The output period ({period} frames) is larger than the PipeWire graph block ({quantum} frames), so the coarser period sets the latency."
                )
            } else {
                format!(
                    "The output period ({period} frames) does not divide the PipeWire graph block ({quantum} frames), so the two clocks drift against each other and can xrun."
                )
            };
            return Some(LiveIssue {
                key: IssueKey::QuantumMisfit,
                severity: IssueSeverity::Warning,
                title: "Graph block and output period do not line up".to_owned(),
                detail,
                place: "OUTPUT · Out clock".to_owned(),
                card: Some(IssueCard::Output),
                action,
            });
        }
        let running = self.engine.state == EngineState::Running;
        (!info.low_latency && self.low_latency_output && running).then(|| LiveIssue {
            key: IssueKey::QuantumNotLow,
            severity: IssueSeverity::Warning,
            title: "Low latency is on, but the graph block is not".to_owned(),
            detail: format!(
                "The desktop PipeWire graph runs {quantum} frames ({:.1} ms), not the {PIPEWIRE_LOW_QUANTUM} RigWerk asked for. Another app may hold the quantum.",
                quantum as f32 / info.rate.max(1) as f32 * 1000.0
            ),
            place: "OUTPUT · Low latency".to_owned(),
            card: Some(IssueCard::Output),
            action: IssueAction::OpenDiagnostics,
        })
    }

    /// (e) A low-latency output whose driver has no reading yet: MODE
    /// shows what was asked for, not what the driver allows.  Not while a
    /// check is running.
    fn driver_issue(&self, issues: &mut Vec<LiveIssue>) {
        if !self.output_driver_checkable()
            || !self.wants_low_latency_output()
            || self.driver_probe_job.is_some()
            || self.output_actual().is_some()
        {
            return;
        }
        issues.push(LiveIssue {
            key: IssueKey::Driver,
            severity: IssueSeverity::Warning,
            title: "Output driver not checked yet".to_owned(),
            detail: "Ask the sound system which rate and step this output allows before trusting MODE. Opens no audio.".to_owned(),
            place: "OUTPUT · Driver".to_owned(),
            card: Some(IssueCard::Output),
            action: IssueAction::CheckDriver,
        });
    }

    /// (f) Underruns in this bridge session: the ring ran empty and the
    /// output played a gap.  An error while they come in a burst.
    fn underrun_issue(&self, issues: &mut Vec<LiveIssue>) {
        if self.engine.state != EngineState::Running {
            return;
        }
        let watch = &self.issue_state.underruns;
        let Some(episode) = watch.episode() else {
            return;
        };
        let count = |count: u64| {
            if count == 1 {
                "1 underrun".to_owned()
            } else {
                format!("{count} underruns")
            }
        };
        let ago = episode.since_last.as_secs();
        let when = if ago == 0 {
            "just now".to_owned()
        } else {
            format!("{ago} s ago")
        };
        let action = self.safer_latency_action();
        let remedy = match action {
            IssueAction::SafeCushion => "A Safe cushion grows after each dropout.".to_owned(),
            IssueAction::LowLatencyOff => {
                "Turning Low latency off gives the output a larger block.".to_owned()
            }
            IssueAction::SharedOutput => {
                "Shared output uses the chosen buffer instead of the driver's smallest step."
                    .to_owned()
            }
            IssueAction::SetOutputBuffer(frames) => {
                format!("A {frames}-sample output buffer gives it more room.")
            }
            _ => "Check the DSP load and the driver in Diagnostics.".to_owned(),
        };
        let (severity, title) = if episode.burst {
            (IssueSeverity::Error, "Audio is dropping out on the output")
        } else {
            (IssueSeverity::Warning, "The output ran dry")
        };
        issues.push(LiveIssue {
            key: IssueKey::Underruns,
            severity,
            title: title.to_owned(),
            detail: format!(
                "{}, the last {when} ({} this session): the output played gaps. {remedy}",
                count(episode.count),
                count(watch.session_count())
            ),
            place: format!("OUTPUT · {}", self.selected_output().name),
            card: Some(IssueCard::Output),
            action,
        });
    }

    /// The control that buys the output the most safety for the least
    /// latency: the Safe cushion, then Low latency (or REAL-style /
    /// Exclusive) off, then the next larger output buffer.
    pub(crate) fn safer_latency_action(&self) -> IssueAction {
        if !self.snapshot.direct && self.cushion_mode != CushionMode::Safe {
            return IssueAction::SafeCushion;
        }
        if self.selected_output().asio_clsid.is_some() {
            return IssueAction::OpenDiagnostics;
        }
        if self.wants_low_latency_output() {
            return if cfg!(target_os = "windows") {
                IssueAction::SharedOutput
            } else {
                IssueAction::LowLatencyOff
            };
        }
        BUFFERS
            .iter()
            .copied()
            .find(|&frames| frames > self.output_buffer)
            .map_or(IssueAction::OpenDiagnostics, IssueAction::SetOutputBuffer)
    }

    /// Runs an issue's action (a click on its button).
    pub(crate) fn apply_issue_action(&mut self, action: IssueAction) {
        use crate::app::Page;
        match action {
            IssueAction::AlignDevice(device) => {
                self.set_device_align(device, true);
                let name = self
                    .output_device_rows()
                    .get(device)
                    .map_or_else(|| "Output device".to_owned(), |row| row.name.clone());
                self.push_log(
                    format!("{name}: alignment on, the paths line up again."),
                    crate::ui::theme::TEAL,
                );
            }
            IssueAction::ShowRouting(device) => {
                self.routing_device = device;
                self.page = Page::Routing;
            }
            IssueAction::OpenRouting => self.page = Page::Routing,
            IssueAction::OpenDiagnostics => self.page = Page::Diagnostics,
            IssueAction::OpenDevices => self.page = Page::Devices,
            IssueAction::CheckDriver => self.start_output_driver_check(true),
            IssueAction::SetOutputBuffer(frames) => {
                self.output_buffer = frames;
                self.push_log(
                    format!("Output buffer set to {frames} samples."),
                    crate::ui::theme::BLUE,
                );
            }
            IssueAction::LowLatencyOff => self.set_low_latency_output(false),
            IssueAction::SharedOutput => {
                self.set_wasapi_output_mode(crate::audio::WasapiOutputMode::Shared);
                self.push_log("WASAPI clock: Shared output.", crate::ui::theme::MUTED);
            }
            IssueAction::SafeCushion => {
                self.cushion_mode = CushionMode::Safe;
                self.engine.set_cushion(self.cushion_mode, self.cushion_ms);
                self.push_log("Cushion: Safe.", crate::ui::theme::AMBER);
            }
        }
    }
}

#[cfg(test)]
mod tests;
