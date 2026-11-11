use super::LatencySource;
use crate::app::blank_app;
use crate::app::latency::{LoopbackKind, MeasuredLatency};
use crate::audio::EngineState;

fn measurement() -> MeasuredLatency {
    MeasuredLatency {
        latency_ms: 21.5,
        samples: 1_032,
        input_id: "asio:focusrite".to_owned(),
        input_name: "Focusrite USB".to_owned(),
        output_id: "wasapi:{speakers}".to_owned(),
        output_name: "Speakers".to_owned(),
        input_rate: 48_000,
        output_rate: 48_000,
        input_buffer: 128,
        output_step: 128,
        output_mode: "real-style".to_owned(),
        measured_at: 1_700_000_000,
        confidence: 0.94,
        loopback: LoopbackKind::Cable,
    }
}

#[test]
fn the_budget_adds_the_reported_parts_and_labels_their_origin() {
    let mut app = blank_app();
    app.engine.state = EngineState::Running;
    app.buffer_size = 128;
    app.sample_rate = 48_000;
    app.output_buffer = 128;
    app.output_rate = 48_000;
    app.snapshot.input_block_frames = 128;
    app.snapshot.input_rate = 48_000;
    app.snapshot.output_rate = 48_000;
    app.snapshot.output_step_frames = 128;
    app.snapshot.target_frames = 256;
    app.snapshot.path_delay_ms = Some(10.0);
    app.engine.screenshot_path_latency(0, 480);

    let budget = app.latency_budget();
    // 128 + 256 frames at 48 kHz (8.0 ms), 480 samples of chain (10.0 ms),
    // 128 frames out (2.7 ms), 10.0 ms after the node.
    assert!(
        (budget.estimate_ms - 30.666).abs() < 0.01,
        "estimate {}",
        budget.estimate_ms
    );
    let labels: Vec<&str> = budget.parts.iter().map(|part| part.label).collect();
    assert_eq!(
        labels,
        vec![
            "INPUT BLOCK",
            "BRIDGE CUSHION",
            "PLUGIN CHAIN",
            "OUTPUT STEP",
            "AFTER PLAYBACK NODE"
        ]
    );
    assert_eq!(
        budget
            .parts
            .iter()
            .map(|part| part.source)
            .collect::<Vec<_>>(),
        vec![
            LatencySource::Reported,
            LatencySource::Setting,
            LatencySource::Reported,
            LatencySource::Reported,
            LatencySource::Reported
        ]
    );
    assert_eq!(budget.measured, None);
    let text = budget.text_lines().join("\n");
    assert!(text.contains("Estimated path: 30.7 ms (estimated;"));
    assert!(text.contains("Measured loopback: not measured yet"));
    assert!(text.contains("cover different legs"), "{text}");
}

#[test]
fn a_stopped_budget_labels_planned_buffers_and_leaves_out_the_unknown_part() {
    let app = blank_app();
    let budget = app.latency_budget();
    let source = |label: &str| {
        budget
            .parts
            .iter()
            .find(|part| part.label == label)
            .unwrap_or_else(|| panic!("missing {label}"))
            .source
    };
    assert_eq!(source("INPUT BLOCK"), LatencySource::Planned);
    assert_eq!(source("OUTPUT STEP"), LatencySource::Planned);
    assert_eq!(source("BRIDGE CUSHION"), LatencySource::Setting);
    assert!(budget
        .parts
        .iter()
        .any(|part| part.label == "AFTER PLAYBACK NODE" && part.ms.is_none()));
    let text = budget.text_lines().join("\n");
    assert!(
        text.contains("AFTER PLAYBACK NODE unknown, left out of the sum"),
        "{text}"
    );
    assert!(text.contains("(planned)"), "{text}");
}

#[test]
fn the_budget_compares_a_fresh_measurement_with_the_estimate() {
    let mut app = blank_app();
    app.engine.state = EngineState::Running;
    app.sample_rate = 48_000;
    app.buffer_size = 128;
    app.output_buffer = 128;
    app.snapshot.input_block_frames = 128;
    app.snapshot.input_rate = 48_000;
    app.snapshot.output_rate = 48_000;
    app.snapshot.output_step_frames = 128;
    app.snapshot.target_frames = 256;
    let now = app.latency_conditions();
    app.measured_latency = Some(MeasuredLatency {
        latency_ms: 40.0,
        samples: 1_920,
        input_id: now.input_id,
        output_id: now.output_id,
        input_rate: now.input_rate,
        output_rate: now.output_rate,
        input_buffer: now.input_buffer,
        output_step: now.output_step,
        output_mode: now.output_mode,
        ..measurement()
    });

    let budget = app.latency_budget();
    assert_eq!(budget.stale, None);
    let text = budget.text_lines().join("\n");
    assert!(text.contains("Measured loopback: 40.0 ms"));
    assert!(text.contains("OUT Speakers → cable → IN Focusrite USB"));
    assert!(!text.contains("minus estimate"), "{text}");
}

#[test]
fn an_unplugged_device_makes_the_old_measurement_stale() {
    let mut app = blank_app();
    app.measured_latency = Some(MeasuredLatency {
        input_id: "asio:focusrite".to_owned(),
        ..measurement()
    });
    app.engine.state = EngineState::Running;
    app.snapshot.input_rate = 48_000;
    app.snapshot.output_rate = 48_000;
    app.snapshot.input_block_frames = 128;
    app.snapshot.output_step_frames = 128;
    // The blank app's active input is a placeholder, not the measured pair.
    let budget = app.latency_budget();
    assert!(budget.stale.is_some());
    assert!(budget.measured.is_some());
}
