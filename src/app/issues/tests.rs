use std::sync::mpsc;
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use super::silence::{SILENCE_RAISE_ALONE, SILENCE_RAISE_WITH_UNDERRUNS};
use super::{
    highlight_strength, DspAlarm, HeldFlag, IssueAction, IssueCard, IssueFilter, IssueKey,
    IssueSeverity, UnderrunWatch, CARD_HIGHLIGHT_FOR, DRIFT_CLEAR_AFTER, DRIFT_RAISE_AFTER,
    UNDERRUN_BURST_WINDOW, UNDERRUN_QUIET,
};
use crate::app::cards::Card;
use crate::app::driver_check::{DriverActual, DriverCheck, DriverTone};
use crate::app::settings::{cushion_to_pref, FollowerPref};
use crate::app::{blank_app, Page, RigWerkApp};
use crate::audio::{ChannelMap, CushionMode, DeviceItem, EngineState, PathMapSettings, MAX_ALIGN};
use crate::ui::pages::mixer::chain::ChainSelection;

/// A real (not demo) clock output the driver check can read.
fn with_real_output(app: &mut RigWerkApp) {
    app.inventory.outputs.push(DeviceItem {
        id: "test:out".to_owned(),
        name: "Scarlett 2i2".to_owned(),
        backend: if cfg!(target_os = "windows") {
            "WASAPI"
        } else {
            "ALSA"
        }
        .to_owned(),
        channels: 2,
        demo: false,
        preferred: false,
        asio_clsid: None,
    });
}

#[test]
fn one_path_has_no_path_issues() {
    let app = blank_app();
    assert!(app.live_issues().is_empty());
}

#[test]
fn an_unaligned_sum_is_a_warning_that_aligns_on_a_click() {
    let mut app = blank_app();
    assert!(app.apply_card(Card::Blend));
    assert!(app.live_issues().is_empty(), "aligned, same latency");
    let second = app.path_slots()[1];
    app.engine.screenshot_path_latency(second, 480);
    assert!(
        app.live_issues().is_empty(),
        "aligned: the first path waits"
    );
    app.set_device_align(0, false);
    let issues = app.live_issues();
    assert_eq!(issues.len(), 1);
    let issue = &issues[0];
    assert_eq!(
        issue.severity,
        IssueSeverity::Warning,
        "a comb filter colours the sound; it does not break it"
    );
    assert!(issue.title.starts_with("Paths add up unaligned on "));
    assert!(issue.detail.contains("Main and Main 2"), "{}", issue.detail);
    assert!(issue.detail.contains("10.0 ms apart"), "{}", issue.detail);
    assert_eq!(issue.action, IssueAction::AlignDevice(0));
    assert!(issue.action_name().starts_with("Align paths: Paths add up"));
    app.apply_issue_action(issue.action);
    assert!(app.device_align(0));
    assert!(app.log.last().unwrap().text.contains(": alignment on"));
    assert!(app.live_issues().is_empty());
    app.engine.screenshot_path_latency(second, 0);
}

#[test]
fn a_path_past_the_alignment_limit_warns() {
    let mut app = blank_app();
    assert!(app.apply_card(Card::Blend));
    let second = app.path_slots()[1];
    app.engine.screenshot_path_latency(second, MAX_ALIGN + 1);
    let issues = app.live_issues();
    assert!(
        issues
            .iter()
            .any(|issue| issue.title == "Main 2 is past the alignment limit"),
        "{issues:?}"
    );
    app.engine.screenshot_path_latency(second, 0);
}

#[test]
fn an_unmuted_path_no_device_plays_warns() {
    let mut app = blank_app();
    let added = app.add_path().expect("a free slot");
    assert!(
        app.live_issues().is_empty(),
        "a fresh path is muted and routed nowhere on purpose"
    );
    app.set_path_mute(added, false);
    assert_eq!(app.live_issues()[0].title, "Path 2 is routed nowhere");
    app.clock_device.path_map[added].map = ChannelMap::default();
    assert!(app.live_issues().is_empty());
    app.clock_device.mute = true;
    let issues = app.live_issues();
    assert!(issues
        .iter()
        .any(|issue| issue.title == "Path 2 is routed nowhere audible"));
    app.apply_issue_action(issues[0].action);
    assert!(app.page == Page::Routing);
}

#[test]
fn the_only_path_routed_nowhere_warns() {
    let mut app = blank_app();
    let main = app.path_slots()[0];
    app.clock_device.path_map[main] = PathMapSettings::unrouted();
    let issues = app.live_issues();
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert_eq!(issues[0].title, "Main is routed nowhere");
    assert_eq!(issues[0].severity, IssueSeverity::Warning);
    app.set_path_mute(main, true);
    assert!(app.live_issues().is_empty(), "muted on purpose");
}

#[test]
fn the_only_path_on_a_muted_device_warns() {
    let mut app = blank_app();
    app.clock_device.mute = true;
    let issues = app.live_issues();
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert_eq!(issues[0].title, "Main is routed nowhere audible");
}

#[test]
fn the_dsp_alarm_has_hysteresis() {
    let quiet = DspAlarm::default();
    assert_eq!(quiet.next(0.69, 0.9).severity(), None);
    let warned = quiet.next(0.70, 0.9);
    assert_eq!(warned.severity(), Some(IssueSeverity::Warning));
    assert_eq!(warned.next(0.61, 0.9), warned, "held above the clear line");
    assert_eq!(warned.next(0.59, 0.9).severity(), None);
    let errored = warned.next(0.65, 0.95);
    assert_eq!(errored.severity(), Some(IssueSeverity::Error));
    assert!(errored.next(0.65, 0.86).error, "held above 85%");
    let cleared = errored.next(0.65, 0.84);
    assert_eq!(cleared.severity(), Some(IssueSeverity::Warning));
}

