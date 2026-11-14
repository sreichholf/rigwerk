use super::*;

fn started(bpm: f32, rhythm: ClickRhythm) -> Metronome {
    let mut metronome = Metronome::new();
    metronome.latch(true, bpm, rhythm, 48_000);
    metronome
}

/// Sample indices where a click fires (its voice starts on that sample).
fn fires(
    metronome: &mut Metronome,
    samples: usize,
    mut at: impl FnMut(usize, &mut Metronome),
) -> Vec<usize> {
    let mut fired = Vec::new();
    for sample in 0..samples {
        at(sample, metronome);
        let _ = metronome.next();
        if metronome.voice.on && metronome.voice.n == 1 {
            fired.push(sample);
        }
    }
    fired
}

/// Peak absolute sample of each click, in order, for `count` clicks.
fn peaks(rhythm: ClickRhythm, count: usize) -> Vec<f32> {
    let mut metronome = started(120.0, rhythm);
    let mut peaks = Vec::new();
    let mut beat = 0u8;
    let mut peak = 0.0f32;
    // One extra click so the last requested one is closed and its peak is kept.
    let samples = 24_000 * (count + 1) + 64;
    for _ in 0..samples {
        let sample = metronome.next().abs();
        if metronome.beat() != beat {
            if beat != 0 {
                peaks.push(peak);
                if peaks.len() == count {
                    break;
                }
            }
            beat = metronome.beat();
            peak = sample;
        } else {
            peak = peak.max(sample);
        }
    }
    peaks
}

/// Samples from the start of a click until it first goes negative.
fn first_negative(rhythm: ClickRhythm, beat: u8) -> usize {
    let mut metronome = started(120.0, rhythm);
    let mut seen = 0u8;
    let mut into_click = None;
    for _ in 0..24_000 * (beat as usize + 1) {
        let sample = metronome.next();
        if metronome.beat() == beat {
            let n = into_click.get_or_insert(0);
            if sample < 0.0 {
                return *n;
            }
            *n += 1;
            seen = beat;
        } else if seen == beat {
            break;
        }
    }
    panic!("beat {beat} of {} never crossed zero", rhythm.label());
}

#[test]
fn fires_are_24000_samples_apart_at_48k_120_bpm() {
    let mut metronome = started(120.0, ClickRhythm::FourFour);
    let fired = fires(&mut metronome, 48_000, |_, _| {});
    assert!(fired.len() >= 2, "{fired:?}");
    for pair in fired.windows(2) {
        assert_eq!(pair[1] - pair[0], 24_000, "{fired:?}");
    }
    assert_eq!(fired[0], 0, "the first sample after start is a beat");
}

#[test]
fn four_four_makes_the_one_stand_out_from_the_backbeat() {
    let peaks = peaks(ClickRhythm::FourFour, 5);
    assert_eq!(peaks.len(), 5, "{peaks:?}");
    assert!(
        peaks[0] > peaks[2] * 1.5,
        "the one is well above beat 3: {} vs {}",
        peaks[0],
        peaks[2]
    );
    assert!(
        peaks[2] > peaks[1] * 1.3,
        "beat 3 sits above a plain click: {} vs {}",
        peaks[2],
        peaks[1]
    );
    assert!((peaks[1] - peaks[3]).abs() <= peaks[1] * 0.02);
    assert!(peaks[4] > peaks[2] * 1.5, "the one returns");
    let one = first_negative(ClickRhythm::FourFour, 1);
    let backbeat = first_negative(ClickRhythm::FourFour, 3);
    let plain = first_negative(ClickRhythm::FourFour, 2);
    assert!(
        one < backbeat,
        "the one is higher than beat 3: {one} vs {backbeat}"
    );
    assert!(
        backbeat < plain,
        "beat 3 is higher than a plain click: {backbeat} vs {plain}"
    );
}

#[test]
fn three_four_accents_only_the_one() {
    let peaks = peaks(ClickRhythm::ThreeFour, 4);
    assert!(peaks[0] > peaks[1] * 2.0, "{} vs {}", peaks[0], peaks[1]);
    assert!((peaks[1] - peaks[2]).abs() <= peaks[1] * 0.02);
    assert!(peaks[3] > peaks[1] * 2.0, "the one returns");
}

#[test]
fn six_eight_accents_the_one_and_the_second_group() {
    let peaks = peaks(ClickRhythm::SixEight, 7);
    assert!(peaks[0] > peaks[3] * 1.5, "{} vs {}", peaks[0], peaks[3]);
    assert!(peaks[3] > peaks[1] * 1.3, "{} vs {}", peaks[3], peaks[1]);
    for index in [2, 4, 5] {
        assert!(
            (peaks[index] - peaks[1]).abs() <= peaks[1] * 0.02,
            "beat {} matches a plain click",
            index + 1
        );
    }
    assert!(peaks[6] > peaks[3] * 1.5, "the one returns");
}

