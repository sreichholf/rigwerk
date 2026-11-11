use std::time::Instant;

use super::{
    analyze_capture, analyze_channels, click_waveform, round_trip_ms, Calibration,
    CalibrationEmitter, CalibrationFailure, CalibrationState, CAPTURE_SECONDS, CLICK_LEVEL,
    CLICK_SECONDS, MAX_CLICK_SAMPLES,
};

/// A deterministic noise source, so a test never depends on a hardware RNG
/// (or the standard library's thread-local one).
fn noise(len: usize, amplitude: f32, seed: u32) -> Vec<f32> {
    let mut state = seed;
    (0..len)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let unit = state as f32 / u32::MAX as f32 * 2.0 - 1.0;
            unit * amplitude
        })
        .collect()
}

fn capture_with_click(rate: u32, seconds: f32, at_seconds: f32) -> Vec<f32> {
    let template = click_waveform(rate);
    let mut capture = vec![0.0f32; (seconds * rate as f32) as usize];
    let at = (at_seconds * rate as f32) as usize;
    capture[at..at + template.len()].copy_from_slice(&template);
    capture
}

#[test]
fn the_click_is_a_short_bounded_chirp() {
    let click = click_waveform(48_000);
    let wanted = (CLICK_SECONDS * 48_000.0) as usize;
    assert_eq!(click.len(), wanted);
    assert!(click.len() <= MAX_CLICK_SAMPLES);
    let peak = click
        .iter()
        .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
    assert!(peak <= CLICK_LEVEL + 1e-6, "click peaks at {peak}");
    assert!(peak > CLICK_LEVEL * 0.4, "click is too quiet: {peak}");
    // The window starts and ends at silence, so the click has no step.
    assert!(click[0].abs() < 1e-4);
    assert!(click[click.len() - 1].abs() < 1e-4);
    // Changing the rate changes the length, not the seconds it lasts.
    let fast = click_waveform(96_000);
    assert_eq!(fast.len(), 2 * click.len());
}

#[test]
fn the_correlator_finds_the_click_at_its_known_offset() {
    for (rate, seconds, at) in [(48_000, 0.5, 0.25), (44_100, 0.4, 0.05)] {
        let template = click_waveform(rate);
        let capture = capture_with_click(rate, seconds, at);
        let analysis = analyze_capture(&capture, &template, rate).expect("a click was captured");
        let expected = (at * rate as f32) as usize;
        assert!(
            analysis.peak.abs_diff(expected) <= 1,
            "peak {} at {rate} Hz, expected {expected}",
            analysis.peak
        );
        assert!(analysis.score > 0.9, "score {}", analysis.score);
    }
}

#[test]
fn the_correlator_survives_a_noisy_loop() {
    let rate = 48_000;
    let template = click_waveform(rate);
    let mut capture = capture_with_click(rate, 0.6, 0.2);
    let noise = noise(capture.len(), 0.01, 7);
    for (sample, noise) in capture.iter_mut().zip(noise) {
        *sample += noise;
    }
    let analysis = analyze_capture(&capture, &template, rate).expect("a click was captured");
    let expected = (0.2 * rate as f32) as usize;
    assert!(
        analysis.peak.abs_diff(expected) <= 2,
        "peak {} expected {expected}",
        analysis.peak
    );
    assert!(analysis.score > 0.5, "score {}", analysis.score);
}

#[test]
fn a_clipped_capture_is_refused() {
    let rate = 48_000;
    let template = click_waveform(rate);
    let mut capture = capture_with_click(rate, 0.4, 0.1);
    for sample in &mut capture {
        *sample *= 4.0;
    }
    assert_eq!(
        analyze_capture(&capture, &template, rate),
        Err(CalibrationFailure::Clipped)
    );
}

#[test]
fn a_capture_below_the_noise_floor_is_refused() {
    let rate = 48_000;
    let template = click_waveform(rate);
    let capture = vec![0.0001f32; rate as usize / 2];
    match analyze_capture(&capture, &template, rate) {
        Err(CalibrationFailure::TooQuiet { .. }) => {}
        other => panic!("expected TooQuiet, got {other:?}"),
    }
}