#[test]
fn underruns_warn_and_clear_after_a_quiet_spell() {
    let start = Instant::now();
    let mut watch = UnderrunWatch::default();
    watch.update(0, start);
    assert!(watch.episode().is_none());
    watch.update(1, start + Duration::from_secs(1));
    let episode = *watch.episode().expect("one underrun raises it");
    assert_eq!(episode.count, 1);
    assert!(!episode.burst);
    watch.update(1, start + Duration::from_secs(4));
    assert_eq!(watch.episode().unwrap().since_last, Duration::from_secs(3));
    watch.update(1, start + Duration::from_secs(1) + UNDERRUN_QUIET);
    assert!(watch.episode().is_none(), "quiet long enough");
    assert_eq!(watch.session_count(), 1, "the session count stays");
}

#[test]
fn an_underrun_burst_is_an_error_until_it_goes_quiet() {
    let start = Instant::now();
    let mut watch = UnderrunWatch::default();
    watch.update(1, start);
    watch.update(2, start + UNDERRUN_BURST_WINDOW + Duration::from_secs(1));
    watch.update(3, start + UNDERRUN_BURST_WINDOW + Duration::from_secs(2));
    assert!(
        !watch.episode().unwrap().burst,
        "three, but spread wider than the window"
    );
    let late = start + UNDERRUN_BURST_WINDOW + Duration::from_secs(3);
    watch.update(4, late);
    let episode = watch.episode().unwrap();
    assert!(episode.burst, "the last three within the window");
    assert_eq!(episode.count, 4);
    watch.update(4, late + Duration::from_secs(5));
    assert!(watch.episode().unwrap().burst, "held while the run lasts");
    watch.update(4, late + UNDERRUN_QUIET);
    assert!(watch.episode().is_none());
    watch.update(5, late + UNDERRUN_QUIET + Duration::from_secs(1));
    assert!(!watch.episode().unwrap().burst, "a new run starts calm");
}

#[test]
fn a_lower_underrun_count_is_a_new_session() {
    let now = Instant::now();
    let mut watch = UnderrunWatch::default();
    watch.update(5, now);
    assert!(watch.episode().unwrap().burst, "five at once");
    watch.update(0, now + Duration::from_millis(20));
    assert!(watch.episode().is_none());
    assert_eq!(watch.session_count(), 0);
}

#[test]
fn underruns_while_running_offer_a_safer_latency() {
    let mut app = blank_app();
    let now = Instant::now();
    app.issue_state.underruns.update(2, now);
    assert!(app.live_issues().is_empty(), "only while the bridge runs");
    app.engine.state = EngineState::Running;
    let issues = app.live_issues();
    assert_eq!(issues.len(), 1, "{issues:?}");
    let issue = &issues[0];
    assert_eq!(issue.severity, IssueSeverity::Warning);
    assert_eq!(issue.title, "The output ran dry");
    assert!(
        issue
            .detail
            .starts_with("2 underruns, the last just now (2 underruns this session)"),
        "{}",
        issue.detail
    );
    app.cushion_mode = CushionMode::LowLatency;
    assert_eq!(app.live_issues()[0].action, IssueAction::SafeCushion);
    app.apply_issue_action(IssueAction::SafeCushion);
    assert_eq!(app.cushion_mode, CushionMode::Safe);
    let action = app.live_issues()[0].action;
    if cfg!(target_os = "windows") {
        assert_eq!(action, IssueAction::SharedOutput);
    } else {
        assert_eq!(action, IssueAction::LowLatencyOff);
        assert_eq!(action.label(), "Turn Low latency off");
        app.apply_issue_action(action);
        assert!(!app.low_latency_output);
        assert!(app
            .log
            .last()
            .unwrap()
            .text
            .starts_with("Low-latency output off"));
        assert_eq!(
            app.live_issues()[0].action,
            IssueAction::SetOutputBuffer(256)
        );
    }
    app.issue_state
        .underruns
        .update(4, now + Duration::from_millis(500));
    assert_eq!(app.live_issues()[0].severity, IssueSeverity::Error);
    assert_eq!(
        app.live_issues()[0].title,
        "Audio is dropping out on the output"
    );
    app.issue_state
        .underruns
        .update(4, now + Duration::from_millis(500) + UNDERRUN_QUIET);
    assert!(app.live_issues().is_empty(), "cleared after a quiet spell");
    app.engine.state = EngineState::Idle;
}

#[cfg(target_os = "linux")]
#[test]
fn a_period_that_does_not_fit_the_graph_block_sets_the_buffer() {
    let mut app = blank_app();
    app.low_latency_output = false;
    app.output_buffer = 128;
    let issue = app
        .quantum_info_issue(&crate::audio::QuantumInfo::assess(true, 64, 48_000, 128))
        .expect("128 frames do not fit a 64-frame block");
    assert_eq!(issue.severity, IssueSeverity::Warning);
    assert_eq!(issue.title, "Graph block and output period do not line up");
    assert_eq!(issue.action, IssueAction::SetOutputBuffer(64));
    assert_eq!(
        issue.action_name(),
        "Set buffer to 64: Graph block and output period do not line up"
    );
    app.apply_issue_action(issue.action);
    assert_eq!(app.output_buffer, 64);
    assert_eq!(
        app.log.last().unwrap().text,
        "Output buffer set to 64 samples."
    );
    assert!(app
        .quantum_info_issue(&crate::audio::QuantumInfo::assess(true, 64, 48_000, 64))
        .is_none());
    // Low latency picks the driver's period: the buffer would not help.
    app.low_latency_output = true;
    let issue = app
        .quantum_info_issue(&crate::audio::QuantumInfo::assess(true, 64, 48_000, 96))
        .unwrap();
    assert_eq!(issue.action, IssueAction::LowLatencyOff);
}

