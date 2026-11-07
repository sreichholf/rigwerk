use super::{DspAlarm, IssueAction, IssueSeverity};
use crate::app::blank_app;
use crate::app::cards::Card;
use crate::app::Page;
use crate::audio::{ChannelMap, PathMapSettings, MAX_ALIGN};

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