#[test]
fn a_capture_without_the_click_has_no_peak() {
    let rate = 48_000;
    let template = click_waveform(rate);
    let capture = noise(rate as usize / 2, 0.02, 11);
    match analyze_capture(&capture, &template, rate) {
        Err(CalibrationFailure::NoPeak { .. }) => {}
        other => panic!("expected NoPeak, got {other:?}"),
    }
}

#[test]
fn a_capture_too_short_for_the_click_is_refused() {
    let rate = 48_000;
    let template = click_waveform(rate);
    let capture = click_waveform(rate)[..template.len() / 2].to_vec();
    assert_eq!(
        analyze_capture(&capture, &template, rate),
        Err(CalibrationFailure::TooShort)
    );
}

#[test]
fn the_round_trip_uses_the_rate_the_signal_arrives_at() {
    // The click left at the capture start and came back 20 ms later: 441
    // input samples at 22.05 kHz, 960 at 48 kHz, both 20 ms.
    let capture_start = 10_000_000_000u64;
    let (ms, samples) =
        round_trip_ms(441, capture_start, capture_start, 22_050).expect("plausible");
    assert!((ms - 20.0).abs() < 0.01, "{ms} ms");
    assert_eq!(samples, 441);
    let (ms, samples) =
        round_trip_ms(960, capture_start, capture_start, 48_000).expect("plausible");
    assert!((ms - 20.0).abs() < 0.01, "{ms} ms");
    assert_eq!(samples, 960);
}

#[test]
fn an_implausible_round_trip_is_rejected() {
    // The click stamp is after the arrival: not a physical path.
    assert_eq!(
        round_trip_ms(10, 2_000_000_000, 2_100_000_000, 48_000),
        None
    );
    // Beyond the capture window / sane range.
    assert_eq!(round_trip_ms(1_000_000, 1, 1, 48_000), None);
    assert_eq!(round_trip_ms(10, 1, 1, 0), None);
}

#[test]
fn the_emitter_ducks_before_the_click_and_restores_after_a_cancel() {
    let rate = 48_000;
    let click = click_waveform(rate);
    let calibration = Calibration::new();
    calibration.arm(&click, rate, 1, 1).expect("armed");
    assert_eq!(calibration.state(), CalibrationState::Armed);

    // The output has to duck to silence first; capture only then starts, so
    // the program material still fading out is not the noise floor.
    let mut emitter = CalibrationEmitter::new();
    for _ in 0..(super::DUCK_SECONDS * rate as f32) as usize + 2 {
        let (target, step) = emitter.begin(&calibration, rate, Instant::now());
        emitter.next(&calibration, target, step);
    }
    // Capture has to start on the input side before the click may play.
    for _ in 0..(super::SETTLE_SECONDS * rate as f32) as usize + 2 {
        calibration.capture_frame(0.0, 0.0);
    }
    assert_eq!(calibration.state(), CalibrationState::Capturing);

    let mut emitted = Vec::new();
    let mut collecting = false;
    let mut gain = 1.0;
    for _ in 0..40 {
        let (target, step) = emitter.begin(&calibration, rate, Instant::now());
        if calibration.state() == CalibrationState::Emitting {
            collecting = true;
        }
        for _ in 0..64 {
            let (current, sample) = emitter.next(&calibration, target, step);
            gain = current;
            if collecting {
                emitted.push(sample);
            }
        }
    }
    assert!(emitted.len() >= click.len(), "the click did not finish");
    assert_eq!(
        &emitted[..click.len()],
        click.as_slice(),
        "the whole click played once"
    );
    assert!(gain < 0.001, "the mix did not reach silence: {gain}");
    assert_eq!(calibration.state(), CalibrationState::Tail);
    assert!(calibration.emitted_ns() > 0);

    // The app cancelling (or the bridge stopping) ends the duck.
    calibration.finish();
    let (target, step) = emitter.begin(&calibration, rate, Instant::now());
    for _ in 0..(super::DUCK_SECONDS * rate as f32) as usize + 2 {
        let (current, _) = emitter.next(&calibration, target, step);
        gain = current;
    }
    assert!(
        (gain - 1.0).abs() < 1e-6,
        "the mix did not come back: {gain}"
    );
}

