//! Issues: the judgments the console, Setup and the sheets raise.  Readings
//! stay on the cards; a problem becomes one row here, with a severity,
//! where it is and one action that only runs on a click.
//!
//! An error means the audio itself is about to break (a block nearly
//! late); anything that only colours the sound, such as a comb filter, is
//! a warning.
//!
//! Producers so far are the signal paths':
//! - unaligned paths that add up on one device channel (the output sheet's
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
//! - dropouts in this bridge run ([`DropoutWatch`]): a warning that
//!   clears after [`DROPOUT_QUIET`] without a new one, and an error once
//!   [`DROPOUT_BURST`] of them fall within [`DROPOUT_BURST_WINDOW`], held
//!   until the output goes quiet.
//! - a sink that went silent under Low latency ([`SilenceWatch`]): RigWerk
//!   sends a loud signal while PipeWire's monitor of the clock sink stays
//!   quiet.  An error with recent dropouts, since the graph is starving;
//!   the fix is Low latency off.  Without them a warning that also names
//!   the desktop mixer: RigWerk's meter is read before PipeWire's stream
//!   volume, so a muted or turned-down RigWerk stream looks the same.
//!   Only while Low latency runs and the monitor delivers blocks: without
//!   that reading there is no row, never a guess.
//! - a playing device against the safety ceiling (−1.0 dBFS): one warning
//!   while any unmuted device is reducing by at least 0.5 dB, naming the
//!   one with the most reduction.  It cannot be dismissed while it holds
//!   and clears once the signal drops.  → Show on Output.
//!
//! Each extra output's own (a device that plays the master, by its slot),
//! placed as `OUTPUT · <name>` on the Output card, and only while the
//! bridge runs:
//! - an extra output that failed to open, was lost, or is not among the output
//!   devices: a warning, since the clock output and the others play on and
//!   the extra output is retried.  The button opens its output sheet, where it can
//!   be pointed at another device or removed.
//! - dropouts of its own ring (`ExtraOutputBus::dropouts`, counted since its
//!   stream opened: a reopen starts over), one [`DropoutWatch`] per slot
//!   with the clock output's rules.  The safer
//!   step names the extra output's own controls: its bridge buffer (Auto), then the
//!   output buffer its stream asks for.  Never Low latency or the WASAPI
//!   mode: those govern the clock output only.
//! - a drift loop pinned at [`EXTRA_OUTPUT_DRIFT_LIMIT_PPM`]: its clock is
//!   further off than the loop can follow.  A warning to Open diagnostics,
//!   held by a [`HeldFlag`] so an integral hovering at the limit does not
//!   flicker: raised after [`DRIFT_RAISE_AFTER`] pinned, cleared after
//!   [`DRIFT_CLEAR_AFTER`] free.
//!
//! PipeWire in-graph extra outputs have no ring of their own; the graph keeps
//! them in step and only the missing check applies to them.
//!
//! A slot that holds another extra output than the last tick saw (one removed,
//! reordered or pointed at another device while the bridge runs) starts
//! its watches over and drops its dismissals: they were about the old one.
//! Its dropout watch starts at the slot's current count, which may still
//! be the old stream's until the new one opens.
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
//! latches and the dropout watches start over with each bridge start and
//! stop.  A new error (a title not among the last tick's errors) expands a
//! collapsed strip; warnings do not.
//!
//! What the person does with the strip is app state too, never persisted:
//! - the filter (All, Warnings, Errors).  Choosing one expands the strip.
//!   It falls back to All when nothing is left to show, and when a new
//!   error opens the strip while it shows only warnings.
//! - dismissed warnings, by their stable [`IssueKey`] (the kind and the
//!   device or path slot, never the title, which carries numbers).  One
//!   stays hidden until a tick no longer has it as a warning (it cleared,
//!   or it became an error), or until the bridge run changes.  Errors
//!   cannot be dismissed: audio is breaking, so they stay until fixed.
//!   Showing the dismissed ones again is one click.
//! - the card a row relates to ([`IssueCard`]) rings for
//!   [`CARD_HIGHLIGHT_FOR`] after "Show on <card>"; nothing scrolls or
//!   moves, and the time is passed in so the pulse can be tested.
//!
//! The cards keep the readings (DRIVER, BUFFER, MODE); none of these
//! judgments is drawn there.  The fixes the output rows offer are the
//! controls the cards already have: Check driver, the output buffer, Low
//! latency, the WASAPI mode and the bridge buffer, and an extra output's own bridge buffer
//! in the output sheet.

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
    BufferMode, EngineState, EXTRA_OUTPUT_DRIFT_LIMIT_PPM, MAX_ALIGN, MAX_EXTRA_OUTPUTS,
    SAFETY_CEILING_DB, SAFETY_GR_DB,
};
#[cfg(target_os = "linux")]
use crate::audio::{QuantumInfo, PIPEWIRE_LOW_QUANTUM};

