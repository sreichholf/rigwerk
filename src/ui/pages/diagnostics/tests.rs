use std::path::PathBuf;

use crate::app::blank_app;
use crate::ui::theme::MUTED;

#[test]
fn the_report_includes_status_notes_and_the_log() {
    let mut app = blank_app();
    app.inventory
        .notes
        .push("ASIO registry scan: boom".to_owned());
    app.push_log("Bridge ready.", MUTED);
    app.engine.note_quantum_failure(
        "PipeWire quantum was not restored: pw-metadata not available".to_owned(),
    );
    app.engine.shared.set_status("Bridge stopped");

    let text = app.diagnostics_text();
    assert!(text.contains("Bridge status: Bridge stopped"), "{text}");
    assert!(
        text.contains("PipeWire quantum was not restored: pw-metadata not available"),
        "{text}"
    );
    assert!(text.contains("ASIO registry scan: boom"), "{text}");
    assert!(text.contains("Bridge ready."), "{text}");
    assert!(text.contains("cap 400"), "{text}");
    assert!(text.contains("- Main: 0.0 ms · not running"), "{text}");
    assert_eq!(
        super::dsp_text(Some(crate::audio::DspTime {
            avg_ns: 210_000,
            peak_ns: 428_000,
            block_ns: 10_700_000,
        })),
        "0.21 ms avg · 0.43 ms peak of 10.7 ms · 2% / 4%"
    );

    let path: PathBuf = std::env::temp_dir().join(format!(
        "rigwerk-diagnostics-test-{}-report.txt",
        std::process::id()
    ));
    app.write_diagnostics(&path).expect("export");
    let written = std::fs::read_to_string(&path).expect("read export");
    assert_eq!(written, text);
    let _ = std::fs::remove_file(&path);
}
