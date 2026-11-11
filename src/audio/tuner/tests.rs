use super::*;

const RATE: u32 = 48_000;

fn frames_for(rate: u32) -> usize {
    let factor = ((rate / 12_000).clamp(1, 8)) as usize;
    (YIN_WINDOW * factor)
        .max(poly_window_len(rate, MIN_HZ))
        .max(4_096)
}

fn tone(frequency: f32, rate: u32, amplitude: f32, frames: usize, harmonics: usize) -> Vec<f32> {
    (0..frames)
        .map(|index| {
            let phase = TAU * frequency * index as f32 / rate as f32;
            (1..=harmonics)
                .map(|harmonic| amplitude / harmonic as f32 * (phase * harmonic as f32).sin())
                .sum()
        })
        .collect()
}

/// A harmonic series with explicit partial amplitudes, for shaping one
/// partial at a time.
fn tone_with(frequency: f32, rate: u32, frames: usize, amplitudes: &[f32]) -> Vec<f32> {
    (0..frames)
        .map(|index| {
            let phase = TAU * frequency * index as f32 / rate as f32;
            amplitudes
                .iter()
                .enumerate()
                .map(|(harmonic, amplitude)| amplitude * (phase * (harmonic + 1) as f32).sin())
                .sum()
        })
        .collect()
}

fn chord(rate: u32, frames: usize, spec: &[(f32, f32)]) -> Vec<f32> {
    let mut mixed = vec![0.0f32; frames];
    for &(frequency, amplitude) in spec {
        for (sample, value) in mixed
            .iter_mut()
            .zip(tone(frequency, rate, amplitude, frames, HARMONICS))
        {
            *sample += value;
        }
    }
    mixed
}

fn interleave(mono: &[f32]) -> Vec<f32> {
    let mut interleaved = Vec::with_capacity(mono.len() * 2);
    for &sample in mono {
        interleaved.push(sample);
        interleaved.push(sample);
    }
    interleaved
}

fn analyse_mono(
    mono: &[f32],
    rate: u32,
    preset: Option<&'static Tuning>,
    tolerance: TunerTolerance,
) -> TunerAnalysis {
    let interleaved = interleave(mono);
    let mut analysis = TunerAnalysis::new();
    analysis.analyse(
        &interleaved,
        mono.len(),
        rate,
        preset,
        TunerChannel::Sum,
        440.0,
        tolerance,
    );
    analysis
}

/// The frequency and amplitude of every string of a preset at the given
/// detunes.
fn chord_spec(tuning: &Tuning, detunes: &[f32]) -> Vec<(f32, f32)> {
    tuning
        .strings
        .iter()
        .zip(detunes)
        .enumerate()
        .map(|(index, ((_, midi), cents))| {
            (
                midi_to_hz(*midi, 440.0) * cents_to_ratio(*cents),
                0.18 / ((index + 1) as f32).sqrt(),
            )
        })
        .collect()
}

/// Plays every string of a preset at the given detunes and checks that
/// each row sounds and lands on the injected cents.
fn assert_chord(id: &str, detunes: &[f32], allowed_cents: f32) {
    let tuning = tuning_by_id(id).unwrap();
    assert_eq!(tuning.strings.len(), detunes.len(), "{id}: detune list");
    let rate = 48_000;
    let spec = chord_spec(tuning, detunes);
    let mono = chord(rate, frames_for(rate), &spec);
    let analysis = analyse_mono(&mono, rate, Some(tuning), TunerTolerance::Standard);
    let frame = analysis.frame();
    assert_eq!(frame.strings.len(), tuning.strings.len(), "{id}: rows");
    for (index, expected) in detunes.iter().enumerate() {
        let row = &frame.strings[index];
        assert!(row.sounding, "{id}: {} did not sound", row.name);
        assert!(
            row.harmonic_of.is_none(),
            "{id}: played {} was flagged as a harmonic of {:?}",
            row.name,
            row.harmonic_of
        );
        assert!(
            (row.cents - expected).abs() <= allowed_cents,
            "{id}: {} reads {:.2} cents, injected {expected}",
            row.name,
            row.cents
        );
    }
}

/// The presets spell some notes with flats (Eb2, Bb3), so this compares
/// enharmonically instead of against the sharp-only names.
fn name_matches_midi(name: &str, midi: i16) -> bool {
    const SPELLINGS: [(&str, i16); 17] = [
        ("C", 0),
        ("C#", 1),
        ("Db", 1),
        ("D", 2),
        ("D#", 3),
        ("Eb", 3),
        ("E", 4),
        ("F", 5),
        ("F#", 6),
        ("Gb", 6),
        ("G", 7),
        ("G#", 8),
        ("Ab", 8),
        ("A", 9),
        ("A#", 10),
        ("Bb", 10),
        ("B", 11),
    ];
    let split = (0..name.len())
        .rev()
        .find(|&index| !name.as_bytes()[index].is_ascii_digit())
        .map(|index| index + 1)
        .unwrap_or(0);
    let (letters, digits) = name.split_at(split);
    let Ok(octave) = digits.parse::<i16>() else {
        return false;
    };
    let Some((_, pitch)) = SPELLINGS.iter().find(|(spelling, _)| *spelling == letters) else {
        return false;
    };
    midi.rem_euclid(12) == *pitch && octave == midi.div_euclid(12) - 1
}

#[test]
fn note_math_reference_frequencies() {
    assert!((midi_to_hz(69, 440.0) - 440.0).abs() < 0.001);
    assert!((midi_to_hz(40, 440.0) - 82.4069).abs() < 0.01);
    assert!((midi_to_hz(21, 440.0) - 27.5).abs() < 0.01);
    assert!((midi_to_hz(36, 440.0) - 65.406).abs() < 0.01);
    assert!((hz_to_cents(880.0, 440.0) - 1200.0).abs() < 0.01);
    assert!((hz_to_cents(466.1638, 440.0) - 100.0).abs() < 0.05);
    assert!((cents_to_ratio(1200.0) - 2.0).abs() < 1e-5);
    assert!((cents_to_ratio(100.0) - 1.0594631).abs() < 1e-5);
}