#[cfg(target_os = "linux")]
#[test]
fn a_large_graph_block_matters_only_under_low_latency_while_running() {
    let mut app = blank_app();
    let idle = crate::audio::QuantumInfo::assess(true, 1024, 48_000, 64);
    assert!(app.quantum_info_issue(&idle).is_none(), "not running yet");
    app.engine.state = EngineState::Running;
    let issue = app.quantum_info_issue(&idle).expect("low latency is on");
    assert_eq!(issue.title, "Low latency is on, but the graph block is not");
    assert_eq!(issue.action, IssueAction::OpenDiagnostics);
    app.low_latency_output = false;
    assert!(
        app.quantum_info_issue(&idle).is_none(),
        "the person chose it"
    );
    app.engine.state = EngineState::Idle;
}

#[test]
fn a_failed_quantum_change_is_one_row() {
    let mut app = blank_app();
    app.engine
        .note_quantum_failure("PipeWire quantum not set: test".to_owned());
    let issues = app.live_issues();
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert_eq!(issues[0].title, "PipeWire did not take the quantum change");
    assert_eq!(issues[0].detail, "PipeWire quantum not set: test");
    assert_eq!(issues[0].place, "OUTPUT · Low latency");
}

#[test]
fn an_unchecked_low_latency_driver_warns_until_it_has_a_reading() {
    let mut app = blank_app();
    assert!(
        app.live_issues().is_empty(),
        "the demo output has no driver"
    );
    with_real_output(&mut app);
    let issues = app.live_issues();
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert_eq!(issues[0].title, "Output driver not checked yet");
    assert_eq!(issues[0].place, "OUTPUT · Driver");
    assert_eq!(issues[0].action, IssueAction::CheckDriver);
    assert_eq!(issues[0].action.label(), "Check driver");

    let (sender, receiver) = mpsc::channel();
    app.driver_probe_job = Some(receiver);
    assert!(app.live_issues().is_empty(), "a check is running");
    app.driver_probe_job = None;
    drop(sender);

    app.driver_probe = Some(DriverCheck {
        device_id: "test:out".to_owned(),
        name: "Scarlett 2i2".to_owned(),
        result: Ok(DriverActual {
            rate: 48_000,
            frames: 64,
            notes: Vec::new(),
            tone: DriverTone::Good,
        }),
        manual: true,
    });
    assert!(app.live_issues().is_empty(), "a reading answers it");
    app.driver_probe = None;
    app.low_latency_output = false;
    app.wasapi_output_mode = crate::audio::WasapiOutputMode::Shared;
    assert!(
        app.live_issues().is_empty(),
        "without a low-latency mode the step matters less"
    );
}

#[test]
fn reading_the_issues_does_not_move_the_dsp_latches() {
    let mut app = blank_app();
    app.issue_state.dsp = DspAlarm {
        warning: true,
        error: false,
    };
    for _ in 0..3 {
        let issues = app.live_issues();
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].title, "The chain overloads the output block");
        assert_eq!(issues[0].severity, IssueSeverity::Warning);
    }
    assert!(app.issue_state.dsp.warning, "only a tick moves the latch");
    app.tick_issues(Instant::now());
    assert_eq!(
        app.issue_state.dsp,
        DspAlarm::default(),
        "no mixer runs: the tick reads no load and clears it"
    );
    assert!(app.live_issues().is_empty());
}

#[test]
fn a_bridge_start_or_stop_clears_the_dsp_latches_and_underruns() {
    let mut app = blank_app();
    app.issue_state.dsp = DspAlarm {
        warning: true,
        error: true,
    };
    app.issue_state.underruns.update(3, Instant::now());
    app.stop_bridge();
    assert_eq!(app.issue_state.dsp, DspAlarm::default());
    assert_eq!(app.issue_state.underruns.session_count(), 0);
}

#[test]
fn a_new_error_expands_a_collapsed_strip_and_a_warning_does_not() {
    let mut app = blank_app();
    let added = app.add_path().expect("a free slot");
    app.set_path_mute(added, false);
    app.live_issues_collapsed = true;
    app.expand_on_new_error();
    assert!(app.live_issues_collapsed, "a warning leaves it collapsed");

    app.issue_state.dsp = DspAlarm {
        warning: true,
        error: true,
    };
    app.expand_on_new_error();
    assert!(!app.live_issues_collapsed, "a new error opens it");
    app.live_issues_collapsed = true;
    app.expand_on_new_error();
    assert!(
        app.live_issues_collapsed,
        "the same error, collapsed on purpose, stays collapsed"
    );

    app.issue_state.dsp.error = false;
    app.expand_on_new_error();
    assert!(app.live_issues_collapsed);
    app.issue_state.dsp.error = true;
    app.expand_on_new_error();
    assert!(!app.live_issues_collapsed, "it came back: new again");
}