/// The smoothed DSP share of the block that raises the warning, and the
/// share it has to drop under to clear it.
pub(crate) const DSP_WARN_SHARE: f32 = 0.70;
pub(crate) const DSP_WARN_CLEAR: f32 = 0.60;
/// The peak DSP share that raises the error (the block is nearly late),
/// and the share it has to drop under to clear it.
pub(crate) const DSP_ERROR_SHARE: f32 = 0.95;
pub(crate) const DSP_ERROR_CLEAR: f32 = 0.85;

/// How long the output has to play without a new dropout before its
/// issue clears.
pub(crate) const DROPOUT_QUIET: Duration = Duration::from_secs(10);
/// This many dropouts within [`DROPOUT_BURST_WINDOW`] turn the warning
/// into an error: the audio is breaking up, not clicking once.
pub(crate) const DROPOUT_BURST: usize = 3;
pub(crate) const DROPOUT_BURST_WINDOW: Duration = Duration::from_secs(5);

/// How long an extra output's drift loop has to stay pinned before it warns,
/// and how long free before the warning clears.
pub(crate) const DRIFT_RAISE_AFTER: Duration = Duration::from_secs(1);
pub(crate) const DRIFT_CLEAR_AFTER: Duration = Duration::from_secs(3);

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

/// A run of dropouts that has not yet gone quiet for [`DROPOUT_QUIET`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DropoutEpisode {
    /// Dropouts since the run began.
    pub(crate) count: u64,
    /// Time since the latest one, as of the last update.
    pub(crate) since_last: Duration,
    /// Whether a burst was seen in this run.  Held until the run ends.
    pub(crate) burst: bool,
    last: Instant,
}

/// Follows the run's dropout count from frame to frame.  Time is
/// passed in, so the clearing and the burst rule can be tested.
#[derive(Clone, Debug, Default)]
pub(crate) struct DropoutWatch {
    /// The run count at the last update.
    seen: u64,
    /// When the latest dropouts arrived, oldest first, at most
    /// [`DROPOUT_BURST`] of them.
    recent: VecDeque<Instant>,
    episode: Option<DropoutEpisode>,
}

impl DropoutWatch {
    /// Takes the run's dropout count at `now`.  A count lower than the
    /// last one is a new run and starts over.
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

    /// A watch that takes `count` as already seen: no episode from it.
    pub(crate) fn starting_at(count: u64) -> Self {
        Self {
            seen: count,
            ..Self::default()
        }
    }

    /// The current run of dropouts, if the output has not gone quiet.
    pub(crate) fn episode(&self) -> Option<&DropoutEpisode> {
        self.episode.as_ref()
    }

    /// The count it was last given: dropouts in this bridge run, or
    /// for an extra output since its stream opened.
    pub(crate) fn run_count(&self) -> u64 {
        self.seen
    }
}

/// A raw per-tick flag with a hold: it raises once the input has been set
/// for `raise_after`, and clears once it has been clear for `clear_after`.
/// A flip back before then restarts the wait.  Time is passed in, so the
/// hold can be tested.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct HeldFlag {
    raised: bool,
    /// Since when the input has disagreed with `raised`.
    pending: Option<Instant>,
}

impl HeldFlag {
    /// Takes the raw flag at `now` and returns the held one.
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
    /// PipeWire did not take the buffer change.
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
    Dropouts,
    /// Extra output slot: failed to open or lost, and being retried.
    ExtraOutputRetrying(usize),
    /// Extra output slot: not among the output devices.
    ExtraOutputNotListed(usize),
    /// Extra output slot.
    ExtraOutputDropouts(usize),
    /// Extra output slot.
    ExtraOutputDrift(usize),
    SinkSilent,
    /// A playing device is against the safety ceiling.  One row: the device
    /// with the most reduction.
    SafetyCeiling,
}