#[test]
fn note_names_and_octaves() {
    assert_eq!(midi_to_name(21), "A0");
    assert_eq!(midi_to_name(23), "B0");
    assert_eq!(midi_to_name(36), "C2");
    assert_eq!(midi_to_name(40), "E2");
    assert_eq!(midi_to_name(45), "A2");
    assert_eq!(midi_to_name(63), "D#4");
    assert_eq!(midi_to_name(64), "E4");
    assert_eq!(octave(60), 4);
    assert_eq!(octave(21), 0);
    assert_eq!(nearest_midi(440.0, 440.0), 69);
    assert_eq!(nearest_midi(82.4069, 440.0), 40);
    assert_eq!(nearest_midi(0.0, 440.0), 69);
}

#[test]
fn cents_sign_points_at_the_direction_to_tune() {
    // Below the target is negative, which the page renders as "flat".
    assert!(hz_to_cents(430.0, 440.0) < 0.0);
    assert!(hz_to_cents(450.0, 440.0) > 0.0);
    assert_eq!(hz_to_cents(0.0, 440.0), 0.0);
}

#[test]
fn presets_are_well_formed() {
    assert_eq!(PRESETS.len(), 20);
    for (index, tuning) in PRESETS.iter().enumerate() {
        assert!(
            PRESETS[..index].iter().all(|other| other.id != tuning.id),
            "duplicate id {}",
            tuning.id
        );
        assert!(!tuning.strings.is_empty(), "{} has no strings", tuning.id);
        assert!(
            tuning.strings.len() <= MAX_STRINGS,
            "{} has too many strings",
            tuning.id
        );
        let mut previous = i16::MIN;
        for (name, midi) in tuning.strings {
            assert!(
                (21..=64).contains(midi),
                "{} {} is outside A0..E4",
                tuning.id,
                name
            );
            assert!(
                *midi > previous,
                "{} strings are not low to high",
                tuning.id
            );
            previous = *midi;
            assert!(
                name_matches_midi(name, *midi),
                "{}: name {name} does not match MIDI {midi}",
                tuning.id
            );
        }
        let lowest = midi_to_hz(tuning.strings[0].1, 440.0);
        assert!(lowest >= 27.0, "{} goes below A0: {lowest}", tuning.id);
    }
}

#[test]
fn preset_ids_and_names_match_the_table() {
    // Literal spelling, in table order: the page shows these strings, and
    // "either spelling" would let "D#2" creep into a preset that should
    // read "Eb2".
    const EXPECTED: [(&str, &[&str]); 20] = [
        ("e-standard", &["E2", "A2", "D3", "G3", "B3", "E4"]),
        ("eb-standard", &["Eb2", "Ab2", "Db3", "Gb3", "Bb3", "Eb4"]),
        ("d-standard", &["D2", "G2", "C3", "F3", "A3", "D4"]),
        (
            "c-sharp-standard",
            &["C#2", "F#2", "B2", "E3", "G#3", "C#4"],
        ),
        ("c-standard", &["C2", "F2", "Bb2", "Eb3", "G3", "C4"]),
        ("drop-d", &["D2", "A2", "D3", "G3", "B3", "E4"]),
        ("drop-c-sharp", &["C#2", "G#2", "C#3", "F#3", "A#3", "D#4"]),
        ("drop-c", &["C2", "G2", "C3", "F3", "A3", "D4"]),
        ("drop-b", &["B1", "F#2", "B2", "E3", "G#3", "C#4"]),
        ("drop-a", &["A1", "E2", "A2", "D3", "F#3", "B3"]),
        ("7-string-b", &["B1", "E2", "A2", "D3", "G3", "B3", "E4"]),
        (
            "7-string-drop-a",
            &["A1", "E2", "A2", "D3", "G3", "B3", "E4"],
        ),
        (
            "8-string-f-sharp",
            &["F#1", "B1", "E2", "A2", "D3", "G3", "B3", "E4"],
        ),
        (
            "8-string-drop-e",
            &["E1", "B1", "E2", "A2", "D3", "G3", "B3", "E4"],
        ),
        ("bass-4-e", &["E1", "A1", "D2", "G2"]),
        ("bass-4-eb", &["Eb1", "Ab1", "Db2", "Gb2"]),
        ("bass-4-d", &["D1", "G1", "C2", "F2"]),
        ("bass-5-b", &["B0", "E1", "A1", "D2", "G2"]),
        ("bass-5-drop-a", &["A0", "E1", "A1", "D2", "G2"]),
        ("bass-6-b", &["B0", "E1", "A1", "D2", "G2", "C3"]),
    ];
    assert_eq!(PRESETS.len(), EXPECTED.len());
    for (tuning, (id, names)) in PRESETS.iter().zip(EXPECTED) {
        assert_eq!(tuning.id, id);
        let actual: Vec<&str> = tuning.strings.iter().map(|(name, _)| *name).collect();
        assert_eq!(actual, names, "{}: string names", id);
    }
}

#[test]
fn preset_groups_are_listed_in_order() {
    let mut seen = Vec::new();
    let mut counts = [0usize; 3];
    for tuning in PRESETS {
        let position = TuningGroup::ALL
            .iter()
            .position(|group| *group == tuning.group)
            .unwrap();
        counts[position] += 1;
        if seen.last() != Some(&position) {
            assert!(
                !seen.contains(&position),
                "group {:?} appears twice",
                tuning.group
            );
            seen.push(position);
        }
    }
    assert_eq!(
        seen,
        vec![0, 1, 2],
        "groups must run guitar, extended, bass"
    );
    assert_eq!(counts, [10, 4, 6]);
}

#[test]
fn preset_lookup_round_trips() {
    for (index, tuning) in PRESETS.iter().enumerate() {
        assert_eq!(tuning_index(tuning.id), Some(index));
        assert_eq!(
            tuning_by_id(tuning.id).map(|found| found.id),
            Some(tuning.id)
        );
    }
    assert_eq!(tuning_index("chromatic"), None);
    assert!(tuning_by_id("not-a-tuning").is_none());
    assert_eq!(DEFAULT_PRESET_ID, "chromatic");
}

