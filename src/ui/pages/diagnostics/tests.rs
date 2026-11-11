use std::path::PathBuf;

use crate::app::blank_app;
use crate::audio::claim_main_thread;
use crate::ui::theme::MUTED;

#[test]
fn the_report_names_the_chain_and_insert_counts() {
    let app = blank_app();
    let path = app.engine.shared.path(0);
    path.insert_delay_for_test(16);
    let mut left = vec![0.0f32; 64];
    let mut right = vec![0.0f32; 64];
    assert!(path.process_plugins(&mut left, &mut right, 64));

    let text = app.diagnostics_text();
    assert!(
        text.contains("- Main: chain: 0 skipped of 1 chunk (0%)"),
        "{text}"
    );
    assert!(text.contains("  - Delay: 0 of 1 chunk (0%)"), "{text}");
    assert!(
        text.contains("Chain counts are chunks the path tried to process."),
        "{text}"
    );
    path.restore_plugins(Vec::new());
}

#[test]
fn a_chunk_count_is_pluralised() {
    assert_eq!(super::chunk_text(0), "0 chunks");
    assert_eq!(super::chunk_text(1), "1 chunk");
    assert_eq!(super::chunk_text(2), "2 chunks");
    assert_eq!(super::chunk_text(1_000), "1000 chunks");
}

#[test]
fn a_skip_that_rounds_to_zero_reads_below_one_percent() {
    let stats = |attempts, skips| crate::audio::LockStats { attempts, skips };
    assert_eq!(super::percent_text(&stats(1_000, 0)), "0%");
    assert_eq!(super::percent_text(&stats(1_000, 1)), "<1%");
    assert_eq!(super::percent_text(&stats(2, 1)), "50%");
    assert_eq!(
        super::percent_text(&crate::audio::LockStats::default()),
        "0%"
    );
}

#[test]
fn a_chain_that_skipped_every_chunk_says_no_insert_ran() {
    use crate::audio::{LockStats, PluginLockReport};
    let skipped = PluginLockReport {
        chain: LockStats {
            attempts: 4,
            skips: 4,
        },
        inserts: vec![LockStats::default()],
        chain_poisoned: false,
    };
    assert!(super::no_insert_ran(&skipped));
    let ran = PluginLockReport {
        inserts: vec![LockStats {
            attempts: 2,
            skips: 0,
        }],
        ..skipped.clone()
    };
    assert!(!super::no_insert_ran(&ran));
    let poisoned = PluginLockReport {
        chain_poisoned: true,
        ..skipped
    };
    assert!(!super::no_insert_ran(&poisoned));
}

#[test]
fn a_poisoned_chain_is_named_instead_of_a_skip_rate() {
    // The claim lets the chain's slots be destroyed here; one left for the
    // deferred queue can flake the parallel suite.
    let _main = claim_main_thread();
    let app = blank_app();
    let path = app.engine.shared.path(0);
    path.insert_delay_for_test(16);
    path.poison_chain_for_test();
    let mut left = vec![0.0f32; 64];
    let mut right = vec![0.0f32; 64];
    assert!(!path.process_plugins(&mut left, &mut right, 64));

    let text = app.diagnostics_text();
    assert!(
        text.contains("- Main: chain unavailable after an internal error"),
        "{text}"
    );
    assert!(!text.contains("chain: 0 skipped of 1 chunk"), "{text}");
    assert!(!text.contains(super::NO_INSERT_TRIED), "{text}");

    // Reset clears the attempts but not the poison, so the section still
    // has to show the dead chain instead of "No plugin blocks yet.".
    path.reset_plugin_lock_counters();
    let text = app.diagnostics_text();
    assert!(
        text.contains("- Main: chain unavailable after an internal error"),
        "{text}"
    );
    assert!(!text.contains("No plugin blocks yet."), "{text}");
}

#[test]
fn a_zero_latency_edit_panic_is_named_in_diagnostics() {
    // The claim lets the chain's slots be destroyed here; one left for the
    // deferred queue can flake the parallel suite.
    let _main = claim_main_thread();
    let app = blank_app();
    let path = app.engine.shared.path(0);
    path.insert_delay_for_test(0);
    path.poison_chain_in_edit_for_test();

    let text = app.diagnostics_text();
    assert!(
        text.contains("chain unavailable after an internal error"),
        "{text}"
    );
    assert!(!text.contains(super::NO_INSERT_TRIED), "{text}");
}

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
    assert!(text.contains("Plugin locks"), "{text}");
    assert!(text.contains("No plugin blocks yet."), "{text}");
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

#[test]
fn the_report_keeps_the_estimate_and_the_measurement_apart() {
    let mut app = blank_app();
    let conditions = app.latency_conditions();
    app.measured_latency = Some(crate::app::latency::MeasuredLatency {
        latency_ms: 23.4,
        samples: 1_123,
        input_id: conditions.input_id.clone(),
        output_id: conditions.output_id.clone(),
        input_rate: conditions.input_rate,
        output_rate: conditions.output_rate,
        input_buffer: conditions.input_buffer,
        output_step: conditions.output_step,
        output_mode: conditions.output_mode.clone(),
        measured_at: 1_700_000_000,
        confidence: 0.9,
        ..Default::default()
    });

    let text = app.diagnostics_text();
    assert!(text.contains("Latency budget:"), "{text}");
    assert!(text.contains("Estimated path:"), "{text}");
    assert!(text.contains("Measured loopback: 23.4 ms"), "{text}");
    assert!(text.contains("OUT → cable → IN"), "{text}");
    assert!(text.contains("2023-11-14 22:13 UTC"), "{text}");
    // The estimate and the measurement cover different legs; they are never
    // subtracted from each other.
    assert!(!text.contains("minus estimate"), "{text}");
    assert!(text.contains("cover different legs"), "{text}");
    assert!(!text.contains("stale:"), "{text}");

    // A measurement from another device pair is reported as stale, with the
    // reason, instead of being reused.
    app.measured_latency.as_mut().expect("stored").input_id = "hw:Elsewhere".to_owned();
    let text = app.diagnostics_text();
    assert!(text.contains("stale: the input changed to"), "{text}");
}