impl IssueKey {
    /// The extra output slot the issue is about, if it is an extra output's.
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
    pub(crate) dropouts: DropoutWatch,
    /// One watch per extra output slot, on that extra output's own ring.
    pub(crate) extra_output_dropouts: [DropoutWatch; MAX_EXTRA_OUTPUTS],
    /// One held drift-at-limit flag per extra output slot.
    pub(crate) extra_output_drift: [HeldFlag; MAX_EXTRA_OUTPUTS],
    /// The extra output id each slot's state is about, `None` until the first
    /// tick adopts the slots as they are.
    extra_output_ids: Option<[String; MAX_EXTRA_OUTPUTS]>,
    /// Whether the clock sink went silent under Low latency.
    pub(crate) silence: SilenceWatch,
    /// The alert of a bridge stopped on a disconnected device, until the
    /// next bridge start.
    pub(crate) device_lost: Option<String>,
    /// The error titles of the last tick, to tell a new error from one the
    /// person already collapsed away.
    seen_errors: Vec<String>,
    /// Which rows the strip lists.
    pub(crate) filter: IssueFilter,
    /// Warnings the person dismissed, until they clear or the run ends.
    dismissed: Vec<IssueKey>,
    /// The card "Show on <card>" rings, until [`CARD_HIGHLIGHT_FOR`] passed.
    highlight: Option<CardHighlight>,
    /// The issues of the last tick, errors first: what the strip shows.
    current: Vec<LiveIssue>,
}

impl IssueState {
    /// A bridge started or stopped: the DSP latches, the dropout count and
    /// the dismissed warnings belong to the run that ended.
    pub(crate) fn new_run(&mut self) {
        self.dsp = DspAlarm::default();
        self.dropouts = DropoutWatch::default();
        self.extra_output_dropouts = Default::default();
        self.extra_output_drift = Default::default();
        self.silence = SilenceWatch::default();
        self.dismissed.clear();
    }

