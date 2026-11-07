use super::{
    db_to_amp, pole_coeff, GatePhase, GateSettings, GateSlot, NoiseGate, DETECTOR_ATTACK_MS,
    DETECTOR_RELEASE_MS,
};

const RATE: u32 = 48_000;

fn loud_gate() -> NoiseGate {
    let mut gate = NoiseGate::new(RATE);
    gate.update(
        GateSettings {
            enabled: true,
            threshold_db: -40.0,
            range_db: -60.0,
            hysteresis_db: 6.0,
            attack_ms: 1.0,
            release_ms: 50.0,
            hold_ms: 20.0,
            slot: GateSlot::Post,
        },
        RATE,
    );
    gate
}

fn run(gate: &mut NoiseGate, amp: f32, samples: usize) {
    for _ in 0..samples {
        let _ = gate.process(amp, amp);
    }
}

#[test]
fn db_conversion_matches_amplitude() {
    assert!((db_to_amp(0.0) - 1.0).abs() < 1.0e-6);
    assert!((db_to_amp(-20.0) - 0.1).abs() < 1.0e-6);
    assert!((db_to_amp(-40.0) - 0.01).abs() < 1.0e-6);
    assert!((db_to_amp(-60.0) - 0.001).abs() < 1.0e-6);
}

#[test]
fn pole_coeff_is_one_at_zero_time_and_decays() {
    // One sample time constant → e^{-1}.
    let c = pole_coeff(1.0 / RATE as f32, RATE);
    assert!((c - (-1.0f32).exp()).abs() < 1.0e-5);
    // Longer τ → closer to 1 (slower).
    assert!(pole_coeff(0.1, RATE) > pole_coeff(0.01, RATE));
}

#[test]
fn disabled_gate_is_unity() {
    let mut gate = NoiseGate::new(RATE);
    let (l, r) = gate.process(0.5, -0.25);
    assert_eq!((l, r), (0.5, -0.25));
    assert!(!gate.active());
}

#[test]
fn silence_stays_at_the_range_floor() {
    let mut gate = loud_gate();
    run(&mut gate, 0.0, RATE as usize / 10);
    assert_eq!(gate.phase(), GatePhase::Closed);
    let floor = db_to_amp(-60.0);
    assert!(
        (gate.gain() - floor).abs() < 1.0e-4,
        "gain {} vs floor {}",
        gate.gain(),
        floor
    );
    let (l, _) = gate.process(0.0, 0.0);
    assert!(l.abs() < 1.0e-9);
}

#[test]
fn a_loud_signal_opens_toward_unity() {
    let mut gate = loud_gate();
    // Well above −40 dBFS open threshold (0.01).
    run(&mut gate, 0.25, RATE as usize / 50);
    assert_eq!(gate.phase(), GatePhase::Open);
    assert!(gate.gain() > 0.95, "gain {}", gate.gain());
    let (l, r) = gate.process(0.25, 0.25);
    assert!((l - 0.25 * gate.gain()).abs() < 1.0e-6);
    assert!((r - 0.25 * gate.gain()).abs() < 1.0e-6);
}

#[test]
fn hysteresis_keeps_a_closed_gate_closed_between_thresholds() {
    let mut gate = loud_gate();
    // Open at −40 dB (0.01), close at −46 dB (~0.005).  Midway ≈ 0.007.
    let mid = db_to_amp(-43.0);
    run(&mut gate, mid, RATE as usize / 10);
    assert_eq!(gate.phase(), GatePhase::Closed);
    assert!(gate.gain() < 0.05);
}

#[test]
fn hysteresis_keeps_an_open_gate_open_between_thresholds() {
    let mut gate = loud_gate();
    run(&mut gate, 0.25, RATE as usize / 50);
    assert_eq!(gate.phase(), GatePhase::Open);
    let mid = db_to_amp(-43.0);
    // Stay in the hysteresis band long enough that a no-hysteresis gate
    // would have closed; ours must remain Open (or Holding only after
    // dropping below close — mid is above close).
    run(&mut gate, mid, RATE as usize / 10);
    assert_eq!(gate.phase(), GatePhase::Open);
    assert!(gate.gain() > 0.9);
}

/// Silence long enough for the detector envelope to fall below the close
/// threshold (−46 dB from a 0.25 peak with 20 ms detector release).
fn wait_until_holding(gate: &mut NoiseGate) {
    // ~100 ms of silence is several detector time constants below close.
    for _ in 0..(RATE as usize / 10) {
        let _ = gate.process(0.0, 0.0);
        if gate.phase() == GatePhase::Holding || gate.phase() == GatePhase::Closed {
            break;
        }
    }
}

#[test]
fn hold_delays_the_close_after_the_signal_drops() {
    let mut gate = loud_gate();
    run(&mut gate, 0.25, RATE as usize / 50);
    assert_eq!(gate.phase(), GatePhase::Open);
    wait_until_holding(&mut gate);
    assert_eq!(gate.phase(), GatePhase::Holding);
    // Hold is 20 ms (= 960 samples). Finish that, then a few more.
    run(&mut gate, 0.0, 1_200);
    assert_eq!(gate.phase(), GatePhase::Closed);
}