#[test]
fn every_rhythm_makes_the_one_the_loudest_click() {
    for rhythm in CLICK_RHYTHMS {
        let bar = peaks(rhythm, rhythm.clicks() as usize);
        let one = bar[0];
        for (index, peak) in bar.iter().enumerate().skip(1) {
            assert!(
                one > peak * 1.5,
                "{} beat {} is not clearly under the one: {peak} vs {one}",
                rhythm.label(),
                index + 1
            );
        }
    }
}

#[test]
fn a_rhythm_change_waits_until_the_next_beat() {
    let mut metronome = started(120.0, ClickRhythm::FourFour);
    let mut beats = Vec::new();
    let fired = fires(&mut metronome, 24_000 * 6, |sample, metronome| {
        if sample == 1_000 {
            metronome.latch(true, 120.0, ClickRhythm::ThreeFour, 48_000);
        }
    });
    let mut metronome = started(120.0, ClickRhythm::FourFour);
    for sample in 0..24_000 * 6 {
        if sample == 1_000 {
            metronome.latch(true, 120.0, ClickRhythm::ThreeFour, 48_000);
        }
        let _ = metronome.next();
        if fired.contains(&sample) {
            beats.push(metronome.beat());
        }
    }
    assert_eq!(fired[0], 0);
    assert_eq!(fired[1], 24_000, "the beat in progress keeps its length");
    assert_eq!(fired[2] - fired[1], 24_000);
    // Beat 1 of 4/4 was already sounding.  3/4 takes over at the next
    // click, on its one, so the bar that follows is three beats long.
    assert_eq!(&beats[..5], &[1, 1, 2, 3, 1], "{beats:?}");
}

#[test]
fn a_rhythm_change_starts_the_new_bar_on_the_one() {
    let mut metronome = started(120.0, ClickRhythm::FourFour);
    // Beat 1 of 4/4 fires on the first sample.
    let _ = metronome.next();
    assert_eq!(metronome.beat(), 1);
    metronome.latch(true, 120.0, ClickRhythm::ThreeFour, 48_000);
    for _ in 1..24_000 {
        let _ = metronome.next();
    }
    assert_eq!(metronome.beat(), 1, "no click yet");
    assert_eq!(metronome.rhythm(), ClickRhythm::FourFour);
    // The next click is the one of 3/4: an accent, not beat 2.
    let _ = metronome.next();
    assert_eq!(metronome.voice.n, 1, "a click fired");
    assert_eq!(metronome.rhythm(), ClickRhythm::ThreeFour);
    assert_eq!(metronome.beat(), 1);
    assert_eq!(metronome.voice.weight, ClickWeight::Accent);
    // A tempo change alone keeps counting the bar.
    metronome.latch(true, 60.0, ClickRhythm::ThreeFour, 48_000);
    for _ in 0..24_000 {
        let _ = metronome.next();
    }
    assert_eq!(metronome.beat(), 2);
    assert_eq!(metronome.voice.weight, ClickWeight::Pulse);
}

#[test]
fn a_bpm_change_waits_until_the_next_beat() {
    let mut metronome = started(120.0, ClickRhythm::FourFour);
    let fired = fires(&mut metronome, 80_000, |sample, metronome| {
        if sample == 1_000 {
            metronome.latch(true, 60.0, ClickRhythm::FourFour, 48_000);
        }
    });
    assert_eq!(fired[0], 0);
    assert_eq!(fired[1], 24_000, "the beat in progress keeps 120 BPM");
    // 60 BPM at 48 kHz is 48_000 samples.
    assert_eq!(fired[2] - fired[1], 48_000, "the next gap is the new tempo");
}

#[test]
fn the_gap_between_clicks_is_exact_zero() {
    let mut metronome = started(120.0, ClickRhythm::FourFour);
    let mut heard = false;
    let mut gap = false;
    for _ in 0..20_000 {
        let sample = metronome.next();
        if sample != 0.0 {
            heard = true;
        } else if heard {
            assert_eq!(
                sample.to_bits(),
                0,
                "a silent sample must be +0, not a denormal"
            );
            gap = true;
        }
    }
    assert!(heard, "the accent speaks");
    assert!(gap, "the voice dies away before the next beat");
}