    /// Extra output `slot` holds another device now: its watches and its
    /// dismissals were about the old one.  The dropout watch takes the
    /// slot's current `count` as seen.
    fn forget_extra_output(&mut self, slot: usize, count: u64) {
        self.extra_output_dropouts[slot] = DropoutWatch::starting_at(count);
        self.extra_output_drift[slot] = HeldFlag::default();
        self.dismissed
            .retain(|key| key.extra_output_slot() != Some(slot));
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
    /// Open the output sheet for `device`.
    ShowRouting(usize),
    /// Open the clock output's sheet.
    OpenRouting,
    OpenDiagnostics,
    /// Open Setup at the devices section (reconnect or pick another device).
    OpenDevices,
    /// Run the stand-alone output driver check.
    CheckDriver,
    /// Set the clock output's buffer to this many samples.
    SetOutputBuffer(usize),
    /// Turn the Low latency output off (PipeWire / ALSA).
    LowLatencyOff,
    /// Put the WASAPI clock output back to Shared.
    SharedOutput,
    /// Set the input-to-output bridge buffer to Auto.
    AutoBridgeBuffer,
    /// Open extra output `extra_output`'s sheet (index into the extra outputs).
    ShowExtraOutput(usize),
    /// Set extra output `extra_output`'s own bridge buffer to Auto.
    ExtraOutputAutoBridgeBuffer(usize),
    /// Ring the Output card.  The safety ceiling has no control of its own.
    ShowOutput,
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
            IssueAction::ShowExtraOutput(_) => "Show output".to_owned(),
            IssueAction::ShowOutput => "Show on Output".to_owned(),
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

    /// Only warnings can be dismissed.  The safety ceiling stays while a
    /// device is still against it: it clears when the signal drops, not
    /// when someone clicks.
    pub(crate) fn dismissable(&self) -> bool {
        self.severity == IssueSeverity::Warning && self.key != IssueKey::SafetyCeiling
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
    /// Moves the issue state once per update tick: the dropout watches,
    /// the extra output drift holds and the DSP latches from the engine, then the dismissals, the filter and
    /// the card highlight settle, and a collapsed strip opens for a new
    /// error.
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
        self.issue_state.settle(&issues, now);
        self.open_on_new_error(&issues);
        self.issue_state.current = issues;
    }

    /// Starts an extra output slot's state over when it holds another extra output
    /// than the last tick saw.
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

    /// This tick's readings for the silence watch.  Armed while a PipeWire
    /// Low latency bridge runs (Linux only: the sink monitor and its
    /// Turn Low latency off action are PipeWire's, not WASAPI's); parked when nothing should reach the clock
    /// output (master mute, its own mute, or no unmuted path routed to it).
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
            parked: self.mute || !self.monitor || self.clock_output.mute || !routed,
            dropouts: self.issue_state.dropouts.episode().is_some(),
        }
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
        let fresh = errors
            .iter()
            .any(|title| !self.issue_state.seen_errors.contains(title));
        if fresh {
            self.live_issues_collapsed = false;
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
    /// bridge run changes.  An error is not hidden.  Goes by the last
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
        self.close_drawer();
        if card == IssueCard::Output {
            self.open_sheet(crate::app::screen::Sheet::Output { output: 0 });
        }
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
        self.dropout_issue(&mut issues);
        self.silence_issue(&mut issues);
        self.safety_ceiling_issue(&rows, &mut issues);
        self.extra_output_issues(&rows, &mut issues);
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
                    crate::app::alignment::channel_list(&warning.channels),
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
        // The screenshot renderer's startup peak is not the rig's: a shot
        // waits on Xvfb's software GL, not on audio.
        if self.screenshot.is_some() {
            return;
        }
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
        let block_name = if self.snapshot.direct {
            "graph buffer"
        } else {
            "output block"
        };
        let (name, most) = heaviest.unwrap_or_default();
        let shares = format!(
            "{:.0}% of the {:.1} ms {block_name} on average, {:.0}% at peak",
            avg_share * 100.0,
            block / 1e6,
            peak_share * 100.0
        );
        let detail = if infos.len() > 1 {
            format!(
                "{shares}; {name} takes {:.2} ms. Remove an effect or raise the buffer.",
                most / 1e6
            )
        } else {
            format!("{shares}. Remove an effect or raise the buffer.")
        };
        issues.push(LiveIssue {
            key: IssueKey::Dsp,
            severity,
            title: if infos.len() > 1 {
                format!("Paths overload the {block_name}")
            } else {
                format!("The chain overloads the {block_name}")
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
                title: "PipeWire did not take the buffer change".to_owned(),
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
        let graph_frames = info.quantum;
        if !info.available || graph_frames == 0 {
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
            let fitting = BUFFERS.iter().rev().copied().find(|&frames| {
                frames as u32 <= graph_frames && graph_frames.is_multiple_of(frames as u32)
            });
            let action = match fitting {
                // The buffer is what the output asked for: changing it helps.
                Some(frames) if period as usize == self.output_buffer => {
                    IssueAction::SetOutputBuffer(frames)
                }
                _ if self.low_latency_output => IssueAction::LowLatencyOff,
                _ => IssueAction::OpenDiagnostics,
            };
            let detail = if period > graph_frames {
                format!(
                    "The output buffer ({period} frames) is larger than the PipeWire graph buffer ({graph_frames} frames), so the coarser buffer sets the latency."
                )
            } else {
                format!(
                    "The output buffer ({period} frames) does not divide the PipeWire graph buffer ({graph_frames} frames), so the two clocks drift against each other and can cause a dropout."
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
                "The desktop PipeWire graph runs {graph_frames} frames ({:.1} ms), not the {PIPEWIRE_LOW_QUANTUM} RigWerk asked for. Another app may hold the graph buffer.",
                graph_frames as f32 / info.rate.max(1) as f32 * 1000.0
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

    /// (f) Dropouts in this bridge run: the output played a gap.  An
    /// error while they come in a burst.
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
            place: format!("OUTPUT · {}", self.selected_output().name),
            card: Some(IssueCard::Output),
            action,
        });
    }

    /// The clock sink went silent while RigWerk plays into it under Low
    /// latency.  Turning Low latency off gives the graph a larger block.
    /// An error only with recent dropouts: without them the RigWerk
    /// stream may just be muted in the desktop mixer.
    fn silence_issue(&self, issues: &mut Vec<LiveIssue>) {
        if self.engine.state != EngineState::Running || !self.issue_state.silence.silent() {
            return;
        }
        let name = self.selected_output().name.clone();
        let (severity, title, detail) = if self.issue_state.dropouts.episode().is_some() {
            (
                IssueSeverity::Error,
                "Output went silent under low latency",
                format!(
                    "RigWerk sends signal to {name}, but PipeWire's monitor of that sink hears nothing, and the output had a dropout. The graph block is too small for this system; turning Low latency off gives it a larger one."
                ),
            )
        } else {
            (
                IssueSeverity::Warning,
                "Output may be silent under low latency",
                format!(
                    "RigWerk sends signal to {name}, but PipeWire's monitor of that sink hears nothing. The RigWerk stream may be muted or turned down in the desktop mixer; if not, the graph block is too small, and turning Low latency off gives it a larger one."
                ),
            )
        };
        issues.push(LiveIssue {
            key: IssueKey::SinkSilent,
            severity,
            title: title.to_owned(),
            detail,
            place: "OUTPUT · Low latency".to_owned(),
            card: Some(IssueCard::Output),
            action: IssueAction::LowLatencyOff,
        });
    }

    /// A playing device held at the safety ceiling.  One warning, for the
    /// device reducing the most.  Muted, missing and switched-off virtual
    /// devices are not playing.  The row cannot be dismissed while the
    /// reduction holds; the next tick drops it once every device is under
    /// the threshold.
    fn safety_ceiling_issue(&self, rows: &[OutputDeviceRow], issues: &mut Vec<LiveIssue>) {
        if self.engine.state != EngineState::Running {
            return;
        }
        let mut worst: Option<(String, f32)> = None;
        for row in rows {
            if row.settings.mute || row.missing || (row.virtual_output && !row.virtual_output_on) {
                continue;
            }
            let gr = row.reduction_db;
            if gr.is_finite()
                && gr >= SAFETY_GR_DB
                && worst.as_ref().is_none_or(|(_, top)| gr > *top)
            {
                worst = Some((row.name.clone(), gr));
            }
        }
        let Some((name, gr)) = worst else {
            return;
        };
        issues.push(LiveIssue {
            key: IssueKey::SafetyCeiling,
            severity: IssueSeverity::Warning,
            title: format!("{name} is against the safety ceiling"),
            detail: format!("−{:.1} dB · {gr:.1} dB reduction.", -SAFETY_CEILING_DB),
            place: format!("OUTPUT · {name}"),
            card: Some(IssueCard::Output),
            action: IssueAction::ShowOutput,
        });
    }

    /// (g) Each extra output on its own: missing, then its ring's dropouts,
    /// then its drift loop.  A missing extra output plays nothing, so it gets
    /// that one row only.
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
            let place = format!("OUTPUT · {name}");
            let retrying = self.snapshot.extra_output_missing[slot];
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
                    card: Some(IssueCard::Output),
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
                    card: Some(IssueCard::Output),
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
                    card: Some(IssueCard::Output),
                    action: IssueAction::OpenDiagnostics,
                });
            }
        }
    }

    /// The extra output's own safer step: its bridge buffer to Auto, then the next
    /// larger output buffer (its stream asks for that one).  Low latency
    /// and the WASAPI mode only govern the clock output.
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

    /// The control that buys the output the most safety for the least
    /// latency: the Auto bridge buffer, then Low latency (or REAL-style /
    /// Exclusive) off, then the next larger output buffer.
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

    /// Runs an issue's action (a click on its button).
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
                self.output_buffer = frames;
                self.push_log(
                    format!("Output buffer set to {frames} samples."),
                    LogTone::Blue,
                );
            }
            IssueAction::LowLatencyOff => self.set_low_latency_output(false),
            IssueAction::SharedOutput => {
                self.set_wasapi_output_mode(crate::audio::WasapiOutputMode::Shared);
                self.push_log("WASAPI clock: Shared output.", LogTone::Muted);
            }
            IssueAction::AutoBridgeBuffer => {
                self.bridge_buffer_mode = BufferMode::Auto;
                self.apply_bridge_buffer();
                self.push_log("Bridge buffer: Auto.", LogTone::Amber);
            }
            IssueAction::ShowExtraOutput(extra_output) => {
                // Row 0 is the clock output.
                self.open_sheet(crate::app::screen::Sheet::Output {
                    output: extra_output + 1,
                });
            }
            IssueAction::ShowOutput => {
                self.show_issue_card(IssueCard::Output, IssueSeverity::Warning, Instant::now());
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

/// "1 dropout", "3 dropouts".
fn dropout_text(count: u64) -> String {
    if count == 1 {
        "1 dropout".to_owned()
    } else {
        format!("{count} dropouts")
    }
}

/// How long ago the latest dropout was, in whole seconds.
fn ago_text(since: Duration) -> String {
    match since.as_secs() {
        0 => "just now".to_owned(),
        ago => format!("{ago} s ago"),
    }
}

mod silence;

#[cfg(test)]
mod tests;
