//! An error means the audio is about to break.  A warning only colours
//! the sound.  Dismissals and latches reset when the bridge run or the
//! extra-output slot changes.

use crate::app::LogTone;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub(crate) use silence::{SilenceInput, SilenceWatch};

use crate::app::alignment::RoutingWarningKind;
use crate::app::output_devices::OutputDeviceRow;
use crate::app::paths::PathInfo;
use crate::app::settings::{bridge_buffer_from_pref, bridge_buffer_to_pref, BUFFERS};
use crate::app::RigWerkApp;
use crate::audio::{
    BufferMode, CallbackFailure, EngineState, EXTRA_OUTPUT_DRIFT_LIMIT_PPM, MAX_ALIGN,
    MAX_EXTRA_OUTPUTS, SAFETY_CEILING_DB, SAFETY_GR_DB,
};
#[cfg(target_os = "linux")]
use crate::audio::{QuantumInfo, PIPEWIRE_LOW_QUANTUM};

pub(crate) const DSP_WARN_SHARE: f32 = 0.70;
pub(crate) const DSP_WARN_CLEAR: f32 = 0.60;
pub(crate) const DSP_ERROR_SHARE: f32 = 0.95;
pub(crate) const DSP_ERROR_CLEAR: f32 = 0.85;

pub(crate) const DROPOUT_QUIET: Duration = Duration::from_secs(10);
/// This many within [`DROPOUT_BURST_WINDOW`] turn the warning into an error.
pub(crate) const DROPOUT_BURST: usize = 3;
pub(crate) const DROPOUT_BURST_WINDOW: Duration = Duration::from_secs(5);

pub(crate) const DRIFT_RAISE_AFTER: Duration = Duration::from_secs(1);
pub(crate) const DRIFT_CLEAR_AFTER: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum IssueSeverity {
    Error,
    Warning,
}

/// Latches between frames.  The share has to fall before one clears.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct DspAlarm {
    pub(crate) warning: bool,
    pub(crate) error: bool,
}

impl DspAlarm {
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

/// A run that has not yet gone quiet for [`DROPOUT_QUIET`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DropoutEpisode {
    pub(crate) count: u64,
    pub(crate) since_last: Duration,
    /// Held until the run ends.
    pub(crate) burst: bool,
    last: Instant,
}

/// Time is passed in, so a test can move the quiet period and the burst.
#[derive(Clone, Debug, Default)]
pub(crate) struct DropoutWatch {
    seen: u64,
    /// Oldest first, at most [`DROPOUT_BURST`].
    recent: VecDeque<Instant>,
    episode: Option<DropoutEpisode>,
}

impl DropoutWatch {
    /// A count lower than the last one is a new run and starts over.
    pub(crate) fn update(&mut self, count: u64, now: Instant) {
        if count < self.seen {
            *self = Self::default();
        }
        let new = count - self.seen;
        self.seen = count;
        if new > 0 {
            for _ in 0..new.min(DROPOUT_BURST as u64) {
                if self.recent.len() == DROPOUT_BURST {
                    self.recent.pop_front();
                }
                self.recent.push_back(now);
            }
            let burst = self.recent.len() == DROPOUT_BURST
                && self.recent.front().is_some_and(|first| {
                    now.saturating_duration_since(*first) <= DROPOUT_BURST_WINDOW
                });
            let episode = self.episode.get_or_insert(DropoutEpisode {
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
            if episode.since_last >= DROPOUT_QUIET {
                self.episode = None;
                self.recent.clear();
            }
        }
    }

    /// `count` is already seen, so it starts no episode.
    pub(crate) fn starting_at(count: u64) -> Self {
        Self {
            seen: count,
            ..Self::default()
        }
    }

    pub(crate) fn episode(&self) -> Option<&DropoutEpisode> {
        self.episode.as_ref()
    }

    /// Dropouts in this bridge run, or since an extra output's stream opened.
    pub(crate) fn run_count(&self) -> u64 {
        self.seen
    }
}

/// Raises after the input stays set for `raise_after` and clears after it
/// stays clear for `clear_after`.  A flip back restarts the wait.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct HeldFlag {
    raised: bool,
    /// Since the input last disagreed with `raised`.
    pending: Option<Instant>,
}

impl HeldFlag {
    pub(crate) fn update(
        &mut self,
        input: bool,
        now: Instant,
        raise_after: Duration,
        clear_after: Duration,
    ) -> bool {
        if input == self.raised {
            self.pending = None;
            return self.raised;
        }
        let since = *self.pending.get_or_insert(now);
        let wait = if input { raise_after } else { clear_after };
        if now.saturating_duration_since(since) >= wait {
            self.raised = input;
            self.pending = None;
        }
        self.raised
    }

    pub(crate) fn raised(self) -> bool {
        self.raised
    }
}

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

    pub(crate) fn empty_note(self) -> &'static str {
        match self {
            IssueFilter::All => "No issues",
            IssueFilter::Warnings => "No warnings",
            IssueFilter::Errors => "No errors",
        }
    }
}