#[test]
fn the_capture_window_holds_a_full_measurement() {
    let rate = 48_000;
    let click = click_waveform(rate);
    let calibration = Calibration::new();
    calibration.arm(&click, rate, 1, 1).expect("armed");
    calibration.note_ducked();
    let wanted = (CAPTURE_SECONDS * rate as f32) as usize;
    for index in 0..wanted + 100 {
        calibration.capture_frame(0.001 * index as f32, 0.001 * index as f32);
    }
    assert_eq!(calibration.captured(), wanted, "the window is capped");
    // The click had not played yet, so the window stays open (the app's
    // timeout catches this); once the tail begins the input closes it.
    assert_eq!(calibration.state(), CalibrationState::Capturing);
}

#[test]
fn a_finished_capture_needs_the_tail_before_it_reports_ready() {
    let rate = 48_000;
    let click = click_waveform(rate);
    let calibration = Calibration::new();
    calibration.arm(&click, rate, 1, 1).expect("armed");
    let wanted = (CAPTURE_SECONDS * rate as f32) as usize;
    calibration.note_ducked();
    for _ in 0..10 {
        calibration.capture_frame(0.0, 0.0);
    }
    assert_eq!(calibration.state(), CalibrationState::Capturing);
    // Pretend the mixer emitted and finished its click.
    assert!(calibration.note_emitting(Instant::now()));
    calibration.note_emitted();
    assert_eq!(calibration.state(), CalibrationState::Tail);
    for _ in 0..wanted {
        calibration.capture_frame(0.0, 0.0);
    }
    assert_eq!(calibration.state(), CalibrationState::Ready);
    let mut left = Vec::new();
    let mut right = Vec::new();
    assert_eq!(calibration.copy_capture(&mut left, &mut right), wanted);
    assert_eq!(left.len(), wanted);
    assert_eq!(right.len(), wanted);
    calibration.finish();
    assert_eq!(calibration.state(), CalibrationState::Idle);
}

#[test]
fn a_retry_plays_a_second_click_even_without_an_idle_block() {
    let rate = 48_000;
    let click = click_waveform(rate);
    let calibration = Calibration::new();
    calibration.arm(&click, rate, 1, 1).expect("first arm");
    calibration.note_ducked();
    for _ in 0..(super::SETTLE_SECONDS * rate as f32) as usize + 2 {
        calibration.capture_frame(0.0, 0.0);
    }
    let mut emitter = CalibrationEmitter::new();
    let (target, step) = emitter.begin(&calibration, rate, Instant::now());
    let mut played = 0;
    for _ in 0..64 {
        let (_, sample) = emitter.next(&calibration, target, step);
        let _ = sample;
        played += 1;
    }
    assert!(played > 0);
    assert_eq!(calibration.state(), CalibrationState::Emitting);

    // Finish and arm again before the mixer ever sees `Idle`: a retry from
    // the panel can do exactly this.
    calibration.finish();
    calibration.arm(&click, rate, 1, 1).expect("second arm");
    calibration.note_ducked();
    for _ in 0..(super::SETTLE_SECONDS * rate as f32) as usize + 2 {
        calibration.capture_frame(0.0, 0.0);
    }
    let (target, step) = emitter.begin(&calibration, rate, Instant::now());
    assert_eq!(calibration.state(), CalibrationState::Emitting);
    let mut emitted = Vec::new();
    for _ in 0..click.len() {
        let (_, sample) = emitter.next(&calibration, target, step);
        emitted.push(sample);
    }
    assert_eq!(emitted, click, "the retry did not play the click again");
}

