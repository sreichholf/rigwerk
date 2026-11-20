use std::sync::mpsc;
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use super::silence::{SILENCE_RAISE_ALONE, SILENCE_RAISE_WITH_DROPOUTS};
use super::{
    Anchor, DropoutWatch, DspAlarm, HeldFlag, IssueAction, IssueFilter, IssueKey, IssueSeverity,
    DRIFT_CLEAR_AFTER, DRIFT_RAISE_AFTER, DROPOUT_BURST_WINDOW, DROPOUT_QUIET,
};
use crate::app::driver_check::{DriverActual, DriverCheck, DriverTone};
use crate::app::layouts::PathLayout;
use crate::app::settings::{bridge_buffer_to_pref, ExtraOutputPref};
use crate::app::{blank_app, Page, RigWerkApp};
use crate::audio::{BufferMode, ChannelMap, DeviceItem, EngineState, PathMapSettings, MAX_ALIGN};

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
    assert!(app.apply_layout(PathLayout::Blend));
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
    assert!(app.apply_layout(PathLayout::Blend));
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
    app.clock_output.path_map[added].map = ChannelMap::default();
    assert!(app.live_issues().is_empty());
    app.clock_output.mute = true;
    let issues = app.live_issues();
    assert!(issues
        .iter()
        .any(|issue| issue.title == "Path 2 is routed nowhere audible"));
    app.apply_issue_action(issues[0].action);
    assert_eq!(
        app.chrome.sheet,
        Some(crate::app::screen::Sheet::Output { output: 0 })
    );
}

#[test]
fn the_only_path_routed_nowhere_warns() {
    let mut app = blank_app();
    let main = app.path_slots()[0];
    app.clock_output.path_map[main] = PathMapSettings::unrouted();
    let issues = app.live_issues();
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert_eq!(issues[0].title, "Main is routed nowhere");
    assert_eq!(issues[0].severity, IssueSeverity::Warning);
    app.set_path_mute(main, true);
    assert!(app.live_issues().is_empty(), "muted on purpose");
}

#[test]
fn a_path_heard_only_through_a_return_does_not_warn() {
    let mut app = blank_app();
    let main = app.path_slots()[0];
    app.clock_output.path_map[main] = PathMapSettings::unrouted();
    let fx = app.add_return().expect("a free slot");
    app.set_path_mute(fx, false);
    app.clock_output.path_map[fx] = PathMapSettings::default();
    // No send yet: Main is still routed nowhere.
    let titles: Vec<String> = app.live_issues().into_iter().map(|i| i.title).collect();
    assert_eq!(titles, vec!["Main is routed nowhere".to_owned()]);
    app.set_path_send(main, fx, -6.0);
    assert!(app.live_issues().is_empty(), "{:?}", app.live_issues());
    // A muted return carries nothing.
    app.set_path_mute(fx, true);
    let titles: Vec<String> = app.live_issues().into_iter().map(|i| i.title).collect();
    assert_eq!(titles, vec!["Main is routed nowhere".to_owned()]);
}