/// Dismissing goes by this.  Titles carry counts and names that change
/// while the problem stays.  One row that can say different things gets
/// a key for each, so dismissing one does not hide another.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IssueKey {
    /// A lost device or a caught callback failure.
    BridgeStopped,
    UnalignedSum(usize),
    PastAlignment(usize),
    RoutedNowhere(usize),
    Dsp,
    QuantumNotTaken,
    #[cfg(target_os = "linux")]
    QuantumUnknown,
    #[cfg(target_os = "linux")]
    QuantumMisfit,
    #[cfg(target_os = "linux")]
    QuantumNotLow,
    Driver,
    Dropouts,
    ExtraOutputRetrying(usize),
    ExtraOutputNotListed(usize),
    ExtraOutputDropouts(usize),
    ExtraOutputDrift(usize),
    SinkSilent,
    /// One row: the device with the most reduction.
    SafetyCeiling,
}

impl IssueKey {
    pub(crate) fn extra_output_slot(self) -> Option<usize> {
        match self {
            IssueKey::ExtraOutputRetrying(slot)
            | IssueKey::ExtraOutputNotListed(slot)
            | IssueKey::ExtraOutputDropouts(slot)
            | IssueKey::ExtraOutputDrift(slot) => Some(slot),
            _ => None,
        }
    }
}

/// `Output(0)` is the clock output, then the extra outputs, the same
/// index as [`crate::app::screen::Sheet::Output`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Anchor {
    Input,
    Path(usize),
    Master,
    Output(usize),
    Setup(crate::app::screen::SetupSection),
}

/// Every issue except dismissed warnings.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct IssueView {
    /// Errors first.
    pub(crate) visible: Vec<LiveIssue>,
    pub(crate) hidden: usize,
}

impl IssueView {
    pub(crate) fn is_empty(&self) -> bool {
        self.visible.is_empty() && self.hidden == 0
    }

    pub(crate) fn filtered(&self, filter: IssueFilter) -> impl Iterator<Item = &LiveIssue> {
        self.visible
            .iter()
            .filter(move |issue| filter.admits(issue.severity))
    }

    pub(crate) fn count(&self, filter: IssueFilter) -> usize {
        self.filtered(filter).count()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StopReason {
    DeviceLost,
    Callback(CallbackFailure),
}

impl StopReason {
    fn title(self) -> &'static str {
        match self {
            Self::DeviceLost => "The bridge stopped: a device was disconnected",
            Self::Callback(_) => "The bridge stopped: a real-time audio callback failed",
        }
    }

    /// A lost device's alert already ends with its reconnect step.  A
    /// malformed block is not a plugin's fault, so it must not blame one.
    fn next_step(self) -> Option<&'static str> {
        match self {
            Self::DeviceLost => None,
            Self::Callback(CallbackFailure::Panic) => {
                Some("Disable the plugin that failed, then start the bridge again.")
            }
            Self::Callback(CallbackFailure::BadBlock) => Some(
                "Start the bridge again; if it comes back, the device is handing over blocks \
                 that do not line up.",
            ),
        }
    }
}

/// Not persisted.  The drawer shows the last tick instead of judging
/// again per frame.
#[derive(Debug, Default)]
pub(crate) struct IssueState {
    pub(crate) dsp: DspAlarm,
    pub(crate) dropouts: DropoutWatch,
    /// That extra output's own ring.
    pub(crate) extra_output_dropouts: [DropoutWatch; MAX_EXTRA_OUTPUTS],
    pub(crate) extra_output_drift: [HeldFlag; MAX_EXTRA_OUTPUTS],
    /// `None` until the first tick adopts the slots as they are.
    extra_output_ids: Option<[String; MAX_EXTRA_OUTPUTS]>,
    pub(crate) silence: SilenceWatch,
    /// Until the next bridge start.
    pub(crate) stopped: Option<(StopReason, String)>,
    /// Titles from the last tick, so a new error is distinct from one
    /// already announced.
    seen_errors: Vec<String>,
    pub(crate) filter: IssueFilter,
    /// Until they clear or the run ends.
    dismissed: Vec<IssueKey>,
    /// Errors first.
    current: Vec<LiveIssue>,
}

impl IssueState {
    /// The latches, dropout count and dismissals belong to the run that ended.
    pub(crate) fn new_run(&mut self) {
        self.dsp = DspAlarm::default();
        self.dropouts = DropoutWatch::default();
        self.extra_output_dropouts = Default::default();
        self.extra_output_drift = Default::default();
        self.silence = SilenceWatch::default();
        self.dismissed.clear();
    }

    /// Its watches and dismissals were about the old device.  The dropout
    /// watch takes `count` as already seen.
    fn forget_extra_output(&mut self, slot: usize, count: u64) {
        self.extra_output_dropouts[slot] = DropoutWatch::starting_at(count);
        self.extra_output_drift[slot] = HeldFlag::default();
        self.dismissed
            .retain(|key| key.extra_output_slot() != Some(slot));
    }

    /// Errors always show.
    pub(crate) fn hides(&self, issue: &LiveIssue) -> bool {
        issue.severity == IssueSeverity::Warning && self.dismissed.contains(&issue.key)
    }