#[test]
fn yin_tracks_sine_at_supported_rates() {
    for &rate in &[44_100u32, 48_000, 96_000] {
        for &frequency in &[55.0f32, 82.4069, 110.0, 220.0, 440.0, 880.0] {
            let frames = frames_for(rate);
            let mono = tone(frequency, rate, 0.5, frames, 1);
            let analysis = analyse_mono(&mono, rate, None, TunerTolerance::Standard);
            let reading = &analysis.frame().mono;
            assert!(
                reading.valid,
                "{frequency} Hz at {rate} Hz was not detected"
            );
            let cents = hz_to_cents(reading.frequency, frequency);
            let allowed = if frequency < 60.0 { 2.0 } else { 1.0 };
            assert!(
                cents.abs() <= allowed,
                "{frequency} Hz at {rate} Hz read {:.3} Hz ({cents:.2} cents)",
                reading.frequency
            );
        }
    }
}

#[test]
fn yin_tracks_a0_at_supported_rates() {
    // A0 at each supported rate.
    for &rate in &[44_100u32, 48_000, 96_000, 192_000] {
        let frames = frames_for(rate);
        let mono = tone(27.5, rate, 0.5, frames, 1);
        let analysis = analyse_mono(&mono, rate, None, TunerTolerance::Standard);
        let reading = &analysis.frame().mono;
        assert!(reading.valid, "A0 at {rate} Hz was not detected");
        let cents = hz_to_cents(reading.frequency, 27.5);
        assert!(
            cents.abs() <= 2.0,
            "A0 at {rate} Hz read {:.3} Hz ({cents:.2} cents)",
            reading.frequency
        );
    }
}

#[test]
fn yin_tracks_harmonic_tone() {
    let mono = tone(110.0, RATE, 0.1, frames_for(RATE), HARMONICS);
    let analysis = analyse_mono(&mono, RATE, None, TunerTolerance::Standard);
    let reading = &analysis.frame().mono;
    assert!(reading.valid);
    assert!(hz_to_cents(reading.frequency, 110.0).abs() <= 1.0);
    assert_eq!(reading.midi, 45);
}

#[test]
fn yin_rejects_silence_and_noise() {
    let frames = frames_for(RATE);
    let analysis = analyse_mono(&vec![0.0f32; frames], RATE, None, TunerTolerance::Standard);
    assert!(!analysis.frame().mono.valid);
    assert_eq!(analysis.frame().mono.state, TuneState::Silent);

    // A fixed linear-congruential noise floor: broadband, no period.
    let mut state = 0x1234_5678u32;
    let noise: Vec<f32> = (0..frames)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((state >> 9) as f32 / 4_194_304.0 - 1.0) * 0.3
        })
        .collect();
    let analysis = analyse_mono(&noise, RATE, None, TunerTolerance::Standard);
    assert!(!analysis.frame().mono.valid);

    // With a preset the inter-harmonic null is what has to reject it: a
    // noise floor fills the harmonics and the midpoints alike.
    let tuning = tuning_by_id("drop-c").unwrap();
    let analysis = analyse_mono(&noise, RATE, Some(tuning), TunerTolerance::Standard);
    for row in &analysis.frame().strings {
        assert!(!row.sounding, "{} must not sound for white noise", row.name);
    }
}

#[test]
fn yin_accepts_quiet_sine() {
    let mono = tone(196.0, RATE, 0.002, frames_for(RATE), 1);
    let analysis = analyse_mono(&mono, RATE, None, TunerTolerance::Standard);
    assert!(analysis.frame().mono.valid);
    assert!(hz_to_cents(analysis.frame().mono.frequency, 196.0).abs() <= 2.0);
}

#[test]
fn channel_selects_left_or_right() {
    let frames = frames_for(RATE);
    let mono = tone(110.0, RATE, 0.5, frames, 1);
    let mut buffer = vec![0.0f32; frames * 2];
    for (index, sample) in mono.iter().enumerate() {
        buffer[index * 2] = *sample;
    }
    let mut analysis = TunerAnalysis::new();
    analysis.analyse(
        &buffer,
        frames,
        RATE,
        None,
        TunerChannel::Left,
        440.0,
        TunerTolerance::Standard,
    );
    assert!(analysis.frame().mono.valid);
    assert!((analysis.frame().mono.frequency - 110.0).abs() < 0.2);
    // The right channel is silent, so it is not a reading.
    analysis.analyse(
        &buffer,
        frames,
        RATE,
        None,
        TunerChannel::Right,
        440.0,
        TunerTolerance::Standard,
    );
    assert!(!analysis.frame().mono.valid);
    // Sum halves the amplitude, which is still far above the gate.
    analysis.analyse(
        &buffer,
        frames,
        RATE,
        None,
        TunerChannel::Sum,
        440.0,
        TunerTolerance::Standard,
    );
    assert!(analysis.frame().mono.valid);
    assert!((analysis.frame().mono.frequency - 110.0).abs() < 0.2);
}

#[test]
fn preset_folds_harmonics_to_the_open_string() {
    let tuning = tuning_by_id("e-standard").unwrap();
    // 2x and 3x E2 are E3 and B3.  E3 is not in the preset, and B3 is
    // 1.96 cents flat of the pure twelfth, so both still fold to E2.
    for multiple in 2..=3u32 {
        let frequency = midi_to_hz(40, 440.0) * multiple as f32;
        let mono = tone(frequency, RATE, 0.4, frames_for(RATE), 1);
        let analysis = analyse_mono(&mono, RATE, Some(tuning), TunerTolerance::Standard);
        let reading = &analysis.frame().mono;
        assert!(reading.valid, "{multiple}x E2 was not detected");
        assert_eq!(
            reading.string,
            Some(0),
            "{multiple}x E2 matched the wrong string"
        );
        assert!(
            reading.cents.abs() <= 2.0,
            "{multiple}x E2 folded to {:.2} cents",
            reading.cents
        );
    }
    // 4x E2 is E4, exactly two octaves: an exact octave is both the played
    // string and its neighbour's harmonic, and the played string wins.
    let mono = tone(midi_to_hz(64, 440.0), RATE, 0.4, frames_for(RATE), 1);
    let analysis = analyse_mono(&mono, RATE, Some(tuning), TunerTolerance::Standard);
    let reading = &analysis.frame().mono;
    assert_eq!(reading.string, Some(5), "4x E2 is E4 itself");
    assert!(reading.cents.abs() <= 2.0);
}