#[test]
fn the_only_path_on_a_muted_device_warns() {
    let mut app = blank_app();
    app.clock_output.mute = true;
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
fn dropouts_warn_and_clear_after_a_quiet_spell() {
    let start = Instant::now();
    let mut watch = DropoutWatch::default();
    watch.update(0, start);
    assert!(watch.episode().is_none());
    watch.update(1, start + Duration::from_secs(1));
    let episode = *watch.episode().expect("one dropout raises it");
    assert_eq!(episode.count, 1);
    assert!(!episode.burst);
    watch.update(1, start + Duration::from_secs(4));
    assert_eq!(watch.episode().unwrap().since_last, Duration::from_secs(3));
    watch.update(1, start + Duration::from_secs(1) + DROPOUT_QUIET);
    assert!(watch.episode().is_none(), "quiet long enough");
    assert_eq!(watch.run_count(), 1, "the run count stays");
}

#[test]
fn a_dropout_burst_is_an_error_until_it_goes_quiet() {
    let start = Instant::now();
    let mut watch = DropoutWatch::default();
    watch.update(1, start);
    watch.update(2, start + DROPOUT_BURST_WINDOW + Duration::from_secs(1));
    watch.update(3, start + DROPOUT_BURST_WINDOW + Duration::from_secs(2));
    assert!(
        !watch.episode().unwrap().burst,
        "three, but spread wider than the window"
    );
    let late = start + DROPOUT_BURST_WINDOW + Duration::from_secs(3);
    watch.update(4, late);
    let episode = watch.episode().unwrap();
    assert!(episode.burst, "the last three within the window");
    assert_eq!(episode.count, 4);
    watch.update(4, late + Duration::from_secs(5));
    assert!(watch.episode().unwrap().burst, "held while the run lasts");
    watch.update(4, late + DROPOUT_QUIET);
    assert!(watch.episode().is_none());
    watch.update(5, late + DROPOUT_QUIET + Duration::from_secs(1));
    assert!(!watch.episode().unwrap().burst, "a new run starts calm");
}

#[test]
fn a_lower_dropout_count_is_a_new_run() {
    let now = Instant::now();
    let mut watch = DropoutWatch::default();
    watch.update(5, now);
    assert!(watch.episode().unwrap().burst, "five at once");
    watch.update(0, now + Duration::from_millis(20));
    assert!(watch.episode().is_none());
    assert_eq!(watch.run_count(), 0);
}

#[test]
fn dropouts_while_running_offer_a_safer_latency() {
    let mut app = blank_app();
    let now = Instant::now();
    app.issue_state.dropouts.update(2, now);
    assert!(app.live_issues().is_empty(), "only while the bridge runs");
    app.engine.state = EngineState::Running;
    let issues = app.live_issues();
    assert_eq!(issues.len(), 1, "{issues:?}");
    let issue = &issues[0];
    assert_eq!(issue.severity, IssueSeverity::Warning);
    assert_eq!(issue.title, "The output had a dropout");
    assert!(
        issue
            .detail
            .starts_with("2 dropouts, the last just now (2 dropouts this run)"),
        "{}",
        issue.detail
    );
    app.bridge_buffer_mode = BufferMode::Minimum;
    assert_eq!(app.live_issues()[0].action, IssueAction::AutoBridgeBuffer);
    app.apply_issue_action(IssueAction::AutoBridgeBuffer);
    assert_eq!(app.bridge_buffer_mode, BufferMode::Auto);
    let action = app.live_issues()[0].action;
    if cfg!(target_os = "windows") {
        assert_eq!(action, IssueAction::SharedOutput);
    } else {
        assert_eq!(action, IssueAction::LowLatencyOff);
        assert_eq!(action.label(), "Turn Low latency off");
        app.apply_issue_action(action);
        assert!(!app.low_latency_output);
        assert!(app.log.last().unwrap().text.starts_with("Low latency off"));
        assert_eq!(
            app.live_issues()[0].action,
            IssueAction::SetOutputBuffer(256)
        );
    }
    app.issue_state
        .dropouts
        .update(4, now + Duration::from_millis(500));
    assert_eq!(app.live_issues()[0].severity, IssueSeverity::Error);
    assert_eq!(app.live_issues()[0].title, "Dropouts on the output");
    app.issue_state
        .dropouts
        .update(4, now + Duration::from_millis(500) + DROPOUT_QUIET);
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
        .expect("128 samples do not fit a 64-sample buffer");
    assert_eq!(issue.severity, IssueSeverity::Warning);
    assert_eq!(issue.title, "The buffer sizes do not line up");
    assert_eq!(
        issue.anchor,
        Anchor::Setup(crate::app::screen::SetupSection::Clock)
    );
    assert_eq!(issue.action, IssueAction::SetOutputBuffer(64));
    assert_eq!(
        issue.action_name(),
        "Set buffer to 64: The buffer sizes do not line up"
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
    assert_eq!(
        issue.title,
        "Low latency is on, but the graph buffer is not"
    );
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
    assert_eq!(issues[0].title, "PipeWire did not take the buffer change");
    assert_eq!(issues[0].detail, "PipeWire quantum not set: test");
    assert_eq!(issues[0].place, "Setup");
    assert_eq!(
        issues[0].anchor,
        Anchor::Setup(crate::app::screen::SetupSection::Clock)
    );
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
    assert_eq!(issues[0].place, "Setup");
    assert_eq!(
        issues[0].anchor,
        Anchor::Setup(crate::app::screen::SetupSection::Devices)
    );
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
        assert_eq!(issues[0].title, "The chain overloads the output buffer");
        assert_eq!(issues[0].anchor, Anchor::Path(app.path_slots()[0]));
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
fn a_bridge_start_or_stop_clears_the_dsp_latches_and_dropouts() {
    let mut app = blank_app();
    app.issue_state.dsp = DspAlarm {
        warning: true,
        error: true,
    };
    app.issue_state.dropouts.update(3, Instant::now());
    app.stop_bridge();
    assert_eq!(app.issue_state.dsp, DspAlarm::default());
    assert_eq!(app.issue_state.dropouts.run_count(), 0);
}

#[test]
fn a_new_error_sets_the_banner_and_a_warning_does_not() {
    let mut app = blank_app();
    let added = app.add_path().expect("a free slot");
    app.set_path_mute(added, false);
    app.expand_on_new_error();
    assert!(
        app.chrome.error_title.is_none(),
        "a warning leaves the banner"
    );

    app.issue_state.dsp = DspAlarm {
        warning: true,
        error: true,
    };
    app.expand_on_new_error();
    assert_eq!(
        app.chrome.error_title.as_deref(),
        Some("Paths overload the output buffer")
    );
    app.chrome.error_dismissed = true;
    app.expand_on_new_error();
    assert!(
        app.chrome.error_dismissed,
        "the same error does not raise the banner again"
    );

    app.issue_state.dsp.error = false;
    app.expand_on_new_error();
    assert!(app.chrome.error_title.is_none());
    app.issue_state.dsp.error = true;
    app.expand_on_new_error();
    assert!(app.chrome.error_title.is_some(), "it came back: new again");
}

#[test]
fn a_lost_device_leaves_an_error_row_once_the_alert_is_dismissed() {
    let mut app = blank_app();
    app.issue_state.dsp = DspAlarm {
        warning: true,
        error: false,
    };
    app.alert_lost_device("Scarlett 2i2 was unplugged, so the bridge was stopped.".to_owned());
    assert_eq!(app.issue_state.dsp, DspAlarm::default(), "the run ended");
    assert!(
        app.live_issues().is_empty(),
        "the alert window is the hard stop; no second copy behind it"
    );
    app.expand_on_new_error();
    assert!(app.chrome.error_title.is_none());

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
    assert_eq!(issue.action.label(), "Open Setup");
    assert_eq!(issue.place, "Setup");
    assert_eq!(
        issue.anchor,
        Anchor::Setup(crate::app::screen::SetupSection::Devices)
    );
    app.expand_on_new_error();
    assert_eq!(
        app.chrome.error_title.as_deref(),
        Some("The bridge stopped: a device was disconnected")
    );
    app.apply_issue_action(issue.action);
    assert!(app.page == Page::Setup);
    assert_eq!(
        app.chrome.setup_section,
        Some(crate::app::screen::SetupSection::Devices)
    );
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
fn issues_carry_a_stable_key_and_their_anchor() {
    let (mut app, added) = with_unrouted_path();
    let issue = &app.live_issues()[0];
    assert_eq!(issue.key, IssueKey::RoutedNowhere(added));
    assert_eq!(issue.anchor, Anchor::Path(added));
    assert_eq!(issue.place, "Path 2");
    assert_eq!(issue.show_label(), "Show");
    assert_eq!(issue.show_name(), "Show: Path 2 is routed nowhere");

    app.engine
        .note_quantum_failure("PipeWire quantum not set: test".to_owned());
    let quantum = app
        .live_issues()
        .into_iter()
        .find(|issue| issue.key == IssueKey::QuantumNotTaken)
        .expect("the buffer-change row");
    assert_eq!(
        quantum.anchor,
        Anchor::Setup(crate::app::screen::SetupSection::Clock)
    );
    assert_eq!(quantum.show_label(), "Show");
    assert_eq!(
        quantum.show_name(),
        "Show: PipeWire did not take the buffer change"
    );

    app.issue_state.dropouts.update(1, Instant::now());
    app.engine.state = EngineState::Running;
    let first = app
        .live_issues()
        .into_iter()
        .find(|issue| issue.key == IssueKey::Dropouts)
        .expect("the dropout row");
    app.issue_state
        .dropouts
        .update(2, Instant::now() + Duration::from_secs(3));
    let later = app
        .live_issues()
        .into_iter()
        .find(|issue| issue.key == IssueKey::Dropouts)
        .expect("the dropout row");
    assert_ne!(first.detail, later.detail, "the wording moves");
    assert_eq!(first.key, later.key, "the key does not");
}

#[test]
fn a_filter_lists_its_rows() {
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
    app.set_issue_filter(IssueFilter::Errors);
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
    app.issue_state.dsp.error = true;
    app.expand_on_new_error();
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
    app.stop_bridge();
    app.refresh_issues();
    assert_eq!(
        app.issue_view().visible.len(),
        1,
        "a new bridge run shows it again"
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
    app.issue_state.settle(&issues);
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
fn show_selects_the_anchor_and_closes_the_drawer() {
    let mut app = blank_app();
    app.chrome.drawer = Some(crate::app::screen::Drawer::Issues);
    app.show_issue(Anchor::Input);
    assert_eq!(app.page, Page::Console);
    assert!(app.chrome.sheet.is_none());
    assert!(app.chrome.drawer.is_none());

    app.show_issue(Anchor::Master);
    assert_eq!(app.page, Page::Console);
    assert!(app.chrome.sheet.is_none());

    let slot = app.path_slots()[0];
    app.show_issue(Anchor::Path(slot));
    assert_eq!(app.page, Page::Console);
    assert_eq!(app.selected_path, slot);
    assert_eq!(
        app.chrome.sheet,
        Some(crate::app::screen::Sheet::Path { path: slot })
    );
    assert_eq!(app.chrome.reveal_path, Some(slot));

    app.show_issue(Anchor::Stage {
        path: slot,
        stage: crate::app::chain::ChainStage::Gate,
    });
    assert_eq!(
        app.chrome.sheet,
        Some(crate::app::screen::Sheet::Gate { path: slot })
    );
    assert!(app.chrome.follow_stage);
    assert_eq!(app.chrome.reveal_path, Some(slot));
    assert_eq!(app.selected_path, slot);

    app.show_issue(Anchor::Output(1));
    assert_eq!(
        app.chrome.sheet,
        Some(crate::app::screen::Sheet::Output { output: 1 })
    );
    assert_eq!(app.page, Page::Console);

    app.chrome.drawer = Some(crate::app::screen::Drawer::Issues);
    app.show_issue(Anchor::Setup(crate::app::screen::SetupSection::Clock));
    assert_eq!(app.page, Page::Setup);
    assert_eq!(
        app.chrome.setup_section,
        Some(crate::app::screen::SetupSection::Clock)
    );
    assert!(app.chrome.drawer.is_none());
}

/// A running bridge with one generic extra output, "USB Headphones", in the
/// device list and on a Minimum bridge buffer.
fn with_extra_output() -> RigWerkApp {
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
    app.extra_outputs.push(ExtraOutputPref {
        id: "test:usb".to_owned(),
        backend: "ALSA".to_owned(),
        name: "USB Headphones".to_owned(),
        bridge_buffer_mode: bridge_buffer_to_pref(BufferMode::Minimum),
        ..ExtraOutputPref::default()
    });
    app.engine.state = EngineState::Running;
    app
}

#[test]
fn a_extra_output_being_retried_is_a_warning_that_shows_it_in_the_mixer() {
    let mut app = with_extra_output();
    assert!(app.live_issues().is_empty(), "{:?}", app.live_issues());
    app.snapshot.extra_output_missing[0] = true;
    let issues = app.live_issues();
    assert_eq!(issues.len(), 1, "{issues:?}");
    let issue = &issues[0];
    assert_eq!(issue.key, IssueKey::ExtraOutputRetrying(0));
    assert_eq!(issue.severity, IssueSeverity::Warning, "the clock plays on");
    assert_eq!(issue.title, "USB Headphones is not playing");
    assert!(issue.detail.contains("retrying"), "{}", issue.detail);
    assert_eq!(issue.place, "USB Headphones");
    assert_eq!(issue.anchor, Anchor::Output(1));
    assert_eq!(issue.action, IssueAction::ShowExtraOutput(0));
    assert_eq!(issue.action.label(), "Show output");
    app.apply_issue_action(issue.action);
    assert_eq!(
        app.chrome.sheet,
        Some(crate::app::screen::Sheet::Output { output: 1 })
    );
    app.refresh_issues();
    app.dismiss_issue(IssueKey::ExtraOutputRetrying(0));
    assert_eq!(app.issue_view().hidden, 1);

    app.snapshot.extra_output_missing[0] = false;
    app.inventory.outputs.clear();
    let issues = app.live_issues();
    assert_eq!(issues.len(), 1, "gone from the device list: {issues:?}");
    assert_eq!(issues[0].key, IssueKey::ExtraOutputNotListed(0));
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
fn extra_output_dropouts_offer_its_own_bridge_buffer_then_the_buffer() {
    let mut app = with_extra_output();
    let now = Instant::now();
    app.snapshot.extra_output_dropouts[0] = 2;
    app.tick_issues(now);
    let issues = app.live_issues();
    assert_eq!(issues.len(), 1, "the clock output is fine: {issues:?}");
    let issue = &issues[0];
    assert_eq!(issue.key, IssueKey::ExtraOutputDropouts(0));
    assert_eq!(issue.severity, IssueSeverity::Warning);
    assert_eq!(issue.title, "USB Headphones had a dropout");
    assert!(
        issue
            .detail
            .starts_with("2 dropouts, the last just now (2 dropouts since it opened)"),
        "{}",
        issue.detail
    );
    assert_eq!(issue.place, "USB Headphones");
    assert_eq!(issue.anchor, Anchor::Output(1));
    assert_eq!(issue.action, IssueAction::ExtraOutputAutoBridgeBuffer(0));
    assert_eq!(issue.action.label(), "Use Auto bridge buffer");

    // The clock's own bridge buffer is not the one that helps here.
    app.bridge_buffer_mode = BufferMode::Minimum;
    app.apply_issue_action(IssueAction::ExtraOutputAutoBridgeBuffer(0));
    assert_eq!(
        app.extra_outputs[0].bridge_buffer_mode,
        bridge_buffer_to_pref(BufferMode::Auto)
    );
    assert_eq!(app.bridge_buffer_mode, BufferMode::Minimum);
    assert_eq!(
        app.log.last().unwrap().text,
        "USB Headphones: Bridge buffer: Auto."
    );

    app.low_latency_output = true;
    let action = app.live_issues()[0].action;
    assert_eq!(
        action,
        IssueAction::SetOutputBuffer(256),
        "never the clock output's Low latency"
    );
    assert!(app.live_issues()[0].detail.ends_with(
        "Extra outputs ask for the output buffer: 256 samples give USB Headphones more room."
    ));

    app.snapshot.extra_output_dropouts[0] = 4;
    app.tick_issues(now + Duration::from_millis(500));
    let issue = &app.live_issues()[0];
    assert_eq!(issue.severity, IssueSeverity::Error);
    assert_eq!(issue.title, "Dropouts on USB Headphones");

    app.tick_issues(now + Duration::from_millis(500) + DROPOUT_QUIET);
    assert!(app.live_issues().is_empty(), "cleared after a quiet spell");

    app.snapshot.extra_output_dropouts[0] = 5;
    app.tick_issues(now + DROPOUT_QUIET * 2);
    assert_eq!(app.live_issues().len(), 1);
    app.stop_bridge();
    assert_eq!(
        app.issue_state.extra_output_dropouts[0].run_count(),
        0,
        "per session"
    );
}

#[test]
fn a_missing_extra_output_has_no_dropout_or_drift_row() {
    let mut app = with_extra_output();
    app.snapshot.extra_output_dropouts[0] = 1;
    app.snapshot.extra_output_drift_at_limit[0] = true;
    let now = Instant::now();
    app.tick_issues(now);
    app.tick_issues(now + DRIFT_RAISE_AFTER);
    app.snapshot.extra_output_missing[0] = true;
    let keys: Vec<IssueKey> = app.live_issues().iter().map(|issue| issue.key).collect();
    assert_eq!(keys, [IssueKey::ExtraOutputRetrying(0)]);
}

#[test]
fn a_pinned_extra_output_drift_loop_is_a_warning() {
    let mut app = with_extra_output();
    app.snapshot.extra_output_drift_at_limit[0] = true;
    let now = Instant::now();
    app.tick_issues(now);
    assert!(app.live_issues().is_empty(), "not pinned long enough yet");
    app.tick_issues(now + DRIFT_RAISE_AFTER);
    let issues = app.live_issues();
    assert_eq!(issues.len(), 1, "{issues:?}");
    let issue = &issues[0];
    assert_eq!(issue.key, IssueKey::ExtraOutputDrift(0));
    assert_eq!(issue.severity, IssueSeverity::Warning);
    assert_eq!(issue.title, "USB Headphones drifts too far to follow");
    assert!(issue.detail.contains("500 ppm limit"), "{}", issue.detail);
    assert_eq!(issue.place, "USB Headphones");
    assert_eq!(issue.anchor, Anchor::Output(1));
    assert_eq!(issue.action, IssueAction::OpenDiagnostics);
}

#[test]
fn a_extra_output_drift_warning_is_held_against_flicker() {
    let mut app = with_extra_output();
    let start = Instant::now();
    let at = |ms: u64| start + Duration::from_millis(ms);
    // Hovering at the limit: pinned, free, pinned, never a full second.
    for (ms, pinned) in [(0, true), (600, false), (700, true), (1500, false)] {
        app.snapshot.extra_output_drift_at_limit[0] = pinned;
        app.tick_issues(at(ms));
        assert!(app.live_issues().is_empty(), "{ms} ms");
    }
    app.snapshot.extra_output_drift_at_limit[0] = true;
    app.tick_issues(at(2000));
    app.tick_issues(at(3000));
    assert_eq!(app.live_issues()[0].key, IssueKey::ExtraOutputDrift(0));
    // Free for a moment: the warning holds.
    app.snapshot.extra_output_drift_at_limit[0] = false;
    app.tick_issues(at(3100));
    app.snapshot.extra_output_drift_at_limit[0] = true;
    app.tick_issues(at(4000));
    app.snapshot.extra_output_drift_at_limit[0] = false;
    app.tick_issues(at(4100));
    app.tick_issues(at(4100) + DRIFT_CLEAR_AFTER - Duration::from_millis(1));
    assert_eq!(app.live_issues().len(), 1, "still held");
    app.tick_issues(at(4100) + DRIFT_CLEAR_AFTER);
    assert!(app.live_issues().is_empty(), "clear for the full hold");

    app.snapshot.extra_output_drift_at_limit[0] = true;
    app.tick_issues(at(9000));
    app.tick_issues(at(10000));
    assert_eq!(app.live_issues().len(), 1);
    app.stop_bridge();
    assert!(
        !app.issue_state.extra_output_drift[0].raised(),
        "per session"
    );
}

#[test]
fn a_slot_that_holds_another_extra_output_starts_over() {
    let mut app = with_extra_output();
    let now = Instant::now();
    app.tick_issues(now);
    app.snapshot.extra_output_dropouts[0] = 2;
    app.snapshot.extra_output_drift_at_limit[0] = true;
    app.tick_issues(now + Duration::from_millis(100));
    app.tick_issues(now + DRIFT_RAISE_AFTER * 2);
    let keys: Vec<IssueKey> = app.live_issues().iter().map(|issue| issue.key).collect();
    assert_eq!(
        keys,
        [
            IssueKey::ExtraOutputDropouts(0),
            IssueKey::ExtraOutputDrift(0)
        ]
    );
    app.dismiss_issue(IssueKey::ExtraOutputDropouts(0));
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
    app.extra_outputs[0].id = "test:dock".to_owned();
    app.extra_outputs[0].name = "Dock".to_owned();
    app.snapshot.extra_output_drift_at_limit[0] = false;
    let later = now + DRIFT_RAISE_AFTER * 3;
    app.tick_issues(later);
    assert!(app.live_issues().is_empty(), "{:?}", app.live_issues());
    assert_eq!(
        app.issue_view().hidden,
        0,
        "the dismissal was the old one's"
    );

    // Its stream opens (the count starts over), then it has a dropout itself.
    app.snapshot.extra_output_dropouts[0] = 0;
    app.tick_issues(later + Duration::from_millis(100));
    app.snapshot.extra_output_dropouts[0] = 1;
    app.tick_issues(later + Duration::from_millis(200));
    let view = app.issue_view();
    assert_eq!(view.hidden, 0);
    assert_eq!(view.visible[0].key, IssueKey::ExtraOutputDropouts(0));
    assert_eq!(view.visible[0].title, "Dock had a dropout");
    assert!(
        view.visible[0]
            .detail
            .starts_with("1 dropout, the last just now (1 dropout since it opened)"),
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
fn a_silent_sink_under_low_latency_is_an_error_only_with_dropouts() {
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

    // Without dropouts the RigWerk stream may just be muted in the
    // desktop mixer: a warning that says so, and the strip stays shut.
    app.engine.set_test_sink_peak(Some(0.0));
    let now = tick_for(&mut app, now, SILENCE_RAISE_ALONE);
    let issues = app.live_issues();
    assert_eq!(issues.len(), 1, "{issues:?}");
    let issue = &issues[0];
    assert_eq!(issue.key, IssueKey::SinkSilent);
    assert_eq!(issue.severity, IssueSeverity::Warning);
    assert_eq!(issue.title, "Output may be silent under low latency");
    assert!(issue.detail.contains("desktop mixer"), "{}", issue.detail);
    assert_eq!(issue.anchor, Anchor::Output(0));
    assert_eq!(issue.place, "Clock output");
    assert_eq!(issue.action, IssueAction::LowLatencyOff);
    assert!(
        app.chrome.error_title.is_none(),
        "a warning does not raise the banner"
    );

    app.mute = true;
    app.tick_issues(now + Duration::from_millis(50));
    assert!(app.live_issues().is_empty(), "muted on purpose");
    app.mute = false;

    // A rest: neither side is loud, so nothing is judged.
    app.snapshot.out_channels[1] = 0.0;
    let now = tick_for(&mut app, now, Duration::from_secs(5));
    assert!(app.live_issues().is_empty());

    // With dropouts the shorter hold applies, and it is an error.
    app.snapshot.out_channels[1] = 0.1;
    app.snapshot.dropouts = 1;
    let now = tick_for(&mut app, now, SILENCE_RAISE_WITH_DROPOUTS);
    let issue = app
        .live_issues()
        .into_iter()
        .find(|issue| issue.key == IssueKey::SinkSilent)
        .expect("the silence row");
    assert_eq!(issue.severity, IssueSeverity::Error);
    assert_eq!(issue.title, "Output went silent under low latency");
    assert!(issue.detail.contains("had a dropout"), "{}", issue.detail);
    assert!(
        app.chrome.error_title.is_some(),
        "a new error raises the banner"
    );

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
    let headphones = ExtraOutputPref {
        id: "test:hp".to_owned(),
        name: "Headphones".to_owned(),
        ..ExtraOutputPref::default()
    };
    let mut hidden = ExtraOutputPref::virtual_output();
    hidden.virtual_output_on = false;
    hidden.name = "RigWerk Out".to_owned();
    app.extra_outputs.push(headphones);
    app.extra_outputs.push(hidden);
    app.snapshot.extra_output_reduction_db[0] = 3.2;
    app.snapshot.extra_output_reduction_db[1] = 9.0;

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
    assert_eq!(issue.anchor, Anchor::Output(1));
    assert_eq!(issue.place, "Headphones");
    assert_eq!(issue.action, IssueAction::ShowOutput(1));
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
    app.apply_issue_action(IssueAction::ShowOutput(1));
    assert_eq!(
        app.chrome.sheet,
        Some(crate::app::screen::Sheet::Output { output: 1 })
    );

    app.snapshot.out_reduction_db = 0.0;
    app.snapshot.extra_output_reduction_db = [0.0; crate::audio::MAX_EXTRA_OUTPUTS];
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