#[test]
fn a_quiet_click_in_noise_still_correlates() {
    let rate = 48_000;
    let template = click_waveform(rate);
    let mut capture = vec![0.0f32; rate as usize / 2];
    let at = (0.25 * rate as f32) as usize;
    for (slot, sample) in capture[at..at + template.len()].iter_mut().zip(&template) {
        *slot = sample * 0.3;
    }
    let noise = noise(capture.len(), 0.01, 23);
    for (sample, noise) in capture.iter_mut().zip(noise) {
        *sample += noise;
    }
    let analysis = analyze_capture(&capture, &template, rate).expect("a quiet click");
    assert!(analysis.peak.abs_diff(at) <= 2, "peak {}", analysis.peak);
    assert!(analysis.score > 0.4, "score {}", analysis.score);
}

#[test]
fn a_loud_tonal_burst_is_not_mistaken_for_the_click() {
    let rate = 48_000;
    let template = click_waveform(rate);
    let mut capture = vec![0.0f32; rate as usize / 2];
    let at = (0.2 * rate as f32) as usize;
    let len = template.len();
    for (index, slot) in capture[at..at + len].iter_mut().enumerate() {
        let time = index as f32 / rate as f32;
        let window = 0.5 - 0.5 * (std::f32::consts::TAU * index as f32 / len as f32).cos();
        *slot = 0.5 * window * (std::f32::consts::TAU * 3_000.0 * time).sin();
    }
    match analyze_capture(&capture, &template, rate) {
        Err(CalibrationFailure::NoPeak { .. }) => {}
        other => panic!("expected NoPeak, got {other:?}"),
    }
}

#[test]
fn the_analyzer_keeps_the_delay_in_seconds_at_other_input_rates() {
    // The click is rendered once at 48 kHz; the capture and its template run
    // at the input's own rate.  The same 150 ms delay must come back as
    // 150 ms, not as 150 ms worth of the output rate's samples.
    let output_rate = 48_000;
    let played = click_waveform(output_rate);
    let seconds = played.len() as f32 / output_rate as f32;
    for rate in [44_100u32, 96_000] {
        let len = (seconds * rate as f32) as usize;
        let resampled: Vec<f32> = (0..len)
            .map(|index| {
                let source = index as f32 * output_rate as f32 / rate as f32;
                let low = source.floor() as usize;
                let high = (low + 1).min(played.len() - 1);
                let t = source - low as f32;
                played[low] * (1.0 - t) + played[high] * t
            })
            .collect();
        let mut capture = vec![0.0f32; (0.6 * rate as f32) as usize];
        let at = (0.15 * rate as f32) as usize;
        capture[at..at + resampled.len()].copy_from_slice(&resampled);
        let template = click_waveform(rate);
        let analysis = analyze_capture(&capture, &template, rate).expect("a click was captured");
        assert!(
            analysis.peak.abs_diff(at) <= 2,
            "{rate} Hz: peak {} expected {at}",
            analysis.peak
        );
        let ms = analysis.peak as f32 / rate as f32 * 1000.0;
        assert!((ms - 150.0).abs() < 0.2, "{rate} Hz: {ms} ms");
    }
}

#[test]
fn a_cancel_before_capture_leaves_the_state_idle() {
    let rate = 48_000;
    let calibration = Calibration::new();
    calibration
        .arm(&click_waveform(rate), rate, 1, 1)
        .expect("armed");
    calibration.note_ducked();
    calibration.finish();
    for _ in 0..100 {
        calibration.capture_frame(0.0, 0.0);
    }
    assert_eq!(calibration.state(), CalibrationState::Idle);
    assert_eq!(calibration.captured(), 0);
}

#[test]
fn a_stale_frame_cannot_publish_into_the_next_arm() {
    let rate = 48_000;
    let click = click_waveform(rate);
    let calibration = Calibration::new();
    calibration.arm(&click, rate, 128, 128).expect("armed");
    calibration.note_ducked();
    for _ in 0..300 {
        calibration.capture_frame(0.25, -0.25);
    }
    let stale = calibration.generation();
    let index = calibration.captured();
    assert!(index > 10, "the first arm did not capture");
    // The retry path finishes and arms again with a callback still holding
    // the old generation.
    calibration.finish();
    calibration.arm(&click, rate, 128, 128).expect("rearmed");
    assert_eq!(calibration.state(), CalibrationState::Armed);
    assert_eq!(calibration.captured(), 0);
    assert!(
        !calibration.commit_frame(stale, -1.0, 1.0),
        "the old generation published a frame"
    );
    assert_eq!(calibration.captured(), 0);
    assert_eq!(calibration.state(), CalibrationState::Armed);
}