#[test]
fn a_lost_device_leaves_an_error_row_once_the_alert_is_dismissed() {
    let mut app = blank_app();
    app.issue_state.dsp = DspAlarm {
        warning: true,
        error: false,
    };
    app.live_issues_collapsed = true;
    app.alert_lost_device("Scarlett 2i2 was unplugged, so the bridge was stopped.".to_owned());
    assert_eq!(
        app.issue_state.dsp,
        DspAlarm::default(),
        "the session ended"
    );
    assert!(
        app.live_issues().is_empty(),
        "the alert window is the hard stop; no second copy behind it"
    );
    app.expand_on_new_error();
    assert!(app.live_issues_collapsed);

    app.device_alert = None;
    let issues = app.live_issues();
    assert_eq!(issues.len(), 1, "{issues:?}");
    let issue = &issues[0];
    assert_eq!(issue.severity, IssueSeverity::Error);
    assert_eq!(issue.title, "The bridge stopped: a device was disconnected");
    assert_eq!(
        issue.detail,
        "Scarlett 2i2 was unplugged, so the bridge was stopped."
    );
    assert_eq!(issue.action, IssueAction::OpenDevices);
    assert_eq!(issue.action.label(), "Open Devices");
    app.expand_on_new_error();
    assert!(!app.live_issues_collapsed, "the strip opens on it");
    app.apply_issue_action(issue.action);
    assert!(app.page == Page::Devices);
    assert!(app.live_issues().len() == 1, "a click changes nothing else");

    app.start_bridge();
    assert!(
        app.issue_state.device_lost.is_none(),
        "the next bridge start clears it"
    );
}

/// An app with one warning: an unmuted second path routed nowhere.
fn with_unrouted_path() -> (RigWerkApp, usize) {
    let mut app = blank_app();
    let added = app.add_path().expect("a free slot");
    app.set_path_mute(added, false);
    (app, added)
}

#[test]
fn issues_carry_a_stable_key_and_their_card() {
    let (mut app, added) = with_unrouted_path();
    let issue = &app.live_issues()[0];
    assert_eq!(issue.key, IssueKey::RoutedNowhere(added));
    assert_eq!(issue.card, None, "routing rows point to another page");
    assert_eq!(issue.show_label(), None);

    app.engine
        .note_quantum_failure("PipeWire quantum not set: test".to_owned());
    let quantum = app
        .live_issues()
        .into_iter()
        .find(|issue| issue.key == IssueKey::QuantumNotTaken)
        .expect("the quantum row");
    assert_eq!(quantum.card, Some(IssueCard::Output));
    assert_eq!(quantum.show_label().as_deref(), Some("Show on Output"));
    assert_eq!(
        quantum.show_name().as_deref(),
        Some("Show on Output: PipeWire did not take the quantum change")
    );

    app.issue_state.underruns.update(1, Instant::now());
    app.engine.state = EngineState::Running;
    let first = app
        .live_issues()
        .into_iter()
        .find(|issue| issue.key == IssueKey::Underruns)
        .expect("the underrun row");
    app.issue_state
        .underruns
        .update(2, Instant::now() + Duration::from_secs(3));
    let later = app
        .live_issues()
        .into_iter()
        .find(|issue| issue.key == IssueKey::Underruns)
        .expect("the underrun row");
    assert_ne!(first.detail, later.detail, "the wording moves");
    assert_eq!(first.key, later.key, "the key does not");
}

#[test]
fn a_filter_lists_its_rows_and_opens_the_strip() {
    let (mut app, _) = with_unrouted_path();
    app.issue_state.dsp = DspAlarm {
        warning: true,
        error: true,
    };
    app.refresh_issues();
    let view = app.issue_view();
    assert_eq!(view.count(IssueFilter::All), 2);
    assert_eq!(view.count(IssueFilter::Warnings), 1);
    assert_eq!(view.count(IssueFilter::Errors), 1);
    app.live_issues_collapsed = true;
    app.set_issue_filter(IssueFilter::Errors);
    assert!(
        !app.live_issues_collapsed,
        "choosing a filter opens the strip"
    );
    let rows: Vec<_> = app
        .issue_view()
        .filtered(IssueFilter::Errors)
        .cloned()
        .collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key, IssueKey::Dsp);
    assert_eq!(IssueFilter::Errors.empty_note(), "No errors");
}

#[test]
fn the_filter_falls_back_to_all_when_nothing_is_left_or_an_error_is_new() {
    let (mut app, added) = with_unrouted_path();
    let now = Instant::now();
    app.set_issue_filter(IssueFilter::Errors);
    app.tick_issues(now);
    assert_eq!(
        app.issue_state.filter,
        IssueFilter::Errors,
        "a warning is still there; the filter stays"
    );
    app.set_path_mute(added, true);
    app.tick_issues(now);
    assert_eq!(app.issue_state.filter, IssueFilter::All, "nothing left");

    app.set_path_mute(added, false);
    app.set_issue_filter(IssueFilter::Warnings);
    app.live_issues_collapsed = true;
    app.issue_state.dsp.error = true;
    app.expand_on_new_error();
    assert!(!app.live_issues_collapsed);
    assert_eq!(
        app.issue_state.filter,
        IssueFilter::All,
        "the new error is in view"
    );
}