    /// A dismissal whose warning cleared, or became an error, comes back.
    /// The filter returns to All when nothing is left to show.
    fn settle(&mut self, issues: &[LiveIssue]) {
        self.dismissed.retain(|key| {
            issues
                .iter()
                .any(|issue| issue.key == *key && issue.severity == IssueSeverity::Warning)
        });
        if issues.iter().all(|issue| self.hides(issue)) {
            self.filter = IssueFilter::All;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IssueAction {
    AlignDevice(usize),
    ShowRouting(usize),
    OpenRouting,
    OpenDiagnostics,
    OpenDevices,
    CheckDriver,
    SetOutputBuffer(usize),
    LowLatencyOff,
    SharedOutput,
    AutoBridgeBuffer,
    ShowExtraOutput(usize),
    ExtraOutputAutoBridgeBuffer(usize),
    /// The safety ceiling has no control of its own.  The index matches
    /// [`Anchor::Output`].
    ShowOutput(usize),
}

impl IssueAction {
    pub(crate) fn label(self) -> String {
        match self {
            IssueAction::AlignDevice(_) => "Align paths".to_owned(),
            IssueAction::ShowRouting(_) | IssueAction::OpenRouting => "Show output".to_owned(),
            IssueAction::OpenDiagnostics => "Open diagnostics".to_owned(),
            IssueAction::OpenDevices => "Open Setup".to_owned(),
            IssueAction::CheckDriver => "Check driver".to_owned(),
            IssueAction::SetOutputBuffer(frames) => format!("Set buffer to {frames}"),
            IssueAction::LowLatencyOff => "Turn Low latency off".to_owned(),
            IssueAction::SharedOutput => "Use Shared output".to_owned(),
            IssueAction::AutoBridgeBuffer | IssueAction::ExtraOutputAutoBridgeBuffer(_) => {
                "Use Auto bridge buffer".to_owned()
            }
            IssueAction::ShowExtraOutput(_) | IssueAction::ShowOutput(_) => {
                "Show output".to_owned()
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LiveIssue {
    pub(crate) key: IssueKey,
    pub(crate) severity: IssueSeverity,
    pub(crate) title: String,
    pub(crate) detail: String,
    /// Sentence case: a path name, "Clock output", an extra output,
    /// "Master" or "Setup".
    pub(crate) place: String,
    pub(crate) anchor: Anchor,
    pub(crate) action: IssueAction,
}

impl LiveIssue {
    pub(crate) fn action_name(&self) -> String {
        format!("{}: {}", self.action.label(), self.title)
    }

    /// The safety ceiling stays while a device is still against it.
    pub(crate) fn dismissable(&self) -> bool {
        self.severity == IssueSeverity::Warning && self.key != IssueKey::SafetyCeiling
    }

    pub(crate) fn dismiss_name(&self) -> String {
        format!("Dismiss: {}", self.title)
    }

    pub(crate) fn show_label(&self) -> &'static str {
        "Show"
    }

    pub(crate) fn show_name(&self) -> String {
        format!("Show: {}", self.title)
    }
}

impl RigWerkApp {
    pub(crate) fn tick_issues(&mut self, now: Instant) {
        self.follow_extra_output_slots();
        if self.engine.state == EngineState::Running {
            self.issue_state
                .dropouts
                .update(self.snapshot.dropouts, now);
            for (watch, &count) in self
                .issue_state
                .extra_output_dropouts
                .iter_mut()
                .zip(&self.snapshot.extra_output_dropouts)
            {
                watch.update(count, now);
            }
            for (held, &pinned) in self
                .issue_state
                .extra_output_drift
                .iter_mut()
                .zip(&self.snapshot.extra_output_drift_at_limit)
            {
                held.update(pinned, now, DRIFT_RAISE_AFTER, DRIFT_CLEAR_AFTER);
            }
        }
        let silence = self.silence_input(now);
        self.issue_state.silence.update(silence, now);
        let total = self.engine.total_dsp_time().unwrap_or_default();
        self.issue_state.dsp = self
            .issue_state
            .dsp
            .next(total.avg_share(), total.peak_share());
        let issues = self.live_issues();
        self.issue_state.settle(&issues);
        self.open_on_new_error(&issues);
        self.issue_state.current = issues;
    }

    /// Another device in the slot starts its watches over.
    fn follow_extra_output_slots(&mut self) {
        let id = |slot: usize| {
            self.extra_outputs
                .get(slot)
                .map_or("", |pref| pref.id.as_str())
        };
        let Some(ids) = &mut self.issue_state.extra_output_ids else {
            self.issue_state.extra_output_ids =
                Some(std::array::from_fn(|slot| id(slot).to_owned()));
            return;
        };
        let mut changed = [false; MAX_EXTRA_OUTPUTS];
        for (slot, seen) in ids.iter_mut().enumerate() {
            if seen != id(slot) {
                *seen = id(slot).to_owned();
                changed[slot] = true;
            }
        }
        for slot in (0..MAX_EXTRA_OUTPUTS).filter(|&slot| changed[slot]) {
            let count = self.snapshot.extra_output_dropouts[slot];
            self.issue_state.forget_extra_output(slot, count);
        }
    }

    /// Armed only for a PipeWire low-latency bridge: the sink monitor is
    /// not WASAPI's.  Parked when nothing should reach the clock output.
    fn silence_input(&mut self, now: Instant) -> SilenceInput {
        let monitor = self.engine.sink_monitor_peak(now);
        let channels =
            (self.snapshot.output_channels as usize).clamp(1, self.snapshot.out_channels.len());
        let sent = self.snapshot.out_channels[..channels]
            .iter()
            .copied()
            .fold(0.0, f32::max);
        let routed = self
            .path_infos()
            .iter()
            .any(|info| !info.mute && self.path_reaches_device(info.slot, 0));
        SilenceInput {
            armed: cfg!(target_os = "linux")
                && self.engine.state == EngineState::Running
                && self.low_latency_output,
            sent,
            monitor,
            parked: self.mute || self.clock_output.mute || !routed,
            dropouts: self.issue_state.dropouts.episode().is_some(),
        }
    }

    /// Checks the banner without moving the watches.
    #[cfg(test)]
    pub(crate) fn expand_on_new_error(&mut self) {
        let issues = self.live_issues();
        self.open_on_new_error(&issues);
    }

    /// Refreshes the drawer's rows without moving the watches or latches.
    #[cfg(test)]
    pub(crate) fn refresh_issues(&mut self) {
        self.issue_state.current = self.live_issues();
    }

    /// A new error sets the banner and returns a warnings-only filter to
    /// All.  A warning does not.
    fn open_on_new_error(&mut self, issues: &[LiveIssue]) {
        let errors: Vec<String> = issues
            .iter()
            .filter(|issue| issue.severity == IssueSeverity::Error)
            .map(|issue| issue.title.clone())
            .collect();
        let fresh = errors
            .iter()
            .any(|title| !self.issue_state.seen_errors.contains(title));
        if fresh {
            if !self.issue_state.filter.admits(IssueSeverity::Error) {
                self.issue_state.filter = IssueFilter::All;
            }
            self.chrome.error_title = errors.first().cloned();
            self.chrome.error_dismissed = false;
        } else if errors.is_empty() {
            self.chrome.error_title = None;
            self.chrome.error_dismissed = false;
        } else if self
            .chrome
            .error_title
            .as_ref()
            .is_some_and(|title| !errors.iter().any(|item| item == title))
        {
            self.chrome.error_title = None;
        }
        self.issue_state.seen_errors = errors;
    }

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

    pub(crate) fn set_issue_filter(&mut self, filter: IssueFilter) {
        self.issue_state.filter = filter;
    }

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

    pub(crate) fn restore_dismissed_issues(&mut self) {
        self.issue_state.dismissed.clear();
    }

    pub(crate) fn show_issue(&mut self, anchor: Anchor) {
        self.close_drawer();
        match anchor {
            Anchor::Setup(section) => {
                self.open_setup(section);
                return;
            }
            Anchor::Input | Anchor::Master => self.close_sheet(),
            Anchor::Path(slot) => {
                self.open_sheet(crate::app::screen::Sheet::Path { path: slot });
            }
            Anchor::Output(index) => {
                self.open_sheet(crate::app::screen::Sheet::Output { output: index });
            }
        }
        self.page = crate::app::Page::Console;
        self.chrome.setup_section = None;
    }

    /// Errors sort before warnings, so `min` is the worst.
    pub(crate) fn anchor_severity(&self, anchor: Anchor) -> Option<IssueSeverity> {
        self.issue_view()
            .visible
            .iter()
            .filter(|issue| same_strip(issue.anchor, anchor))
            .map(|issue| issue.severity)
            .min()
    }

    /// Only reads.  The state moves in [`Self::tick_issues`].
    pub(crate) fn live_issues(&self) -> Vec<LiveIssue> {
        let infos = self.path_infos();
        let rows = self.output_device_rows();
        let mut issues = Vec::new();
        self.stopped_bridge_issue(&mut issues);
        self.unaligned_sum_issues(&rows, &mut issues);
        self.clamped_issues(&infos, &rows, &mut issues);
        self.unheard_path_issues(&infos, &mut issues);
        self.dsp_issue(&infos, &mut issues);
        self.quantum_issue(&mut issues);
        self.driver_issue(&mut issues);
        self.dropout_issue(&mut issues);
        self.silence_issue(&rows, &mut issues);
        self.safety_ceiling_issue(&rows, &mut issues);
        self.extra_output_issues(&rows, &mut issues);
        issues.sort_by_key(|issue| issue.severity);
        issues
    }

    /// Not while the alert window is up.  The row is what stays once it
    /// is dismissed.
    fn stopped_bridge_issue(&self, issues: &mut Vec<LiveIssue>) {
        let Some((reason, detail)) = &self.issue_state.stopped else {
            return;
        };
        if self.device_alert.is_some() {
            return;
        }
        let detail = match reason.next_step() {
            Some(next) => format!("{detail} {next}"),
            None => detail.clone(),
        };
        issues.push(LiveIssue {
            key: IssueKey::BridgeStopped,
            severity: IssueSeverity::Error,
            title: reason.title().to_owned(),
            detail,
            place: "Setup".to_owned(),
            // No diagnostics anchor yet: the row keeps Setup until there is
            // one, even when its action opens Diagnostics.
            anchor: Anchor::Setup(crate::app::screen::SetupSection::Devices),
            action: match reason {
                StopReason::DeviceLost => IssueAction::OpenDevices,
                StopReason::Callback(_) => IssueAction::OpenDiagnostics,
            },
        });
    }

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
                    crate::app::alignment::channel_list(&warning.channels),
                    if align {
                        "Their trims pull them apart."
                    } else {
                        "Alignment is off on this device."
                    }
                ),
                place: output_place(rows, device),
                anchor: Anchor::Output(device),
                action: if align {
                    IssueAction::ShowRouting(device)
                } else {
                    IssueAction::AlignDevice(device)
                },
            });
        }
    }

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
            // A return counts its wait for its senders too.
            let latencies = self.engine.path_latencies();
            let names: Vec<&str> = infos
                .iter()
                .filter(|info| {
                    alignment.per_path[info.slot].is_some() && latencies[info.slot] > MAX_ALIGN
                })
                .map(|info| info.name.as_str())
                .collect();
            issues.push(LiveIssue {
                key: IssueKey::PastAlignment(device),
                severity: IssueSeverity::Warning,
                title: format!("{} is past the alignment limit", names.join(" and ")),
                detail: format!(
                    "Its chain is longer than the {:.0} ms ({MAX_ALIGN} samples) the alignment covers on {}, so it arrives late against the other paths.",
                    MAX_ALIGN as f32 * 1000.0 / rate as f32,
                    row.name
                ),
                place: output_place(rows, device),
                anchor: Anchor::Output(device),
                action: IssueAction::ShowRouting(device),
            });
        }
    }

    /// A muted path is parked on purpose and does not warn.  A path whose
    /// send reaches an unmuted return that is heard is heard too.
    fn unheard_path_issues(&self, infos: &[PathInfo], issues: &mut Vec<LiveIssue>) {
        let devices = self.device_settings();
        let played = |slot: usize| {
            devices
                .iter()
                .enumerate()
                .any(|(device, settings)| !settings.mute && self.path_reaches_device(slot, device))
        };
        for info in infos {
            if info.mute {
                continue;
            }
            let through_return = self.return_slots().into_iter().any(|ret| {
                self.paths[ret].as_ref().is_some_and(|path| !path.mix.mute)
                    && crate::audio::path_level_gain(self.path_send(info.slot, ret)) > 0.0
                    && played(ret)
            });
            if played(info.slot) || through_return {
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
                    "Its chain runs, but no output plays it. Map it, or mute it.".to_owned(),
                )
            };
            issues.push(LiveIssue {
                key: IssueKey::RoutedNowhere(info.slot),
                severity: IssueSeverity::Warning,
                title,
                detail,
                place: info.name.clone(),
                anchor: Anchor::Path(info.slot),
                action: IssueAction::OpenRouting,
            });
        }
    }

    /// The mixer's total per buffer, so peaks from different buffers never
    /// add.  The anchor is the heaviest path, or Master when there is none.
    fn dsp_issue(&self, infos: &[PathInfo], issues: &mut Vec<LiveIssue>) {
        // The screenshot renderer's startup peak is not the rig's: a shot
        // waits on Xvfb's software GL, not on audio.
        if self.screenshot.is_some() {
            return;
        }
        let Some(severity) = self.issue_state.dsp.severity() else {
            return;
        };
        let total = self.engine.total_dsp_time().unwrap_or_default();
        let buffer_ns = total.block_ns as f32;
        let (avg_share, peak_share) = (total.avg_share(), total.peak_share());
        let mut heaviest: Option<(usize, String, f32)> = None;
        for info in infos {
            let most = self
                .engine
                .path_dsp_time(info.slot)
                .map(|time| time.avg_ns as f32)
                .unwrap_or(0.0);
            if heaviest.as_ref().is_none_or(|(_, _, top)| most > *top) {
                heaviest = Some((info.slot, info.name.clone(), most));
            }
        }
        let buffer_name = if self.snapshot.direct {
            "graph buffer"
        } else {
            "output buffer"
        };
        let (anchor, place, most) = match heaviest {
            Some((slot, name, most)) => (Anchor::Path(slot), name, most),
            None => (Anchor::Master, "Master".to_owned(), 0.0),
        };
        let shares = format!(
            "{:.0}% of the {:.1} ms {buffer_name} on average, {:.0}% at peak",
            avg_share * 100.0,
            buffer_ns / 1e6,
            peak_share * 100.0
        );
        let detail = if infos.len() > 1 {
            format!(
                "{shares}; {place} takes {:.2} ms. Remove an effect or raise the buffer.",
                most / 1e6
            )
        } else {
            format!("{shares}. Remove an effect or raise the buffer.")
        };
        issues.push(LiveIssue {
            key: IssueKey::Dsp,
            severity,
            title: if infos.len() > 1 {
                format!("Paths overload the {buffer_name}")
            } else {
                format!("The chain overloads the {buffer_name}")
            },
            detail,
            place,
            anchor,
            action: IssueAction::OpenDiagnostics,
        });
    }

    /// One row: a failed change, a misfit and a buffer that is not low are
    /// one problem.
    fn quantum_issue(&self, issues: &mut Vec<LiveIssue>) {
        if let Some(note) = self.engine.quantum_note() {
            issues.push(LiveIssue {
                key: IssueKey::QuantumNotTaken,
                severity: IssueSeverity::Warning,
                title: "PipeWire did not take the buffer change".to_owned(),
                detail: note.to_owned(),
                place: "Setup".to_owned(),
                anchor: Anchor::Setup(crate::app::screen::SetupSection::Clock),
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

    /// A large graph buffer with Low latency off is the chosen one.  Before
    /// the bridge runs it is still the idle desktop buffer.  Neither warns.
    #[cfg(target_os = "linux")]
    pub(crate) fn quantum_info_issue(&self, info: &QuantumInfo) -> Option<LiveIssue> {
        let graph_samples = info.quantum;
        let anchor = Anchor::Setup(crate::app::screen::SetupSection::Clock);
        if !info.available || graph_samples == 0 {
            return self.low_latency_output.then(|| LiveIssue {
                key: IssueKey::QuantumUnknown,
                severity: IssueSeverity::Warning,
                title: "PipeWire's graph buffer size is unknown".to_owned(),
                detail: info.note.clone(),
                place: "Setup".to_owned(),
                anchor,
                action: IssueAction::OpenDiagnostics,
            });
        }
        let output_samples = info.alsa_period;
        if output_samples > 0 && !info.fits {
            let fitting = BUFFERS.iter().rev().copied().find(|&samples| {
                samples as u32 <= graph_samples && graph_samples.is_multiple_of(samples as u32)
            });
            let action = match fitting {
                // The buffer is what the output asked for: changing it helps.
                Some(samples) if output_samples as usize == self.output_buffer => {
                    IssueAction::SetOutputBuffer(samples)
                }
                _ if self.low_latency_output => IssueAction::LowLatencyOff,
                _ => IssueAction::OpenDiagnostics,
            };
            let detail = if output_samples > graph_samples {
                format!(
                    "The output buffer ({output_samples} samples) is larger than the PipeWire graph buffer ({graph_samples} samples), so the coarser buffer sets the latency."
                )
            } else {
                format!(
                    "The output buffer ({output_samples} samples) does not divide the PipeWire graph buffer ({graph_samples} samples), so the two clocks drift against each other and can cause a dropout."
                )
            };
            return Some(LiveIssue {
                key: IssueKey::QuantumMisfit,
                severity: IssueSeverity::Warning,
                title: "The buffer sizes do not line up".to_owned(),
                detail,
                place: "Setup".to_owned(),
                anchor,
                action,
            });
        }
        let running = self.engine.state == EngineState::Running;
        (!info.low_latency && self.low_latency_output && running).then(|| LiveIssue {
            key: IssueKey::QuantumNotLow,
            severity: IssueSeverity::Warning,
            title: "Low latency is on, but the graph buffer is not".to_owned(),
            detail: format!(
                "The desktop PipeWire graph runs {graph_samples} samples ({:.1} ms), not the {PIPEWIRE_LOW_QUANTUM} samples RigWerk asked for. Another app may hold the graph buffer.",
                graph_samples as f32 / info.rate.max(1) as f32 * 1000.0
            ),
            place: "Setup".to_owned(),
            anchor,
            action: IssueAction::OpenDiagnostics,
        })
    }

    /// The mode shows what was asked for, not what the driver allows.  Not
    /// while a check is running.
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
            detail: "Ask the sound system which rate and buffer size this output allows before trusting the output mode. Opens no audio.".to_owned(),
            place: "Setup".to_owned(),
            anchor: Anchor::Setup(crate::app::screen::SetupSection::Devices),
            action: IssueAction::CheckDriver,
        });
    }

    fn dropout_issue(&self, issues: &mut Vec<LiveIssue>) {
        if self.engine.state != EngineState::Running {
            return;
        }
        let watch = &self.issue_state.dropouts;
        let Some(episode) = watch.episode() else {
            return;
        };
        let when = ago_text(episode.since_last);
        let action = self.safer_latency_action();
        let remedy = match action {
            IssueAction::AutoBridgeBuffer => {
                "An Auto bridge buffer grows after each dropout.".to_owned()
            }
            IssueAction::LowLatencyOff => {
                "Turning Low latency off gives the output a larger buffer.".to_owned()
            }
            IssueAction::SharedOutput => {
                "Shared output uses the chosen buffer instead of the driver's smallest one."
                    .to_owned()
            }
            IssueAction::SetOutputBuffer(frames) => {
                format!("A {frames}-sample output buffer gives it more room.")
            }
            _ => "Check the DSP load and the driver in Diagnostics.".to_owned(),
        };
        let (severity, title) = if episode.burst {
            (IssueSeverity::Error, "Dropouts on the output")
        } else {
            (IssueSeverity::Warning, "The output had a dropout")
        };
        issues.push(LiveIssue {
            key: IssueKey::Dropouts,
            severity,
            title: title.to_owned(),
            detail: format!(
                "{}, the last {when} ({} this run): the output played gaps. {remedy}",
                dropout_text(episode.count),
                dropout_text(watch.run_count())
            ),
            place: "Clock output".to_owned(),
            anchor: Anchor::Output(0),
            action,
        });
    }

    /// An error only with recent dropouts.  Without them the stream may
    /// just be muted in the desktop mixer.
    fn silence_issue(&self, rows: &[OutputDeviceRow], issues: &mut Vec<LiveIssue>) {
        if self.engine.state != EngineState::Running || !self.issue_state.silence.silent() {
            return;
        }
        let name = self.selected_output().name.clone();
        let device = rows.iter().position(|row| row.name == name).unwrap_or(0);
        let (severity, title, detail) = if self.issue_state.dropouts.episode().is_some() {
            (
                IssueSeverity::Error,
                "Output went silent under low latency",
                format!(
                    "RigWerk sends signal to {name}, but PipeWire's monitor of that output hears nothing, and the output had a dropout. The graph buffer is too small for this system; turning Low latency off gives it a larger one."
                ),
            )
        } else {
            (
                IssueSeverity::Warning,
                "Output may be silent under low latency",
                format!(
                    "RigWerk sends signal to {name}, but PipeWire's monitor of that output hears nothing. The RigWerk stream may be muted or turned down in the desktop mixer; if not, the graph buffer is too small, and turning Low latency off gives it a larger one."
                ),
            )
        };
        issues.push(LiveIssue {
            key: IssueKey::SinkSilent,
            severity,
            title: title.to_owned(),
            detail,
            place: output_place(rows, device),
            anchor: Anchor::Output(device),
            action: IssueAction::LowLatencyOff,
        });
    }

    /// Muted, missing and switched-off virtual devices are not playing.
    fn safety_ceiling_issue(&self, rows: &[OutputDeviceRow], issues: &mut Vec<LiveIssue>) {
        if self.engine.state != EngineState::Running {
            return;
        }
        let mut worst: Option<(usize, String, f32)> = None;
        for (device, row) in rows.iter().enumerate() {
            if row.settings.mute || row.missing || (row.virtual_output && !row.virtual_output_on) {
                continue;
            }
            let gr = row.reduction_db;
            if gr.is_finite()
                && gr >= SAFETY_GR_DB
                && worst.as_ref().is_none_or(|(_, _, top)| gr > *top)
            {
                worst = Some((device, row.name.clone(), gr));
            }
        }
        let Some((device, name, gr)) = worst else {
            return;
        };
        issues.push(LiveIssue {
            key: IssueKey::SafetyCeiling,
            severity: IssueSeverity::Warning,
            title: format!("{name} is against the safety ceiling"),
            detail: format!("−{:.1} dB · {gr:.1} dB reduction.", -SAFETY_CEILING_DB),
            place: output_place(rows, device),
            anchor: Anchor::Output(device),
            action: IssueAction::ShowOutput(device),
        });
    }

    /// A missing extra output plays nothing, so it gets that one row only.
    fn extra_output_issues(&self, rows: &[OutputDeviceRow], issues: &mut Vec<LiveIssue>) {
        if self.engine.state != EngineState::Running {
            return;
        }
        for row in rows {
            let Some(slot) = row
                .extra_output_index
                .filter(|&slot| slot < MAX_EXTRA_OUTPUTS)
            else {
                continue;
            };
            let name = &row.name;
            let place = output_place(rows, slot + 1);
            let anchor = Anchor::Output(slot + 1);
            let retrying = self.snapshot.extra_output_missing[slot];
            let drifting = self.issue_state.extra_output_drift[slot].raised();
            let dropping = self.issue_state.extra_output_dropouts[slot]
                .episode()
                .is_some();
            if !retrying && !row.missing && !drifting && !dropping {
                continue;
            }
            if retrying || row.missing {
                issues.push(LiveIssue {
                    key: if retrying {
                        IssueKey::ExtraOutputRetrying(slot)
                    } else {
                        IssueKey::ExtraOutputNotListed(slot)
                    },
                    severity: IssueSeverity::Warning,
                    title: format!("{name} is not playing"),
                    detail: if retrying {
                        "It failed to open or was disconnected. RigWerk keeps retrying it; the clock output and the other outputs play on.".to_owned()
                    } else {
                        "It is not among the output devices right now. The clock output and the other outputs play on.".to_owned()
                    },
                    place,
                    anchor,
                    action: IssueAction::ShowExtraOutput(slot),
                });
                continue;
            }
            let watch = &self.issue_state.extra_output_dropouts[slot];
            if let Some(episode) = watch.episode() {
                let action = self.extra_output_safer_action(slot);
                let remedy = match action {
                    IssueAction::ExtraOutputAutoBridgeBuffer(_) => {
                        format!("An Auto bridge buffer on {name} grows after each dropout.")
                    }
                    IssueAction::SetOutputBuffer(frames) => format!(
                        "Extra outputs ask for the output buffer: {frames} samples give {name} more room."
                    ),
                    _ => "Check the extra output and its driver in Diagnostics.".to_owned(),
                };
                let (severity, title) = if episode.burst {
                    (IssueSeverity::Error, format!("Dropouts on {name}"))
                } else {
                    (IssueSeverity::Warning, format!("{name} had a dropout"))
                };
                issues.push(LiveIssue {
                    key: IssueKey::ExtraOutputDropouts(slot),
                    severity,
                    title,
                    detail: format!(
                        "{}, the last {} ({} since it opened): it played gaps. {remedy}",
                        dropout_text(episode.count),
                        ago_text(episode.since_last),
                        dropout_text(watch.run_count())
                    ),
                    place: place.clone(),
                    anchor,
                    action,
                });
            }
            if self.issue_state.extra_output_drift[slot].raised() {
                issues.push(LiveIssue {
                    key: IssueKey::ExtraOutputDrift(slot),
                    severity: IssueSeverity::Warning,
                    title: format!("{name} drifts too far to follow"),
                    detail: format!(
                        "Its drift correction is at the {EXTRA_OUTPUT_DRIFT_LIMIT_PPM:.0} ppm limit, so its bridge buffer keeps filling or emptying. The device may run at another rate than it reports."
                    ),
                    place,
                    anchor,
                    action: IssueAction::OpenDiagnostics,
                });
            }
        }
    }

    /// Low latency and the WASAPI mode govern the clock output only.
    pub(crate) fn extra_output_safer_action(&self, extra_output: usize) -> IssueAction {
        let safe = self.extra_outputs.get(extra_output).is_none_or(|pref| {
            bridge_buffer_from_pref(pref.bridge_buffer_mode) == BufferMode::Auto
        });
        if !safe {
            return IssueAction::ExtraOutputAutoBridgeBuffer(extra_output);
        }
        BUFFERS
            .iter()
            .copied()
            .find(|&frames| frames > self.output_buffer)
            .map_or(IssueAction::OpenDiagnostics, IssueAction::SetOutputBuffer)
    }

    /// Auto bridge buffer, then low latency off, then a larger output
    /// buffer: the most room for the least added latency.
    pub(crate) fn safer_latency_action(&self) -> IssueAction {
        if !self.snapshot.direct && self.bridge_buffer_mode != BufferMode::Auto {
            return IssueAction::AutoBridgeBuffer;
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

    pub(crate) fn apply_issue_action(&mut self, action: IssueAction) {
        match action {
            IssueAction::AlignDevice(device) => {
                self.set_device_align(device, true);
                let name = self
                    .output_device_rows()
                    .get(device)
                    .map_or_else(|| "Output device".to_owned(), |row| row.name.clone());
                self.push_log(
                    format!("{name}: alignment on, the paths line up again."),
                    LogTone::Teal,
                );
            }
            IssueAction::ShowRouting(device) => {
                self.routing_device = device;
                self.open_sheet(crate::app::screen::Sheet::Output { output: device });
            }
            IssueAction::OpenRouting => {
                self.open_sheet(crate::app::screen::Sheet::Output { output: 0 });
            }
            IssueAction::OpenDiagnostics => self.open_diagnostics(),
            IssueAction::OpenDevices => {
                self.open_setup(crate::app::screen::SetupSection::Devices);
            }
            IssueAction::CheckDriver => self.start_output_driver_check(true),
            IssueAction::SetOutputBuffer(frames) => {
                self.set_output_buffer(frames);
                self.push_log(
                    format!("Output buffer set to {} samples.", self.output_buffer),
                    LogTone::Blue,
                );
            }
            IssueAction::LowLatencyOff => self.set_low_latency_output(false),
            IssueAction::SharedOutput => {
                self.set_wasapi_output_mode(crate::audio::WasapiOutputMode::Shared);
                self.push_log("WASAPI clock: Shared output.", LogTone::Muted);
            }
            IssueAction::AutoBridgeBuffer => {
                self.set_bridge_buffer_mode(BufferMode::Auto);
                self.push_log("Bridge buffer: Auto.", LogTone::Amber);
            }
            IssueAction::ShowExtraOutput(extra_output) => {
                // Row 0 is the clock output.
                self.open_sheet(crate::app::screen::Sheet::Output {
                    output: extra_output + 1,
                });
            }
            IssueAction::ShowOutput(device) => {
                self.open_sheet(crate::app::screen::Sheet::Output { output: device });
            }
            IssueAction::ExtraOutputAutoBridgeBuffer(extra_output) => {
                let Some(pref) = self.extra_outputs.get_mut(extra_output) else {
                    return;
                };
                pref.bridge_buffer_mode = bridge_buffer_to_pref(BufferMode::Auto);
                let name = if pref.name.is_empty() {
                    pref.id.clone()
                } else {
                    pref.name.clone()
                };
                self.push_extra_outputs_to_engine();
                self.push_log(format!("{name}: Bridge buffer: Auto."), LogTone::Amber);
            }
        }
    }
}

fn output_place(rows: &[OutputDeviceRow], device: usize) -> String {
    if device == 0 {
        "Clock output".to_owned()
    } else {
        rows.get(device)
            .map(|row| row.name.clone())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "Clock output".to_owned())
    }
}

fn same_strip(issue: Anchor, anchor: Anchor) -> bool {
    match anchor {
        Anchor::Path(slot) => matches!(issue, Anchor::Path(path) if path == slot),
        Anchor::Output(index) => matches!(issue, Anchor::Output(output) if output == index),
        Anchor::Setup(section) => matches!(issue, Anchor::Setup(have) if have == section),
        other => issue == other,
    }
}

fn dropout_text(count: u64) -> String {
    if count == 1 {
        "1 dropout".to_owned()
    } else {
        format!("{count} dropouts")
    }
}

fn ago_text(since: Duration) -> String {
    match since.as_secs() {
        0 => "just now".to_owned(),
        ago => format!("{ago} s ago"),
    }
}

mod silence;

#[cfg(test)]
mod tests;