#[test]
fn open_strings_match_their_own_row() {
    // Every string of every preset, including the octave pairs a
    // divisor tie could otherwise fold downward.
    for tuning in PRESETS {
        let id = tuning.id;
        for (index, (name, midi)) in tuning.strings.iter().enumerate() {
            let mono = tone(midi_to_hz(*midi, 440.0), RATE, 0.5, frames_for(RATE), 1);
            let analysis = analyse_mono(&mono, RATE, Some(tuning), TunerTolerance::Standard);
            let reading = &analysis.frame().mono;
            assert!(reading.valid, "{id}: {name} was not detected");
            assert_eq!(
                reading.string,
                Some(index),
                "{id}: {name} matched {}",
                reading
                    .string
                    .map(|matched| tuning.strings[matched].0)
                    .unwrap_or("nothing")
            );
            assert!(
                reading.cents.abs() <= 0.5,
                "{id}: {name} read {:.2} cents",
                reading.cents
            );
        }
    }
}

#[test]
fn b3_does_not_fold_onto_e2() {
    let tuning = tuning_by_id("e-standard").unwrap();
    let b3 = midi_to_hz(59, 440.0);
    for detune in [-2.0f32, 0.0, 1.0] {
        let mono = tone(b3 * cents_to_ratio(detune), RATE, 0.5, frames_for(RATE), 1);
        let analysis = analyse_mono(&mono, RATE, Some(tuning), TunerTolerance::Standard);
        let reading = &analysis.frame().mono;
        assert_eq!(
            reading.string,
            Some(4),
            "B3 {detune:+} was folded onto another string"
        );
        assert!(
            (reading.cents - detune).abs() <= 0.5,
            "B3 {detune:+} read {:.2} cents",
            reading.cents
        );
    }
    // Near its target B3 stays B3; the smaller divisor only wins once B3 is
    // close enough to the pure twelfth.  The cents are then relative to E2,
    // which is the limit of a pitch-only matcher.
    let twelfth_offset = hz_to_cents(midi_to_hz(59, 440.0), 3.0 * midi_to_hz(40, 440.0));
    assert!((twelfth_offset + 1.955).abs() < 0.02);
    for detune in [3.0f32, 10.0] {
        let mono = tone(b3 * cents_to_ratio(detune), RATE, 0.5, frames_for(RATE), 1);
        let analysis = analyse_mono(&mono, RATE, Some(tuning), TunerTolerance::Standard);
        let reading = &analysis.frame().mono;
        assert_eq!(
            reading.string,
            Some(0),
            "B3 {detune:+} is inside the twelfth's ambiguity band"
        );
        assert!(
            (reading.cents - (detune + twelfth_offset)).abs() <= 0.5,
            "B3 {detune:+} read {:.2} on E2, expected {:.2}",
            reading.cents,
            detune + twelfth_offset
        );
    }
}

#[test]
fn chromatic_does_not_fold_clean_tones() {
    // The tuner must show A4 for a 440 Hz tone, not A3: a chromatic
    // reading reports what was played.
    let analysis = analyse_mono(
        &tone(440.0, RATE, 0.5, frames_for(RATE), 1),
        RATE,
        None,
        TunerTolerance::Standard,
    );
    let reading = &analysis.frame().mono;
    assert_eq!(reading.midi, 69);
    assert!(reading.string.is_none());
    assert!(hz_to_cents(reading.target_hz, 440.0).abs() < 0.1);
}

#[test]
fn decimated_path_keeps_pitch() {
    let mut readings = Vec::new();
    for &rate in &[44_100u32, 48_000, 96_000] {
        let analysis = analyse_mono(
            &tone(220.0, rate, 0.5, frames_for(rate), 1),
            rate,
            None,
            TunerTolerance::Standard,
        );
        assert!(analysis.frame().mono.valid);
        readings.push(analysis.frame().mono.frequency);
    }
    for frequency in &readings {
        assert!(hz_to_cents(*frequency, 220.0).abs() <= 1.0);
    }
}

#[test]
fn tolerance_states_follow_cents() {
    assert_eq!(classify(0.0, 5.0), TuneState::Centered);
    assert_eq!(classify(0.9, 5.0), TuneState::Centered);
    assert_eq!(classify(-0.9, 5.0), TuneState::Centered);
    assert_eq!(classify(1.1, 3.0), TuneState::InTune);
    assert_eq!(classify(3.0, 3.0), TuneState::InTune);
    assert_eq!(classify(5.0, 5.0), TuneState::InTune);
    assert_eq!(classify(5.1, 5.0), TuneState::Sharp);
    assert_eq!(classify(-5.1, 5.0), TuneState::Flat);
    assert_eq!(classify(10.0, 10.0), TuneState::InTune);
    assert_eq!(classify(10.1, 10.0), TuneState::Sharp);
}

#[test]
fn in_tune_hysteresis_keeps_one_extra_cent() {
    let mut latch = StateLatch::new();
    assert_ne!(latch.update(4.9, 5.0), TuneState::InTune);
    assert_eq!(latch.update(4.9, 5.0), TuneState::InTune);
    // 5.5 is outside the enter band but inside the exit band.
    assert_eq!(latch.update(5.5, 5.0), TuneState::InTune);
    assert_eq!(latch.update(6.1, 5.0), TuneState::Sharp);
    assert_eq!(latch.update(-6.1, 5.0), TuneState::Flat);
}

#[test]
fn in_tune_needs_two_frames_and_leaves_at_once() {
    let mut latch = StateLatch::new();
    assert_ne!(latch.update(2.0, 5.0), TuneState::InTune);
    assert_ne!(latch.update(0.4, 5.0), TuneState::InTune);
    assert_eq!(latch.update(0.4, 5.0), TuneState::Centered);
    // One reading past the exit band leaves immediately.
    assert_eq!(latch.update(6.5, 5.0), TuneState::Sharp);
    assert_ne!(latch.update(2.0, 5.0), TuneState::InTune);
}

