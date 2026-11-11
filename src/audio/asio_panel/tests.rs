use super::{AsioPanelRequests, AsioPanelSide};
use crate::audio::AudioEngine;

#[test]
fn a_ui_request_is_taken_once_by_the_bridge() {
    let engine = AudioEngine::new(48_000);
    assert!(!engine.shared.asio_panel.take(AsioPanelSide::Input));
    engine.request_asio_panel(AsioPanelSide::Input);
    assert!(engine.shared.asio_panel.take(AsioPanelSide::Input));
    assert!(!engine.shared.asio_panel.take(AsioPanelSide::Input));
}

#[test]
fn each_side_keeps_its_own_request() {
    let engine = AudioEngine::new(48_000);
    engine.request_asio_panel(AsioPanelSide::Input);
    engine.request_asio_panel(AsioPanelSide::Output);
    assert!(engine.shared.asio_panel.take(AsioPanelSide::Output));
    assert!(
        !engine.shared.asio_panel.take(AsioPanelSide::Output),
        "taking the output request clears only that side"
    );
    assert!(engine.shared.asio_panel.take(AsioPanelSide::Input));
}

#[test]
fn a_repeated_request_is_still_one_open() {
    let requests = AsioPanelRequests::new();
    requests.request(AsioPanelSide::Input);
    requests.request(AsioPanelSide::Input);
    assert!(requests.take(AsioPanelSide::Input));
    assert!(!requests.take(AsioPanelSide::Input));
}

#[test]
fn clearing_a_request_before_the_bridge_takes_it_drops_the_open() {
    let requests = AsioPanelRequests::new();
    requests.request(AsioPanelSide::Output);
    requests.clear_requests();
    assert!(!requests.take(AsioPanelSide::Output));
}

#[test]
fn the_open_mark_stays_until_every_panel_call_returns() {
    let requests = AsioPanelRequests::new();
    assert!(!requests.is_open());
    let outer = requests.enter();
    let inner = requests.enter();
    assert!(requests.is_open());
    drop(inner);
    assert!(requests.is_open());
    drop(outer);
    assert!(!requests.is_open());
}