#[test]
fn a_dismissed_warning_stays_hidden_until_it_clears_and_comes_back() {
    let (mut app, added) = with_unrouted_path();
    let now = Instant::now();
    let key = IssueKey::RoutedNowhere(added);
    app.refresh_issues();
    app.dismiss_issue(key);
    let view = app.issue_view();
    assert!(view.visible.is_empty());
    assert_eq!(view.hidden, 1);
    assert!(!view.is_empty(), "the header stays, to show it again");
    app.tick_issues(now);
    assert_eq!(app.issue_view().hidden, 1, "still there: still dismissed");

    app.set_path_mute(added, true);
    app.tick_issues(now);
    app.set_path_mute(added, false);
    app.refresh_issues();
    let view = app.issue_view();
    assert_eq!(view.visible.len(), 1, "it cleared and came back");
    assert_eq!(view.hidden, 0);

    app.dismiss_issue(key);
    app.restore_dismissed_issues();
    assert_eq!(app.issue_view().visible.len(), 1, "shown again on request");

    app.dismiss_issue(key);
    app.live_issues_collapsed = true;
    app.restore_dismissed_issues();
    assert!(!app.live_issues_collapsed, "shown in an open strip");

    app.dismiss_issue(key);
    app.stop_bridge();
    app.refresh_issues();
    assert_eq!(
        app.issue_view().visible.len(),
        1,
        "a new bridge session shows it again"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn dismissing_one_quantum_problem_does_not_hide_another() {
    let mut app = blank_app();
    app.engine.state = EngineState::Running;
    app.output_buffer = 128;
    app.quantum = Some(crate::audio::QuantumInfo::assess(true, 64, 48_000, 128));
    let issues = app.live_issues();
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert_eq!(issues[0].key, IssueKey::QuantumMisfit);
    app.refresh_issues();
    app.dismiss_issue(IssueKey::QuantumMisfit);
    assert_eq!(app.issue_view().hidden, 1);

    // The period fits now, but the graph block is not the low one.
    app.quantum = Some(crate::audio::QuantumInfo::assess(true, 1024, 48_000, 64));
    app.refresh_issues();
    let view = app.issue_view();
    assert_eq!(view.hidden, 0, "a different problem: {view:?}");
    assert_eq!(view.visible[0].key, IssueKey::QuantumNotLow);
    app.engine.state = EngineState::Idle;
}

#[test]
fn an_error_is_not_dismissed_and_a_dismissed_warning_that_turns_error_shows() {
    let mut app = blank_app();
    let now = Instant::now();
    app.issue_state.dsp = DspAlarm {
        warning: true,
        error: false,
    };
    app.refresh_issues();
    app.dismiss_issue(IssueKey::Dsp);
    assert_eq!(app.issue_view().hidden, 1);

    app.issue_state.dsp.error = true;
    let issues = app.live_issues();
    assert_eq!(issues[0].severity, IssueSeverity::Error);
    assert!(!issues[0].dismissable());
    app.refresh_issues();
    let view = app.issue_view();
    assert_eq!(view.visible.len(), 1, "an error always shows");
    assert_eq!(view.hidden, 0);
    app.issue_state.settle(&issues, now);
    app.dismiss_issue(IssueKey::Dsp);
    app.issue_state.dsp.error = false;
    app.refresh_issues();
    assert_eq!(
        app.issue_view().visible.len(),
        1,
        "the error ended the dismissal, and it was not dismissed itself"
    );
}

#[test]
fn show_on_a_card_rings_it_for_a_moment() {
    let mut app = blank_app();
    let now = Instant::now();
    assert_eq!(app.card_highlight(IssueCard::Output, now), None);
    app.show_issue_card(IssueCard::Output, IssueSeverity::Warning, now);
    assert_eq!(
        app.card_highlight(IssueCard::Output, now),
        Some((IssueSeverity::Warning, 1.0))
    );
    assert_eq!(app.card_highlight(IssueCard::Bridge, now), None);
    let half = app
        .card_highlight(IssueCard::Output, now + CARD_HIGHLIGHT_FOR / 2)
        .expect("still ringing")
        .1;
    assert!(half > 0.0 && half < 1.0, "{half}");
    assert_eq!(
        app.card_highlight(IssueCard::Output, now + CARD_HIGHLIGHT_FOR),
        None
    );
    assert_eq!(highlight_strength(CARD_HIGHLIGHT_FOR * 2), 0.0);
    app.tick_issues(now + CARD_HIGHLIGHT_FOR);
    assert!(app.issue_state.highlight.is_none(), "the tick forgets it");
}

/// A running bridge with one generic follower, "USB Headphones", in the
/// device list and on a Low latency cushion.
fn with_follower() -> RigWerkApp {
    let mut app = blank_app();
    app.inventory.outputs.push(DeviceItem {
        id: "test:usb".to_owned(),
        name: "USB Headphones".to_owned(),
        backend: "ALSA".to_owned(),
        channels: 2,
        demo: false,
        preferred: false,
        asio_clsid: None,
    });
    app.followers.push(FollowerPref {
        id: "test:usb".to_owned(),
        backend: "ALSA".to_owned(),
        name: "USB Headphones".to_owned(),
        cushion_mode: cushion_to_pref(CushionMode::LowLatency),
        ..FollowerPref::default()
    });
    app.engine.state = EngineState::Running;
    app
}

#[test]
fn a_follower_being_retried_is_a_warning_that_shows_it_in_the_mixer() {
    let mut app = with_follower();
    assert!(app.live_issues().is_empty(), "{:?}", app.live_issues());
    app.snapshot.follower_missing[0] = true;
    let issues = app.live_issues();
    assert_eq!(issues.len(), 1, "{issues:?}");
    let issue = &issues[0];
    assert_eq!(issue.key, IssueKey::FollowerRetrying(0));
    assert_eq!(issue.severity, IssueSeverity::Warning, "the clock plays on");
    assert_eq!(issue.title, "USB Headphones is not playing");
    assert!(issue.detail.contains("retrying"), "{}", issue.detail);
    assert_eq!(issue.place, "OUTPUT · USB Headphones");
    assert_eq!(issue.card, Some(IssueCard::Output));
    assert_eq!(issue.action, IssueAction::ShowFollower(0));
    assert_eq!(issue.action.label(), "Show in Mixer");
    app.apply_issue_action(issue.action);
    assert!(app.page == Page::Mixer);
    assert_eq!(app.chain_selected, ChainSelection::OutputDevice(1));
    app.refresh_issues();
    app.dismiss_issue(IssueKey::FollowerRetrying(0));
    assert_eq!(app.issue_view().hidden, 1);

    app.snapshot.follower_missing[0] = false;
    app.inventory.outputs.clear();
    let issues = app.live_issues();
    assert_eq!(issues.len(), 1, "gone from the device list: {issues:?}");
    assert_eq!(issues[0].key, IssueKey::FollowerNotListed(0));
    app.refresh_issues();
    assert_eq!(app.issue_view().hidden, 0, "a different problem");
    assert!(
        !issues[0].detail.contains("retrying"),
        "{}",
        issues[0].detail
    );

    app.engine.state = EngineState::Idle;
    assert!(app.live_issues().is_empty(), "only while the bridge runs");
}

#[test]
fn follower_underruns_offer_its_own_cushion_then_the_buffer() {
    let mut app = with_follower();
    let now = Instant::now();
    app.snapshot.follower_underruns[0] = 2;
    app.tick_issues(now);
    let issues = app.live_issues();
    assert_eq!(issues.len(), 1, "the clock output is fine: {issues:?}");
    let issue = &issues[0];
    assert_eq!(issue.key, IssueKey::FollowerUnderruns(0));
    assert_eq!(issue.severity, IssueSeverity::Warning);
    assert_eq!(issue.title, "USB Headphones ran dry");
    assert!(
        issue
            .detail
            .starts_with("2 underruns, the last just now (2 underruns since it opened)"),
        "{}",
        issue.detail
    );
    assert_eq!(issue.place, "OUTPUT · USB Headphones");
    assert_eq!(issue.action, IssueAction::FollowerSafeCushion(0));
    assert_eq!(issue.action.label(), "Use Safe cushion");

    // The clock's own cushion is not the one that helps here.
    app.cushion_mode = CushionMode::LowLatency;
    app.apply_issue_action(IssueAction::FollowerSafeCushion(0));
    assert_eq!(
        app.followers[0].cushion_mode,
        cushion_to_pref(CushionMode::Safe)
    );
    assert_eq!(app.cushion_mode, CushionMode::LowLatency);
    assert_eq!(
        app.log.last().unwrap().text,
        "USB Headphones: cushion Safe."
    );

    app.low_latency_output = true;
    let action = app.live_issues()[0].action;
    assert_eq!(
        action,
        IssueAction::SetOutputBuffer(256),
        "never the clock device's Low latency"
    );
    assert!(app.live_issues()[0].detail.ends_with(
        "Followers ask for the output buffer: 256 samples give USB Headphones more room."
    ));

    app.snapshot.follower_underruns[0] = 4;
    app.tick_issues(now + Duration::from_millis(500));
    let issue = &app.live_issues()[0];
    assert_eq!(issue.severity, IssueSeverity::Error);
    assert_eq!(issue.title, "Audio is dropping out on USB Headphones");

    app.tick_issues(now + Duration::from_millis(500) + UNDERRUN_QUIET);
    assert!(app.live_issues().is_empty(), "cleared after a quiet spell");

    app.snapshot.follower_underruns[0] = 5;
    app.tick_issues(now + UNDERRUN_QUIET * 2);
    assert_eq!(app.live_issues().len(), 1);
    app.stop_bridge();
    assert_eq!(
        app.issue_state.follower_underruns[0].session_count(),
        0,
        "per session"
    );
}

#[test]
fn a_missing_follower_has_no_underrun_or_drift_row() {
    let mut app = with_follower();
    app.snapshot.follower_underruns[0] = 1;
    app.snapshot.follower_drift_at_limit[0] = true;
    let now = Instant::now();
    app.tick_issues(now);
    app.tick_issues(now + DRIFT_RAISE_AFTER);
    app.snapshot.follower_missing[0] = true;
    let keys: Vec<IssueKey> = app.live_issues().iter().map(|issue| issue.key).collect();
    assert_eq!(keys, [IssueKey::FollowerRetrying(0)]);
}

#[test]
fn a_pinned_follower_drift_loop_is_a_warning() {
    let mut app = with_follower();
    app.snapshot.follower_drift_at_limit[0] = true;
    let now = Instant::now();
    app.tick_issues(now);
    assert!(app.live_issues().is_empty(), "not pinned long enough yet");
    app.tick_issues(now + DRIFT_RAISE_AFTER);
    let issues = app.live_issues();
    assert_eq!(issues.len(), 1, "{issues:?}");
    let issue = &issues[0];
    assert_eq!(issue.key, IssueKey::FollowerDrift(0));
    assert_eq!(issue.severity, IssueSeverity::Warning);
    assert_eq!(issue.title, "USB Headphones drifts too far to follow");
    assert!(issue.detail.contains("500 ppm limit"), "{}", issue.detail);
    assert_eq!(issue.place, "OUTPUT · USB Headphones");
    assert_eq!(issue.action, IssueAction::OpenDiagnostics);
}

#[test]
fn a_follower_drift_warning_is_held_against_flicker() {
    let mut app = with_follower();
    let start = Instant::now();
    let at = |ms: u64| start + Duration::from_millis(ms);
    // Hovering at the limit: pinned, free, pinned, never a full second.
    for (ms, pinned) in [(0, true), (600, false), (700, true), (1500, false)] {
        app.snapshot.follower_drift_at_limit[0] = pinned;
        app.tick_issues(at(ms));
        assert!(app.live_issues().is_empty(), "{ms} ms");
    }
    app.snapshot.follower_drift_at_limit[0] = true;
    app.tick_issues(at(2000));
    app.tick_issues(at(3000));
    assert_eq!(app.live_issues()[0].key, IssueKey::FollowerDrift(0));
    // Free for a moment: the warning holds.
    app.snapshot.follower_drift_at_limit[0] = false;
    app.tick_issues(at(3100));
    app.snapshot.follower_drift_at_limit[0] = true;
    app.tick_issues(at(4000));
    app.snapshot.follower_drift_at_limit[0] = false;
    app.tick_issues(at(4100));
    app.tick_issues(at(4100) + DRIFT_CLEAR_AFTER - Duration::from_millis(1));
    assert_eq!(app.live_issues().len(), 1, "still held");
    app.tick_issues(at(4100) + DRIFT_CLEAR_AFTER);
    assert!(app.live_issues().is_empty(), "clear for the full hold");

    app.snapshot.follower_drift_at_limit[0] = true;
    app.tick_issues(at(9000));
    app.tick_issues(at(10000));
    assert_eq!(app.live_issues().len(), 1);
    app.stop_bridge();
    assert!(!app.issue_state.follower_drift[0].raised(), "per session");
}

#[test]
fn a_slot_that_holds_another_follower_starts_over() {
    let mut app = with_follower();
    let now = Instant::now();
    app.tick_issues(now);
    app.snapshot.follower_underruns[0] = 2;
    app.snapshot.follower_drift_at_limit[0] = true;
    app.tick_issues(now + Duration::from_millis(100));
    app.tick_issues(now + DRIFT_RAISE_AFTER * 2);
    let keys: Vec<IssueKey> = app.live_issues().iter().map(|issue| issue.key).collect();
    assert_eq!(
        keys,
        [IssueKey::FollowerUnderruns(0), IssueKey::FollowerDrift(0)]
    );
    app.dismiss_issue(IssueKey::FollowerUnderruns(0));
    assert_eq!(app.issue_view().hidden, 1);

    // Replaced live by another device; its stream has not opened yet, so
    // the slot still reads the old one's count and drift.
    app.inventory.outputs.push(DeviceItem {
        id: "test:dock".to_owned(),
        name: "Dock".to_owned(),
        backend: "ALSA".to_owned(),
        channels: 2,
        demo: false,
        preferred: false,
        asio_clsid: None,
    });
    app.followers[0].id = "test:dock".to_owned();
    app.followers[0].name = "Dock".to_owned();
    app.snapshot.follower_drift_at_limit[0] = false;
    let later = now + DRIFT_RAISE_AFTER * 3;
    app.tick_issues(later);
    assert!(app.live_issues().is_empty(), "{:?}", app.live_issues());
    assert_eq!(
        app.issue_view().hidden,
        0,
        "the dismissal was the old one's"
    );

    // Its stream opens (the count starts over), then it runs dry itself.
    app.snapshot.follower_underruns[0] = 0;
    app.tick_issues(later + Duration::from_millis(100));
    app.snapshot.follower_underruns[0] = 1;
    app.tick_issues(later + Duration::from_millis(200));
    let view = app.issue_view();
    assert_eq!(view.hidden, 0);
    assert_eq!(view.visible[0].key, IssueKey::FollowerUnderruns(0));
    assert_eq!(view.visible[0].title, "Dock ran dry");
    assert!(
        view.visible[0]
            .detail
            .starts_with("1 underrun, the last just now (1 underrun since it opened)"),
        "{}",
        view.visible[0].detail
    );
}

#[test]
fn a_held_flag_waits_both_ways() {
    let mut flag = HeldFlag::default();
    let start = Instant::now();
    let second = Duration::from_secs(1);
    let three = Duration::from_secs(3);
    assert!(!flag.update(true, start, second, three));
    assert!(!flag.update(false, start + second / 2, second, three));
    assert!(!flag.update(true, start + second, second, three));
    assert!(flag.update(true, start + second * 2, second, three));
    assert!(flag.update(false, start + second * 3, second, three));
    assert!(flag.update(false, start + second * 5, second, three));
    assert!(!flag.update(false, start + second * 6, second, three));
}

/// Ticks every 50 ms for `span` from `from`, returning the time after.
#[cfg(target_os = "linux")]
fn tick_for(app: &mut RigWerkApp, from: Instant, span: Duration) -> Instant {
    let mut now = from;
    while now < from + span {
        now += Duration::from_millis(50);
        app.tick_issues(now);
    }
    now
}

#[cfg(target_os = "linux")]
#[test]
fn a_silent_sink_under_low_latency_is_an_error_only_with_underruns() {
    let mut app = blank_app();
    app.engine.state = EngineState::Running;
    app.low_latency_output = true;
    app.snapshot.output_channels = 2;
    app.snapshot.out_channels[1] = 0.1;
    let start = Instant::now();
    app.tick_issues(start);
    let now = tick_for(&mut app, start, Duration::from_secs(4));
    assert!(
        app.live_issues().is_empty(),
        "no monitor data: never a guess"
    );

    // Without underruns the RigWerk stream may just be muted in the
    // desktop mixer: a warning that says so, and the strip stays shut.
    app.engine.set_test_sink_peak(Some(0.0));
    app.live_issues_collapsed = true;
    let now = tick_for(&mut app, now, SILENCE_RAISE_ALONE);
    let issues = app.live_issues();
    assert_eq!(issues.len(), 1, "{issues:?}");
    let issue = &issues[0];
    assert_eq!(issue.key, IssueKey::SinkSilent);
    assert_eq!(issue.severity, IssueSeverity::Warning);
    assert_eq!(issue.title, "Output may be silent under low latency");
    assert!(issue.detail.contains("desktop mixer"), "{}", issue.detail);
    assert_eq!(issue.card, Some(IssueCard::Output));
    assert_eq!(issue.action, IssueAction::LowLatencyOff);
    assert!(
        app.live_issues_collapsed,
        "a warning does not open the strip"
    );

    app.mute = true;
    app.tick_issues(now + Duration::from_millis(50));
    assert!(app.live_issues().is_empty(), "muted on purpose");
    app.mute = false;

    // A rest: neither side is loud, so nothing is judged.
    app.snapshot.out_channels[1] = 0.0;
    let now = tick_for(&mut app, now, Duration::from_secs(5));
    assert!(app.live_issues().is_empty());

    // With underruns the shorter hold applies, and it is an error.
    app.snapshot.out_channels[1] = 0.1;
    app.snapshot.underruns = 1;
    let now = tick_for(&mut app, now, SILENCE_RAISE_WITH_UNDERRUNS);
    let issue = app
        .live_issues()
        .into_iter()
        .find(|issue| issue.key == IssueKey::SinkSilent)
        .expect("the silence row");
    assert_eq!(issue.severity, IssueSeverity::Error);
    assert_eq!(issue.title, "Output went silent under low latency");
    assert!(issue.detail.contains("ran dry"), "{}", issue.detail);
    assert!(!app.live_issues_collapsed, "a new error opens the strip");

    app.engine.set_test_sink_peak(Some(0.05));
    tick_for(&mut app, now, Duration::from_secs(1));
    let keys: Vec<IssueKey> = app.live_issues().iter().map(|issue| issue.key).collect();
    assert!(!keys.contains(&IssueKey::SinkSilent), "heard again");

    app.low_latency_output = false;
    app.engine.set_test_sink_peak(Some(0.0));
    tick_for(&mut app, now, Duration::from_secs(5));
    let keys: Vec<IssueKey> = app.live_issues().iter().map(|issue| issue.key).collect();
    assert!(
        !keys.contains(&IssueKey::SinkSilent),
        "only armed under Low latency"
    );
    app.engine.state = EngineState::Idle;
}

#[test]
fn a_device_against_the_ceiling_warns_until_the_signal_drops() {
    let mut app = blank_app();
    with_real_output(&mut app);
    app.engine.state = EngineState::Running;
    app.snapshot.out_reduction_db = 1.0;
    app.inventory.outputs.push(DeviceItem {
        id: "test:hp".to_owned(),
        name: "Headphones".to_owned(),
        backend: "ALSA".to_owned(),
        channels: 2,
        demo: false,
        preferred: false,
        asio_clsid: None,
    });
    let headphones = FollowerPref {
        id: "test:hp".to_owned(),
        name: "Headphones".to_owned(),
        ..FollowerPref::default()
    };
    let mut hidden = FollowerPref::virtual_out();
    hidden.publish = false;
    hidden.name = "RigWerk Out".to_owned();
    app.followers.push(headphones);
    app.followers.push(hidden);
    app.snapshot.follower_reduction_db[0] = 3.2;
    app.snapshot.follower_reduction_db[1] = 9.0;

    let now = Instant::now();
    app.tick_issues(now);
    let issue = app
        .issue_view()
        .visible
        .into_iter()
        .find(|issue| issue.key == IssueKey::SafetyCeiling)
        .expect("the ceiling row");
    assert_eq!(issue.title, "Headphones is against the safety ceiling");
    assert_eq!(issue.detail, "−1.0 dB · 3.2 dB reduction.");
    assert_eq!(issue.card, Some(IssueCard::Output));
    assert_eq!(issue.action, IssueAction::ShowOutput);
    assert!(!issue.dismissable());
    app.dismiss_issue(IssueKey::SafetyCeiling);
    assert!(
        !app.issue_state.dismissed.contains(&IssueKey::SafetyCeiling),
        "a held ceiling cannot be dismissed"
    );
    app.tick_issues(now);
    assert!(app
        .issue_view()
        .visible
        .iter()
        .any(|issue| issue.key == IssueKey::SafetyCeiling));
    app.apply_issue_action(IssueAction::ShowOutput);
    assert!(app
        .card_highlight(IssueCard::Output, Instant::now())
        .is_some());

    app.snapshot.out_reduction_db = 0.0;
    app.snapshot.follower_reduction_db = [0.0; crate::audio::MAX_FOLLOWERS];
    app.tick_issues(now);
    assert!(app
        .live_issues()
        .iter()
        .all(|issue| issue.key != IssueKey::SafetyCeiling));

    app.snapshot.out_reduction_db = 3.2;
    app.engine.state = EngineState::Idle;
    app.tick_issues(now);
    assert!(
        app.live_issues()
            .iter()
            .all(|issue| issue.key != IssueKey::SafetyCeiling),
        "a stop clears a stale reduction"
    );
}