#[test]
fn note_change_resets_the_state_bracket() {
    let tuning = tuning_by_id("e-standard").unwrap();
    let frames = frames_for(RATE);
    let mut analysis = TunerAnalysis::new();
    let e2 = interleave(&tone(midi_to_hz(40, 440.0), RATE, 0.5, frames, 1));
    for _ in 0..3 {
        analysis.analyse(
            &e2,
            frames,
            RATE,
            Some(tuning),
            TunerChannel::Sum,
            440.0,
            TunerTolerance::Standard,
        );
    }
    assert!(matches!(
        analysis.frame().mono.state,
        TuneState::InTune | TuneState::Centered
    ));
    // A new note starts its own bracket: the first in-tune A2 frame must
    // not inherit the InTune verdict E2 earned.
    let a2 = interleave(&tone(midi_to_hz(45, 440.0), RATE, 0.5, frames, 1));
    analysis.analyse(
        &a2,
        frames,
        RATE,
        Some(tuning),
        TunerChannel::Sum,
        440.0,
        TunerTolerance::Standard,
    );
    assert_eq!(analysis.frame().mono.string, Some(1));
    assert!(!matches!(
        analysis.frame().mono.state,
        TuneState::InTune | TuneState::Centered
    ));
}

#[test]
fn poly_reads_a_drop_c_chord() {
    assert_chord("drop-c", &[-7.0, 2.0, 0.0, 5.0, -3.0, 1.0], 3.0);
}

#[test]
fn poly_reads_lone_a0() {
    let tuning = tuning_by_id("bass-5-drop-a").unwrap();
    let mono = tone(
        midi_to_hz(21, 440.0),
        RATE,
        0.18,
        frames_for(RATE),
        HARMONICS,
    );
    let analysis = analyse_mono(&mono, RATE, Some(tuning), TunerTolerance::Standard);
    let frame = analysis.frame();
    assert!(frame.strings[0].sounding, "A0 must sound");
    assert!(
        frame.strings[0].cents.abs() <= 2.0,
        "A0 read {:.2} cents",
        frame.strings[0].cents
    );
    // A1's target is A0's second harmonic, so it may appear as a ghost;
    // nothing else has a partial where it needs one.
    for row in &frame.strings[1..] {
        assert!(
            !row.sounding || row.harmonic_of.is_some(),
            "{} must not be independent of A0",
            row.name
        );
    }
    let a1 = &frame.strings[2];
    assert!(
        !a1.sounding || a1.harmonic_of == Some(0),
        "A1 must be flagged when it appears"
    );
}

#[test]
fn poly_reads_bass_five_drop_a_chord() {
    assert_chord("bass-5-drop-a", &[-4.0, 2.0, 0.0, 3.0, -2.0], 5.0);
}

#[test]
fn poly_reads_seven_string_drop_a_chord() {
    assert_chord(
        "7-string-drop-a",
        &[-4.0, 2.0, 0.0, 3.0, -2.0, 1.0, 4.0],
        5.0,
    );
}

#[test]
fn poly_reads_eight_string_drop_e_chord() {
    assert_chord(
        "8-string-drop-e",
        &[-4.0, 2.0, 0.0, 3.0, -2.0, 1.0, 4.0, -1.0],
        5.0,
    );
}

#[test]
fn poly_emits_only_preset_strings() {
    let tuning = tuning_by_id("drop-c").unwrap();
    let mono = chord(
        RATE,
        frames_for(RATE),
        &[(midi_to_hz(43, 440.0), 0.2), (midi_to_hz(53, 440.0), 0.2)],
    );
    let analysis = analyse_mono(&mono, RATE, Some(tuning), TunerTolerance::Standard);
    let frame = analysis.frame();
    assert_eq!(frame.strings.len(), tuning.strings.len());
    for (index, row) in frame.strings.iter().enumerate() {
        assert_eq!(row.string, index);
        assert_eq!(row.name, tuning.strings[index].0);
        assert_eq!(row.midi, tuning.strings[index].1);
    }
    assert!(frame.strings[1].sounding, "played G2 should sound");
    assert!(frame.strings[3].sounding, "played F3 should sound");
    // C2 and D4 were not played.  D4 may still show up as a harmonic of
    // G2 (294 Hz is G2's third harmonic); it must then be flagged, not
    // presented as an independent string.
    for index in [0usize, 5] {
        let row = &frame.strings[index];
        assert!(
            !row.sounding || row.harmonic_of.is_some(),
            "{} was not played but sounded independently",
            row.name
        );
    }
    assert!(!frame.strings[0].sounding, "C2 has no partial there");
}

#[test]
fn poly_reads_a_flat_bottom_string_on_an_eight_string() {
    // E1's pitch fit keeps h1 and h5 only, and at the bottom of the guitar
    // h5 sits a few hertz from G3's fundamental.  Three to five cents flat
    // can push that argmax off the sweep and silence the row, which is why
    // the presence fit (which always keeps h1) is the fallback.
    let tuning = tuning_by_id("8-string-drop-e").unwrap();
    for detune in [-5.0f32, -4.0, -3.0] {
        let detunes = [detune, 2.0, -1.0, 1.0, 0.0, 2.0, -2.0, 1.0];
        let mono = chord(RATE, frames_for(RATE), &chord_spec(tuning, &detunes));
        let analysis = analyse_mono(&mono, RATE, Some(tuning), TunerTolerance::Standard);
        let frame = analysis.frame();
        let e1 = &frame.strings[0];
        assert!(e1.sounding, "E1 at {detune:+} must sound");
        assert!(
            (e1.cents - detune).abs() <= 5.0,
            "E1 at {detune:+} read {:.2} cents",
            e1.cents
        );
        for (index, expected) in detunes.iter().enumerate().skip(1) {
            let row = &frame.strings[index];
            assert!(row.sounding, "{} must sound", row.name);
            assert!(
                (row.cents - expected).abs() <= 5.0,
                "{} reads {:.2} cents, injected {expected}",
                row.name,
                row.cents
            );
            assert!(
                row.harmonic_of.is_none(),
                "{} must stay independent",
                row.name
            );
        }
    }
}

