use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::{
    DspAlarm, IssueAction, IssueSeverity, UnderrunWatch, UNDERRUN_BURST_WINDOW, UNDERRUN_QUIET,
};
use crate::app::cards::Card;
use crate::app::driver_check::{DriverActual, DriverCheck, DriverTone};
use crate::app::{blank_app, Page, RigWerkApp};
use crate::audio::{ChannelMap, CushionMode, DeviceItem, EngineState, PathMapSettings, MAX_ALIGN};

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
