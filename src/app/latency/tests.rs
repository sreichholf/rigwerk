use super::{civil_from_days, utc_stamp, LatencyConditions, LoopbackKind, MeasuredLatency};

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

fn conditions() -> LatencyConditions {
    LatencyConditions {
        input_id: "asio:focusrite".to_owned(),
        input_name: "Focusrite USB".to_owned(),
        output_id: "wasapi:{speakers}".to_owned(),
        output_name: "Speakers".to_owned(),
        input_rate: 48_000,
        output_rate: 48_000,
        input_buffer: 128,
        output_step: 128,
        output_mode: "real-style".to_owned(),
    }
}

#[test]
fn a_measurement_matches_the_conditions_it_was_made_under() {
    assert_eq!(measurement().stale_reason(&conditions()), None);
}

#[test]
fn every_changed_condition_marks_the_measurement_stale() {
    let cases: [(&str, LatencyConditions, &str); 7] = [
        (
            "input",
            LatencyConditions {
                input_id: "hw:Mic".to_owned(),
                input_name: "Mic".to_owned(),
                ..conditions()
            },
            "input changed to Mic",
        ),
        (
            "output",
            LatencyConditions {
                output_id: "default-out".to_owned(),
                output_name: "Headphones".to_owned(),
                ..conditions()
            },
            "output changed to Headphones",
        ),
        (
            "input rate",
            LatencyConditions {
                input_rate: 44_100,
                ..conditions()
            },
            "input rate changed from 48.0 kHz to 44.1 kHz",
        ),
        (
            "output rate",
            LatencyConditions {
                output_rate: 44_100,
                ..conditions()
            },
            "output rate changed from 48.0 kHz to 44.1 kHz",
        ),
        (
            "input block",
            LatencyConditions {
                input_buffer: 64,
                ..conditions()
            },
            "input block changed from 128 to 64 frames",
        ),
        (
            "output step",
            LatencyConditions {
                output_step: 256,
                ..conditions()
            },
            "output step changed from 128 to 256 frames",
        ),
        (
            "mode",
            LatencyConditions {
                output_mode: "shared".to_owned(),
                ..conditions()
            },
            "output mode changed from real-style to shared",
        ),
    ];
    for (what, now, expected) in cases {
        let reason = measurement()
            .stale_reason(&now)
            .unwrap_or_else(|| panic!("{what} did not mark the measurement stale"));
        assert!(
            reason.contains(expected),
            "{what}: expected {expected:?}, got {reason:?}"
        );
    }
}

#[test]
fn a_renamed_device_keeps_its_measurement() {
    let renamed = LatencyConditions {
        input_name: "Focusrite (renamed)".to_owned(),
        output_name: "Speakers 2".to_owned(),
        ..conditions()
    };
    assert_eq!(measurement().stale_reason(&renamed), None);
}

#[test]
fn an_impossible_measurement_is_dropped() {
    assert!(measurement().sanitized().is_some());
    for broken in [
        MeasuredLatency {
            latency_ms: f32::NAN,
            ..measurement()
        },
        MeasuredLatency {
            latency_ms: -1.0,
            ..measurement()
        },
        MeasuredLatency {
            latency_ms: 0.0,
            ..measurement()
        },
        MeasuredLatency {
            samples: 0,
            ..measurement()
        },
        MeasuredLatency {
            input_rate: 0,
            ..measurement()
        },
        MeasuredLatency {
            output_id: String::new(),
            ..measurement()
        },
    ] {
        assert_eq!(broken.sanitized(), None);
    }
}

#[test]
fn sanitizing_clamps_the_confidence() {
    let measured = MeasuredLatency {
        confidence: 2.5,
        ..measurement()
    }
    .sanitized()
    .expect("a plausible measurement");
    assert_eq!(measured.confidence, 1.0);
}

#[test]
fn the_summary_says_measured_and_names_the_stale_reason() {
    let text = measurement().summary(Some("the output rate changed"));
    assert!(text.contains("21.5 ms"));
    assert!(text.contains("1032 smp"));
    assert!(
        text.contains("OUT Speakers → cable → IN Focusrite USB"),
        "{text}"
    );
    assert!(text.contains("48.0 kHz"));
    assert!(text.contains("2023-11-14 22:13 UTC"));
    assert!(text.contains("stale: the output rate changed"));
}

#[test]
fn the_opened_mode_is_the_comparison_key() {
    let cases = [
        (
            "Exclusive · 128 smp step (2.7 ms) · this device only",
            false,
            Some("exclusive"),
        ),
        ("Exclusive · 128 smp", true, Some("exclusive")),
        (
            "Standard output · Exclusive refused: busy",
            true,
            Some("shared"),
        ),
        (
            "Standard output · REAL-style refused: no client",
            true,
            Some("shared"),
        ),
        (
            "REAL-style · 128 smp step (2.7 ms)",
            true,
            Some("real-style"),
        ),
        (
            "Low latency · ALSA smallest period",
            false,
            Some("low-latency"),
        ),
        (
            "Low latency · ALSA smallest period · fixed buffer refused, driver default",
            false,
            Some("standard"),
        ),
        ("Standard output", false, Some("standard")),
        ("Standard output", true, Some("shared")),
        ("ASIO output · driver buffer 128 smp", true, Some("asio")),
        (
            "Low latency · PipeWire duplex node · 128 frames at 48000 Hz · no bridge buffer",
            false,
            Some("low-latency"),
        ),
        (
            "PipeWire duplex node · 1024 frames at 48000 Hz · no bridge buffer",
            false,
            Some("standard"),
        ),
        ("", false, None),
    ];
    for (mode, windows, expected) in cases {
        assert_eq!(
            super::opened_output_mode_key_on(mode, windows),
            expected,
            "{mode:?} windows={windows}"
        );
    }
}

#[test]
fn unix_seconds_read_as_utc_dates() {
    assert_eq!(utc_stamp(0), "1970-01-01 00:00 UTC");
    assert_eq!(utc_stamp(1_700_000_000), "2023-11-14 22:13 UTC");
    // A leap day.
    assert_eq!(utc_stamp(951_782_400), "2000-02-29 00:00 UTC");
    assert_eq!(civil_from_days(0), (1970, 1, 1));
}