#[test]
fn a_played_high_e_is_not_a_ghost_of_the_low_one() {
    let tuning = tuning_by_id("8-string-drop-e").unwrap();
    // E1 and E4 together.  E4's fundamental is E1's fourth harmonic and
    // E4 is the quieter string, which is exactly the shape the ghost rule
    // looks for; its own upper partials are clean, so it must stay an
    // independent row.
    let mono = chord(
        RATE,
        frames_for(RATE),
        &[
            (midi_to_hz(28, 440.0), 0.18),
            (midi_to_hz(64, 440.0), 0.073),
        ],
    );
    let analysis = analyse_mono(&mono, RATE, Some(tuning), TunerTolerance::Standard);
    let frame = analysis.frame();
    assert!(frame.strings[0].sounding, "E1 should sound");
    let e4 = &frame.strings[7];
    assert!(e4.sounding, "E4 should sound");
    assert_eq!(
        e4.harmonic_of, None,
        "a played E4 carries its own partials and is not E1's ghost"
    );
}

#[test]
fn poly_flags_c3_as_a_c2_harmonic() {
    let tuning = tuning_by_id("drop-c").unwrap();
    let mono = tone(
        midi_to_hz(36, 440.0),
        RATE,
        0.18,
        frames_for(RATE),
        HARMONICS,
    );
    let analysis = analyse_mono(&mono, RATE, Some(tuning), TunerTolerance::Standard);
    let frame = analysis.frame();
    assert!(frame.strings[0].sounding, "C2 should sound");
    // G2, F3 and A3 have no partial where their fundamentals sit for a
    // lone C2, so they are explicitly silent.
    for index in [1usize, 3, 4] {
        assert!(
            !frame.strings[index].sounding,
            "{} must stay silent for a lone C2",
            frame.strings[index].name
        );
    }
    // C3's fundamental is C2's second harmonic.  Its detune is not
    // meaningful (the clean-partial fit has no evidence, so it falls back
    // to the presence fit and can land anywhere), so this only asserts the
    // flag, never a cents value.
    let c3 = &frame.strings[2];
    assert!(
        !c3.sounding || c3.harmonic_of == Some(0),
        "C3 must never be offered as an independent string"
    );
}

#[test]
fn poly_flags_an_exaggerated_c2_harmonic() {
    let tuning = tuning_by_id("drop-c").unwrap();
    let c2 = midi_to_hz(36, 440.0);
    // The second partial is raised from the natural 0.5x to 0.55x of the
    // fundamental: loud enough that the C3 row clears every sounding gate,
    // still under the 0.6 ratio the ghost rule uses.
    let mono = tone_with(
        c2,
        RATE,
        frames_for(RATE),
        &[0.18, 0.099, 0.06, 0.045, 0.036, 0.03],
    );
    let analysis = analyse_mono(&mono, RATE, Some(tuning), TunerTolerance::Standard);
    let frame = analysis.frame();
    assert!(frame.strings[0].sounding, "C2 should sound");
    let c3 = &frame.strings[2];
    assert!(c3.sounding, "the boosted harmonic should reach the C3 row");
    assert_eq!(
        c3.harmonic_of,
        Some(0),
        "C3 must be flagged, not offered as a string"
    );

    // A second partial as loud as the fundamental is what a real C3 looks
    // like, so the heuristic must stop claiming a ghost there.
    let mono = tone_with(
        c2,
        RATE,
        frames_for(RATE),
        &[0.18, 0.18, 0.06, 0.045, 0.036, 0.03],
    );
    let analysis = analyse_mono(&mono, RATE, Some(tuning), TunerTolerance::Standard);
    let c3 = &analysis.frame().strings[2];
    assert!(c3.sounding);
    assert_eq!(
        c3.harmonic_of, None,
        "a loud h2 is not distinguishable from a real C3"
    );
}

#[test]
fn poly_flags_e4_and_b3_as_e2_harmonics() {
    let tuning = tuning_by_id("e-standard").unwrap();
    let mono = tone(
        midi_to_hz(40, 440.0),
        RATE,
        0.18,
        frames_for(RATE),
        HARMONICS,
    );
    let analysis = analyse_mono(&mono, RATE, Some(tuning), TunerTolerance::Standard);
    let frame = analysis.frame();
    assert!(frame.strings[0].sounding, "E2 should sound");
    // B3 lands on E2's third harmonic: it sounds and is flagged.
    let b3 = &frame.strings[4];
    assert!(b3.sounding, "B3 is E2's third harmonic here");
    assert_eq!(b3.harmonic_of, Some(0), "B3 must be flagged");
    // E4's own fundamental is E2's fourth harmonic and it has no clean
    // partial energy in this signal, so the row is a ghost of E2 too.
    let e4 = &frame.strings[5];
    assert!(e4.sounding, "E4 reaches its own row through E2's series");
    assert_eq!(e4.harmonic_of, Some(0), "E4 must be flagged");
    for index in [1usize, 2, 3] {
        assert!(
            !frame.strings[index].sounding,
            "{} must not sound",
            frame.strings[index].name
        );
    }
}

#[test]
fn poly_rejects_edge_pinned_rows() {
    let tuning = tuning_by_id("drop-c").unwrap();
    // C2 seventy cents sharp: inside the string's own lobe, outside the
    // ±60 cents a row is allowed to show, so the score peaks at the edge.
    let mono = tone(
        midi_to_hz(36, 440.0) * cents_to_ratio(70.0),
        RATE,
        0.2,
        frames_for(RATE),
        HARMONICS,
    );
    let analysis = analyse_mono(&mono, RATE, Some(tuning), TunerTolerance::Standard);
    let row = &analysis.frame().strings[0];
    assert!(
        row.cents.abs() >= EDGE_CENTS,
        "the peak should be pinned, got {:.2}",
        row.cents
    );
    assert!(!row.sounding, "an edge-pinned row is not a reading");
}