#[test]
fn signal_returning_during_hold_reopens() {
    let mut gate = loud_gate();
    run(&mut gate, 0.25, RATE as usize / 50);
    wait_until_holding(&mut gate);
    assert_eq!(gate.phase(), GatePhase::Holding);
    run(&mut gate, 0.25, 50);
    assert_eq!(gate.phase(), GatePhase::Open);
}

#[test]
fn gain_release_follows_the_time_constant_once_closed() {
    let mut gate = NoiseGate::new(RATE);
    gate.update(
        GateSettings {
            enabled: true,
            hold_ms: 0.0,
            release_ms: 50.0,
            attack_ms: 1.0,
            threshold_db: -40.0,
            range_db: -60.0,
            hysteresis_db: 6.0,
            slot: GateSlot::Post,
        },
        RATE,
    );
    run(&mut gate, 0.5, RATE as usize / 20);
    assert_eq!(gate.phase(), GatePhase::Open);
    let open_gain = gate.gain();
    assert!(open_gain > 0.95);

    // Wait until the detector has fallen and the phase is Closed, then
    // measure one release τ of gain motion from a known starting gain.
    wait_until_holding(&mut gate);
    // hold_ms is 0, so Closing is immediate once the envelope is low.
    assert_eq!(gate.phase(), GatePhase::Closed);
    // Snap the smoothed gain back to the open value so the release τ is
    // measured cleanly (envelope already below close).
    gate.gain = open_gain;
    let release_samples = (0.050 * RATE as f32).round() as usize;
    run(&mut gate, 0.0, release_samples);
    let after_release = gate.gain();
    let floor = db_to_amp(-60.0);
    let expected = floor + (open_gain - floor) / std::f32::consts::E;
    assert!(
        (after_release - expected).abs() < 0.05,
        "release {after_release} vs ~{expected}"
    );
}

#[test]
fn attack_rises_most_of_the_way_in_one_time_constant() {
    let mut gate = loud_gate();
    let attack_samples = (0.001 * RATE as f32).round() as usize;
    run(&mut gate, 0.5, attack_samples);
    // From the floor (~0.001) toward 1, one τ ≈ 63% of the gap → ~0.63.
    assert!(
        gate.gain() > 0.55,
        "after one attack τ gain should be well up, got {}",
        gate.gain()
    );
}

#[test]
fn stereo_link_uses_the_louder_channel() {
    let mut gate = loud_gate();
    // Only the right channel is loud.
    for _ in 0..RATE as usize / 50 {
        let _ = gate.process(0.0, 0.25);
    }
    assert_eq!(gate.phase(), GatePhase::Open);
    assert!(gate.envelope() > db_to_amp(-40.0));
}

#[test]
fn default_slot_is_post() {
    assert_eq!(GateSettings::default().slot, GateSlot::Post);
}

#[test]
fn sanitized_clamps_out_of_range_knobs() {
    let s = GateSettings {
        threshold_db: -200.0,
        range_db: 10.0,
        hysteresis_db: 100.0,
        attack_ms: 0.0,
        release_ms: 9_999.0,
        hold_ms: -5.0,
        ..GateSettings::default()
    }
    .sanitized();
    assert_eq!(s.threshold_db, super::MIN_THRESHOLD_DB);
    assert_eq!(s.range_db, super::MAX_RANGE_DB);
    assert_eq!(s.hysteresis_db, super::MAX_HYSTERESIS_DB);
    assert_eq!(s.attack_ms, super::MIN_ATTACK_MS);
    assert_eq!(s.release_ms, super::MAX_RELEASE_MS);
    assert_eq!(s.hold_ms, super::MIN_HOLD_MS);
}

#[test]
fn sanitized_replaces_non_finite_knobs() {
    let defaults = GateSettings::default();
    let s = GateSettings {
        threshold_db: f32::NAN,
        range_db: f32::INFINITY,
        hysteresis_db: f32::NEG_INFINITY,
        attack_ms: f32::NAN,
        release_ms: f32::INFINITY,
        hold_ms: f32::NAN,
        ..defaults
    }
    .sanitized();
    assert_eq!(s.threshold_db, defaults.threshold_db);
    assert_eq!(s.range_db, defaults.range_db);
    assert_eq!(s.hysteresis_db, defaults.hysteresis_db);
    assert_eq!(s.attack_ms, defaults.attack_ms);
    assert_eq!(s.release_ms, defaults.release_ms);
    assert_eq!(s.hold_ms, defaults.hold_ms);
}

#[test]
#[allow(clippy::assertions_on_constants)]
fn detector_constants_are_sensible() {
    assert!(DETECTOR_ATTACK_MS < DETECTOR_RELEASE_MS);
    assert!(DETECTOR_ATTACK_MS <= 1.0);
}
