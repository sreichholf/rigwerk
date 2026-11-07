use super::{EqSettings, Equalizer, MAX_Q, MIN_Q};

fn settings_with(band: usize, gain_db: f32) -> EqSettings {
    let mut settings = EqSettings::default();
    settings.bands[band].gain_db = gain_db;
    settings
}

/// Feeds a steady sine through the EQ and returns the output level in dB
/// relative to the input.
fn measured_gain_db(settings: EqSettings, hz: f32) -> f32 {
    let rate = 48_000;
    let mut eq = Equalizer::new(rate);
    eq.update(settings, rate);
    let mut peak = 0.0f32;
    for n in 0..rate as usize {
        let x = (2.0 * std::f32::consts::PI * hz * n as f32 / rate as f32).sin();
        let (y, _) = eq.process(x, x);
        if n > rate as usize / 2 {
            peak = peak.max(y.abs());
        }
    }
    20.0 * peak.log10()
}

#[test]
fn flat_eq_changes_nothing() {
    let settings = EqSettings::default();
    assert!(!settings.is_active());
    let mut eq = Equalizer::new(48_000);
    assert_eq!(eq.process(0.25, -0.5), (0.25, -0.5));
}

#[test]
fn mid_bell_boosts_its_own_frequency_by_its_gain() {
    let gain = measured_gain_db(settings_with(2, 9.0), 800.0);
    assert!((gain - 9.0).abs() < 0.3, "{gain}");
    // Far away from the bell the level is untouched.
    let far = measured_gain_db(settings_with(2, 9.0), 60.0);
    assert!(far.abs() < 0.5, "{far}");
}

#[test]
fn shelves_reach_their_gain_beyond_the_corner() {
    let low = measured_gain_db(settings_with(0, -12.0), 30.0);
    assert!((low + 12.0).abs() < 1.0, "{low}");
    let high = measured_gain_db(settings_with(4, 6.0), 15_000.0);
    assert!((high - 6.0).abs() < 1.0, "{high}");
}

#[test]
fn drawn_curve_matches_what_is_heard() {
    let settings = settings_with(3, -6.0);
    let drawn = settings.response_db(2_500.0, 48_000);
    let heard = measured_gain_db(settings, 2_500.0);
    assert!((drawn - heard).abs() < 0.3, "{drawn} vs {heard}");
}

#[test]
fn width_changes_the_curve_around_a_bell() {
    let mut settings = settings_with(2, 9.0);
    settings.bands[2].q = 0.4;
    let broad = settings.response_db(1_600.0, 48_000);
    settings.bands[2].q = 4.0;
    let narrow = settings.response_db(1_600.0, 48_000);
    assert!(
        broad > narrow + 1.0,
        "wide Q should reach an octave away more than narrow Q: {broad} vs {narrow}"
    );
}

#[test]
fn width_changes_the_curve_around_a_shelf() {
    let mut wide = settings_with(0, 9.0);
    wide.bands[0].q = MIN_Q;
    let mut narrow = settings_with(0, 9.0);
    narrow.bands[0].q = MAX_Q;
    let biggest = [80.0, 100.0, 125.0, 160.0, 200.0, 250.0]
        .iter()
        .map(|hz| (wide.response_db(*hz, 48_000) - narrow.response_db(*hz, 48_000)).abs())
        .fold(0.0f32, f32::max);
    assert!(
        biggest > 1.0,
        "shelf width should change the curve: {biggest}"
    );
}

#[test]
fn combined_curve_is_the_sum_of_its_bands() {
    let mut settings = EqSettings::default();
    settings.bands[1].gain_db = 6.0;
    settings.bands[3].gain_db = -4.0;
    for hz in [80.0, 450.0, 2_500.0, 9_000.0] {
        let sum: f32 = (0..5)
            .map(|index| settings.band_response_db(index, hz, 48_000))
            .sum();
        let combined = settings.response_db(hz, 48_000);
        assert!(
            (sum - combined).abs() < 0.001,
            "{hz} Hz: {sum} vs {combined}"
        );
    }
}

#[test]
fn bypass_switches_every_band_off() {
    let mut settings = settings_with(2, 9.0);
    settings.enabled = false;
    assert!(!settings.is_active());
    let gain = measured_gain_db(settings, 800.0);
    assert!(gain.abs() < 0.05, "{gain}");
}