#[test]
fn poly_smoothing_does_not_jump() {
    let tuning = tuning_by_id("drop-c").unwrap();
    let frames = frames_for(RATE);
    let spec = |cents: f32| -> Vec<(f32, f32)> {
        tuning
            .strings
            .iter()
            .enumerate()
            .map(|(index, (_, midi))| {
                (
                    midi_to_hz(*midi, 440.0) * cents_to_ratio(if index == 0 { cents } else { 0.0 }),
                    0.18 / ((index + 1) as f32).sqrt(),
                )
            })
            .collect()
    };
    let mut analysis = analyse_mono(
        &chord(RATE, frames, &spec(0.0)),
        RATE,
        Some(tuning),
        TunerTolerance::Standard,
    );
    assert!(analysis.frame().strings[0].sounding);
    // A second frame inside the band gives the string a history to blend
    // with; the third is the one that jumps.
    for cents in [0.0f32, 20.0] {
        let interleaved = interleave(&chord(RATE, frames, &spec(cents)));
        analysis.analyse(
            &interleaved,
            frames,
            RATE,
            Some(tuning),
            TunerChannel::Sum,
            440.0,
            TunerTolerance::Standard,
        );
    }
    let row = &analysis.frame().strings[0];
    assert!(row.sounding);
    assert!(
        (3.0..12.0).contains(&row.cents),
        "smoothing should move about a third of the way toward 20 cents, got {:.2}",
        row.cents
    );
}

#[test]
fn preset_change_resets_poly_history() {
    let standard = tuning_by_id("e-standard").unwrap();
    let drop_d = tuning_by_id("drop-d").unwrap();
    let frames = frames_for(RATE);
    let play = |cents: f32| {
        interleave(&tone(
            midi_to_hz(45, 440.0) * cents_to_ratio(cents),
            RATE,
            0.2,
            frames,
            HARMONICS,
        ))
    };
    let analyse = |analysis: &mut TunerAnalysis, buffer: &[f32], tuning: &'static Tuning| {
        analysis.analyse(
            buffer,
            frames,
            RATE,
            Some(tuning),
            TunerChannel::Sum,
            440.0,
            TunerTolerance::Standard,
        );
    };
    // A2 is index 1 in both presets and both have six strings, so only the
    // preset id can tell the change apart.

    // The latch must not carry over.
    let in_tune = play(0.0);
    let mut analysis = TunerAnalysis::new();
    for _ in 0..3 {
        analyse(&mut analysis, &in_tune, standard);
    }
    assert!(matches!(
        analysis.frame().strings[1].state,
        TuneState::InTune | TuneState::Centered
    ));
    analyse(&mut analysis, &in_tune, drop_d);
    assert!(
        !matches!(
            analysis.frame().strings[1].state,
            TuneState::InTune | TuneState::Centered
        ),
        "the verdict must not carry over to another preset"
    );

    // The smoothing history must not carry over either.  Twenty
    // cents sharp settles the A2 row; switching presets and playing the
    // same string twenty flat must snap to the new reading, not blend 35%
    // of the way from the old one (which would read about +6 cents).
    let sharp = play(20.0);
    let flat = play(-20.0);
    let mut analysis = TunerAnalysis::new();
    for _ in 0..3 {
        analyse(&mut analysis, &sharp, standard);
    }
    assert!((analysis.frame().strings[1].cents - 20.0).abs() <= 1.0);
    analyse(&mut analysis, &flat, drop_d);
    assert!(
        (analysis.frame().strings[1].cents + 20.0).abs() <= 1.5,
        "preset change must drop the smoothing history, got {:.2}",
        analysis.frame().strings[1].cents
    );
}

#[test]
fn harmonic_flag_matches_integer_multiples() {
    for multiple in 2..=8u32 {
        assert!(
            harmonic_of(55.0 * multiple as f32, 55.0),
            "{multiple}x must count as a harmonic"
        );
    }
    assert!(harmonic_of(midi_to_hz(64, 440.0), midi_to_hz(40, 440.0)));
    assert!(harmonic_of(midi_to_hz(59, 440.0), midi_to_hz(40, 440.0)));
    assert!(!harmonic_of(midi_to_hz(57, 440.0), midi_to_hz(40, 440.0)));
    assert!(!harmonic_of(120.0, 100.0));
    assert!(!harmonic_of(50.0, 100.0));
}

#[test]
fn preview_produces_a_valid_chromatic_frame() {
    let mut analysis = TunerAnalysis::new();
    analysis.preview(None, 440.0, TunerTolerance::Standard, 0.0);
    let reading = &analysis.frame().mono;
    assert!(reading.valid);
    assert!(reading.frequency.is_finite());
    assert!(reading.target_hz.is_finite());
    assert!(reading.cents.is_finite());
    assert_eq!(reading.midi, 45);
    assert!((reading.target_hz - 110.0).abs() < 0.1);
    assert!((reading.cents - 3.0).abs() <= 1.0);
    assert_ne!(reading.state, TuneState::Silent);
}

#[test]
fn preview_mono_follows_the_lowest_string() {
    let tuning = tuning_by_id("drop-c").unwrap();
    let mut analysis = TunerAnalysis::new();
    analysis.preview(Some(tuning), 440.0, TunerTolerance::Standard, 0.0);
    let frame = analysis.frame();
    assert!(frame.mono.valid, "the mono stage must hear the string");
    assert_eq!(frame.mono.string, Some(0));
    assert_eq!(frame.mono.midi, 36);
    assert!((frame.mono.target_hz - 65.406).abs() < 0.01);
    assert!(
        (frame.mono.cents + 12.4).abs() <= 1.0,
        "the lowest string is synthesized 12.4 cents flat, read {:.2}",
        frame.mono.cents
    );
    // The chord still reaches the poly rows.
    assert_eq!(frame.strings.len(), 6);
    assert!(frame.strings.iter().filter(|row| row.sounding).count() >= 5);
}

#[test]
fn preview_synthesises_the_preset_rows() {
    let tuning = tuning_by_id("drop-c").unwrap();
    // At elapsed 0 the wobble is zero, so the rows must come back on the
    // exact detunes the preview synthesized.
    let mut analysis = TunerAnalysis::new();
    analysis.preview(Some(tuning), 440.0, TunerTolerance::Standard, 0.0);
    let frame = analysis.frame();
    assert_eq!(frame.strings.len(), PREVIEW_DETUNES.len());
    for (row, expected) in frame.strings.iter().zip(PREVIEW_DETUNES) {
        assert!(row.cents.is_finite());
        assert!(row.target_hz.is_finite());
        assert!(row.confidence.is_finite());
        assert!(row.sounding, "preview {} should sound", row.name);
        assert!(
            (row.cents - expected).abs() <= 3.0,
            "preview {} reads {:.2} cents, synthesized {expected}",
            row.name,
            row.cents
        );
    }
}