#[test]
fn a_click_dies_below_the_floor_instead_of_cutting_at_20_ms() {
    let mut metronome = started(120.0, ClickRhythm::FourFour);
    let mut last = 0.0f32;
    let mut length = 0usize;
    let mut started_click = false;
    for index in 0..8_000 {
        let sample = metronome.next();
        if sample != 0.0 {
            started_click = true;
            last = sample.abs();
            length = index;
        } else if started_click {
            break;
        }
    }
    assert!(started_click, "the accent speaks");
    // 20 ms at 48 kHz is 960 samples.  The envelope floor is past that.
    assert!(
        length > 960,
        "the click is cut while it is still loud: {length} samples, last {last}"
    );
    assert!(
        last < 1.0e-3,
        "the tail should already be inaudible: {last}"
    );
}

#[test]
fn stopping_lets_the_click_finish() {
    let mut metronome = started(120.0, ClickRhythm::FourFour);
    let mut caught = false;
    for _ in 0..4_000 {
        let sample = metronome.next();
        if sample.abs() <= 0.5 {
            continue;
        }
        metronome.latch(false, 120.0, ClickRhythm::FourFour, 48_000);
        assert_eq!(metronome.beat(), 0, "stop clears the displayed beat");
        let mut tail_peak = 0.0f32;
        let mut last = 0.0f32;
        let mut ended = false;
        for _ in 0..8_000 {
            let tail = metronome.next();
            if tail == 0.0 {
                ended = tail_peak > 0.0;
                if ended {
                    break;
                }
            } else {
                tail_peak = tail_peak.max(tail.abs());
                last = tail.abs();
            }
        }
        assert!(tail_peak > 0.2, "stop does not cut the click: {tail_peak}");
        assert!(ended, "the tail reaches silence");
        assert!(last < 1.0e-3, "the tail ends quietly: {last}");
        caught = true;
        break;
    }
    assert!(caught, "the accent never got loud enough to stop");
}

#[test]
fn an_unknown_rhythm_byte_is_four_four() {
    assert_eq!(ClickRhythm::from_u8(0), ClickRhythm::FourFour);
    assert_eq!(ClickRhythm::from_u8(99), ClickRhythm::FourFour);
    assert_eq!(ClickRhythm::from_u8(3), ClickRhythm::SixEight);
    assert_eq!(ClickRhythm::default(), ClickRhythm::FourFour);
}

#[test]
fn a_restart_lets_the_ringing_click_finish_under_the_new_one() {
    // Stop 200 samples into the accent and start again 50 samples later,
    // while that accent still rings.  The output is the old click on its
    // own envelope plus a new click from the restart: nothing is cut.
    let render = |metronome: &mut Metronome, samples: usize| -> Vec<f32> {
        (0..samples).map(|_| metronome.next()).collect()
    };
    let stopped = |restart: bool| {
        let mut metronome = started(120.0, ClickRhythm::FourFour);
        let _ = render(&mut metronome, 200);
        metronome.latch(false, 120.0, ClickRhythm::FourFour, 48_000);
        let _ = render(&mut metronome, 50);
        if restart {
            metronome.latch(true, 120.0, ClickRhythm::FourFour, 48_000);
        }
        render(&mut metronome, 4_000)
    };
    let restarted = stopped(true);
    let tail = stopped(false);
    let fresh = render(&mut started(120.0, ClickRhythm::FourFour), 4_000);
    assert!(
        tail[..16].iter().any(|sample| sample.abs() > 0.05),
        "the old click still rings at the restart"
    );
    for (index, sample) in restarted.iter().enumerate() {
        let expected = fresh[index] + tail[index];
        assert!(
            (sample - expected).abs() < 1.0e-6,
            "sample {index}: {sample} != new {} + tail {}",
            fresh[index],
            tail[index]
        );
    }
    assert!(
        tail[3_600..].iter().all(|sample| *sample == 0.0),
        "the old click has died away"
    );
    assert_eq!(restarted[3_600..], fresh[3_600..]);
}

#[test]
fn a_rate_change_mid_click_keeps_its_time() {
    let mut reference = started(120.0, ClickRhythm::FourFour);
    let mut changed = started(120.0, ClickRhythm::FourFour);
    for _ in 0..100 {
        reference.next();
        changed.next();
    }
    // The output reopens at 96 kHz while the click still sounds.
    changed.scale(2.0);
    changed.latch(true, 120.0, ClickRhythm::FourFour, 96_000);
    assert_eq!(changed.voice.n, 2 * reference.voice.n);
    // 100 samples at 48 kHz are 200 at 96 kHz: the same point of the click.
    let expected = reference.next();
    let sample = changed.next();
    assert!(
        (sample - expected).abs() < 1.0e-4,
        "the click jumps from {expected} to {sample}"
    );
}
