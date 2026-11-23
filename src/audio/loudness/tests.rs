use super::*;

const RATE: u32 = 48_000;

fn meter(channels: usize) -> LoudnessMeter {
    let mut meter = LoudnessMeter::new(RATE);
    meter.configure(RATE, channels);
    meter
}

/// Feeds `seconds` of a 1 kHz sine at `amplitude` peak on every channel.
fn feed_sine(meter: &mut LoudnessMeter, seconds: f32, amplitude: f32, channels: usize) {
    let frames = (RATE as f32 * seconds) as usize;
    let step = 2.0 * std::f64::consts::PI * 1000.0 / RATE as f64;
    let mut phase = 0.0f64;
    for _ in 0..frames {
        let sample = phase.sin() as f32 * amplitude;
        let frame = [sample; MAX_OUTPUT_CHANNELS];
        meter.push(&frame[..channels]);
        phase += step;
        if phase > std::f64::consts::TAU {
            phase -= std::f64::consts::TAU;
        }
    }
}

fn assert_close(got: f32, expected: f32) {
    assert!(
        (got - expected).abs() < 0.15,
        "expected {expected:.2} LUFS, got {got:.2}"
    );
}

#[test]
fn a_minus_twenty_sine_reads_minus_twenty_three_lufs() {
    let mut meter = meter(1);
    feed_sine(&mut meter, 4.0, 0.1, 1);
    let reading = meter.reading(0.0);
    assert_close(reading.momentary_lufs, -23.0);
    assert_close(reading.short_term_lufs, -23.0);
    assert_close(reading.integrated_lufs, -23.0);
}

#[test]
fn a_full_scale_sine_reads_about_minus_three_lufs() {
    let mut meter = meter(1);
    feed_sine(&mut meter, 4.0, 1.0, 1);
    assert_close(meter.reading(0.0).short_term_lufs, -3.0);
}

#[test]
fn two_equal_channels_are_three_lu_louder_than_one() {
    let mut mono = meter(1);
    let mut stereo = meter(2);
    feed_sine(&mut mono, 4.0, 0.1, 1);
    feed_sine(&mut stereo, 4.0, 0.1, 2);
    let difference = stereo.reading(0.0).short_term_lufs - mono.reading(0.0).short_term_lufs;
    assert!(
        (difference - 3.01).abs() < 0.1,
        "stereo should add 3.01 LU, got {difference:.2}"
    );
}

#[test]
fn every_rate_reads_the_same_loudness() {
    for rate in [44_100, 48_000, 96_000, 192_000] {
        let mut meter = LoudnessMeter::new(rate);
        meter.configure(rate, 1);
        let frames = rate as usize * 4;
        let step = 2.0 * std::f64::consts::PI * 1000.0 / rate as f64;
        let mut phase = 0.0f64;
        for _ in 0..frames {
            let sample = phase.sin() as f32 * 0.1;
            meter.push(&[sample]);
            phase += step;
            if phase > std::f64::consts::TAU {
                phase -= std::f64::consts::TAU;
            }
        }
        assert_close(meter.reading(0.0).short_term_lufs, -23.0);
    }
}

#[test]
fn silence_reads_silence_not_a_number() {
    let mut meter = meter(2);
    for _ in 0..(RATE as usize * 4) {
        meter.push(&[0.0, 0.0]);
    }
    let reading = meter.reading(0.0);
    assert_eq!(reading.short_term_lufs, SILENCE_LUFS);
    assert!(!reading.has_reading());
}

#[test]
fn the_relative_gate_ignores_a_quiet_tail() {
    let mut meter = meter(1);
    feed_sine(&mut meter, 2.0, 0.1, 1);
    feed_sine(&mut meter, 2.0, 0.01, 1);
    // The tail is about -43 LUFS: above the absolute gate, but more than
    // 10 LU below the loud block, so only the relative gate removes it.  An
    // ungated mean of both halves would read about -26 LUFS.
    let integrated = meter.reading(0.0).integrated_lufs;
    assert!(
        (-23.6..=-22.8).contains(&integrated),
        "gated integrated was {integrated:.2} LUFS"
    );
}

#[test]
fn a_reset_forgets_the_windows() {
    let mut meter = meter(1);
    feed_sine(&mut meter, 4.0, 0.1, 1);
    assert!(meter.reading(0.0).has_reading());
    meter.reset();
    let reading = meter.reading(0.0);
    assert_eq!(reading.momentary_lufs, SILENCE_LUFS);
    assert_eq!(reading.short_term_lufs, SILENCE_LUFS);
    assert_eq!(reading.integrated_lufs, SILENCE_LUFS);
}

#[test]
fn the_history_keeps_sixty_seconds_of_blocks() {
    // One block per 100 ms, so 600 blocks is 60 s.  Feeding more must not
    // grow the reading's backing store or count blocks past its ring.
    let mut meter = meter(1);
    feed_sine(&mut meter, 61.0, 0.1, 1);
    let reading = meter.reading(0.0);
    assert_eq!(meter.block_count, HISTORY_BLOCKS);
    assert_close(reading.integrated_lufs, -23.0);
}