#[test]
fn preview_does_not_accumulate_rows() {
    let tuning = tuning_by_id("drop-c").unwrap();
    let mut analysis = TunerAnalysis::new();
    analysis.preview(Some(tuning), 440.0, TunerTolerance::Standard, 0.0);
    let first = analysis.frame().strings.len();
    analysis.preview(Some(tuning), 440.0, TunerTolerance::Standard, 0.04);
    assert_eq!(first, PREVIEW_DETUNES.len());
    assert_eq!(analysis.frame().strings.len(), first);
}

#[test]
fn demo_matches_the_drop_c_frame() {
    let mut analysis = TunerAnalysis::new();
    analysis.demo(None, 440.0, TunerTolerance::Standard);
    let frame = analysis.frame();
    // C3 at -0.8 is inside the permanent ±1 mark, so it reads Centered,
    // not InTune.  With C2 flat and F3 sharp the frame is 4 of 6 in tune.
    let expected = [
        ("C2", TuneState::Flat),
        ("G2", TuneState::InTune),
        ("C3", TuneState::Centered),
        ("F3", TuneState::Sharp),
        ("A3", TuneState::InTune),
        ("D4", TuneState::InTune),
    ];
    assert_eq!(frame.strings.len(), expected.len());
    for ((row, (name, state)), cents) in frame.strings.iter().zip(expected).zip(PREVIEW_DETUNES) {
        assert_eq!(row.name, name);
        assert!((row.cents - cents).abs() < 0.01);
        assert_eq!(row.state, state, "{name}");
        assert!(row.sounding);
        assert!(row.harmonic_of.is_none());
    }
    let in_tune = |frame: &TunerFrame| {
        frame
            .strings
            .iter()
            .filter(|row| matches!(row.state, TuneState::InTune | TuneState::Centered))
            .count()
    };
    assert_eq!(in_tune(frame), 4);
    // The states come from the live band rules, so a wider tolerance
    // admits F3 and a tighter one stays at four.
    let mut live = TunerAnalysis::new();
    live.demo(None, 440.0, TunerTolerance::Live);
    assert_eq!(in_tune(live.frame()), 5);
    let mut tight = TunerAnalysis::new();
    tight.demo(None, 440.0, TunerTolerance::Tight);
    // At ±3, A3 (-3.4) is no longer in tune either.
    assert_eq!(in_tune(tight.frame()), 3);

    assert!(frame.mono.valid);
    assert_eq!(frame.mono.string, Some(0));
    assert!((frame.mono.target_hz - 65.406).abs() < 0.01);
    assert!((frame.mono.frequency - 64.94).abs() < 0.05);
}

#[test]
fn degenerate_input_is_finite_and_silent() {
    let mut analysis = TunerAnalysis::new();
    analysis.analyse(
        &[],
        0,
        0,
        None,
        TunerChannel::Sum,
        440.0,
        TunerTolerance::Standard,
    );
    assert!(!analysis.frame().mono.valid);
    assert!(analysis.frame().strings.is_empty());

    // Fewer frames than one window.
    let short = vec![0.1f32; 64];
    analysis.analyse(
        &short,
        32,
        48_000,
        None,
        TunerChannel::Sum,
        440.0,
        TunerTolerance::Standard,
    );
    assert!(!analysis.frame().mono.valid);

    // A slice shorter than the frame count claims.
    analysis.analyse(
        &short,
        1_000,
        48_000,
        None,
        TunerChannel::Sum,
        440.0,
        TunerTolerance::Standard,
    );
    assert!(!analysis.frame().mono.valid);

    let frame = analysis.frame();
    for value in [
        frame.mono.frequency,
        frame.mono.target_hz,
        frame.mono.cents,
        frame.mono.level_db,
    ] {
        assert!(value.is_finite());
    }
}

#[test]
fn non_finite_samples_are_rejected() {
    let frames = frames_for(RATE);
    let tuning = tuning_by_id("drop-c").unwrap();
    for poison in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let mut mono = tone(midi_to_hz(36, 440.0), RATE, 0.3, frames, HARMONICS);
        mono[frames / 2] = poison;
        let analysis = analyse_mono(&mono, RATE, Some(tuning), TunerTolerance::Standard);
        let frame = analysis.frame();
        assert!(!frame.mono.valid, "{poison} must not produce a reading");
        assert_eq!(frame.mono.state, TuneState::Silent);
        assert!(frame.strings.is_empty(), "{poison} must clear the rows");
    }
    // The decimator is restarted, so the next clean window reads again.
    let mut analysis = TunerAnalysis::new();
    let mut poisoned = tone(midi_to_hz(36, 440.0), RATE, 0.3, frames, HARMONICS);
    poisoned[frames / 2] = f32::NAN;
    let buffer = interleave(&poisoned);
    analysis.analyse(
        &buffer,
        frames,
        RATE,
        Some(tuning),
        TunerChannel::Sum,
        440.0,
        TunerTolerance::Standard,
    );
    assert!(!analysis.frame().mono.valid);
    let clean = interleave(&tone(midi_to_hz(36, 440.0), RATE, 0.3, frames, HARMONICS));
    analysis.analyse(
        &clean,
        frames,
        RATE,
        Some(tuning),
        TunerChannel::Sum,
        440.0,
        TunerTolerance::Standard,
    );
    assert!(analysis.frame().mono.valid, "a clean window must recover");
}

#[test]
fn tolerance_and_channel_round_trip() {
    for tolerance in TunerTolerance::ALL {
        assert_eq!(TunerTolerance::from_u8(tolerance.to_u8()), tolerance);
        assert!(tolerance.cents() > 0.0);
        assert!(tolerance.label().starts_with('±'));
    }
    assert_eq!(TunerTolerance::from_u8(9), TunerTolerance::Standard);
    for channel in TunerChannel::ALL {
        assert_eq!(TunerChannel::from_u8(channel.to_u8()), channel);
    }
    assert_eq!(TunerChannel::from_u8(9), TunerChannel::Sum);
}