#[test]
fn an_inverted_pair_still_correlates_on_the_louder_channel() {
    let rate = 48_000;
    let template = click_waveform(rate);
    let left = vec![0.0f32; rate as usize / 2];
    let mut right = vec![0.0f32; left.len()];
    let at = (0.2 * rate as f32) as usize;
    for (slot, sample) in right[at..at + template.len()].iter_mut().zip(&template) {
        *slot = -sample;
    }
    // The sum of the two channels is silence.  Either channel on its own is
    // the click.
    let analysis = analyze_channels(&left, &right, &template, rate).expect("the right channel");
    assert!(analysis.peak.abs_diff(at) <= 1, "peak {}", analysis.peak);
    assert!(analysis.score > 0.9, "score {}", analysis.score);
}

#[test]
fn the_emitted_click_correlates_at_the_input_rate() {
    let output_rate = 48_000u32;
    let click = click_waveform(output_rate);
    let calibration = Calibration::new();
    calibration.arm(&click, output_rate, 1, 1).expect("armed");
    let mut emitter = CalibrationEmitter::new();
    for _ in 0..(super::DUCK_SECONDS * output_rate as f32) as usize + 2 {
        let (target, step) = emitter.begin(&calibration, output_rate, std::time::Instant::now());
        emitter.next(&calibration, target, step);
    }
    for _ in 0..(super::SETTLE_SECONDS * output_rate as f32) as usize + 2 {
        calibration.capture_frame(0.0, 0.0);
    }
    let (target, step) = emitter.begin(&calibration, output_rate, std::time::Instant::now());
    assert_eq!(calibration.state(), CalibrationState::Emitting);
    let mut emitted = Vec::new();
    for _ in 0..click.len() {
        let (_, sample) = emitter.next(&calibration, target, step);
        emitted.push(sample);
    }
    assert_eq!(
        emitted, click,
        "the mixer did not emit the click it was armed with"
    );

    for input_rate in [48_000u32, 44_100] {
        let played = if input_rate == output_rate {
            emitted.clone()
        } else {
            let len = (emitted.len() as f32 * input_rate as f32 / output_rate as f32) as usize;
            (0..len)
                .map(|index| {
                    let source = index as f32 * output_rate as f32 / input_rate as f32;
                    let low = source.floor() as usize;
                    let high = (low + 1).min(emitted.len() - 1);
                    let t = source - low as f32;
                    emitted[low] * (1.0 - t) + emitted[high] * t
                })
                .collect()
        };
        let mut left = vec![0.0f32; (0.6 * input_rate as f32) as usize];
        let right = vec![0.0f32; left.len()];
        let at = (0.15 * input_rate as f32) as usize;
        for (slot, sample) in left[at..at + played.len()].iter_mut().zip(&played) {
            *slot = *sample;
        }
        let template = click_waveform(input_rate);
        let analysis =
            analyze_channels(&left, &right, &template, input_rate).expect("the emitted click");
        assert!(
            analysis.peak.abs_diff(at) <= 2,
            "{input_rate} Hz: peak {} expected {at}",
            analysis.peak
        );
    }
}

#[test]
fn a_cancel_before_the_click_does_not_start_emitting() {
    let rate = 48_000;
    let calibration = Calibration::new();
    calibration
        .arm(&click_waveform(rate), rate, 1, 1)
        .expect("armed");
    calibration.note_ducked();
    calibration.finish();
    assert!(!calibration.note_emitting(Instant::now()));
    assert_eq!(calibration.state(), CalibrationState::Idle);
}
