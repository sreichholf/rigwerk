use super::AudioEngine;
use crate::audio::{AsioPanelSide, EngineState};

#[test]
fn the_panel_stays_unavailable_until_the_bridge_is_running() {
    let mut engine = AudioEngine::new(48_000);
    engine.asio_input_clsid = Some(0xA510);
    engine.asio_output_clsid = Some(0xB0);
    assert_eq!(engine.open_asio_driver(AsioPanelSide::Input), None);
    engine.state = EngineState::Starting;
    assert_eq!(engine.open_asio_driver(AsioPanelSide::Input), None);
    engine.state = EngineState::Running;
    assert_eq!(engine.open_asio_driver(AsioPanelSide::Input), Some(0xA510));
    assert_eq!(engine.open_asio_driver(AsioPanelSide::Output), Some(0xB0));
    engine.stop();
    engine.state = EngineState::Running;
    assert_eq!(
        engine.open_asio_driver(AsioPanelSide::Input),
        None,
        "stopping forgets which driver was open"
    );
}

#[test]
fn the_bridge_status_is_part_of_the_snapshot() {
    let engine = AudioEngine::new(48_000);
    engine.shared.set_status("Opening independent output…");
    assert_eq!(engine.bridge_status(), "Opening independent output…");
    assert_eq!(engine.snapshot().status, "Opening independent output…");
}

#[test]
fn a_quantum_failure_survives_bridge_stopped() {
    let mut engine = AudioEngine::new(48_000);
    engine.note_quantum_failure(
        "PipeWire quantum was not restored: pw-metadata not available".to_owned(),
    );
    engine.stop();
    assert_eq!(
        engine.quantum_note(),
        Some("PipeWire quantum was not restored: pw-metadata not available")
    );
    assert_eq!(engine.bridge_status(), "Bridge stopped");
}

#[cfg(target_os = "linux")]
#[test]
fn a_missing_metadata_tool_is_kept_when_the_quantum_is_restored() {
    let tool_missing = std::process::Command::new("pw-metadata")
        .arg("-n")
        .arg("settings")
        .output()
        .is_err();
    let mut engine = AudioEngine::new(48_000);
    engine.quantum_forced = true;
    engine.previous_quantum = 1024;
    engine.stop();
    assert_eq!(engine.bridge_status(), "Bridge stopped");
    if tool_missing {
        let note = engine
            .quantum_note()
            .expect("a missing pw-metadata must be reported");
        assert!(
            note.starts_with("PipeWire quantum was not restored:"),
            "{note}"
        );
    }
}
