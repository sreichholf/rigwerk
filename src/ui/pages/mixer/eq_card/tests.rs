use super::hz_from_log_slider;

#[test]
fn one_arrow_step_moves_a_low_frequency_by_one_hertz() {
    let hz = 40.0;
    let up = hz_from_log_slider(hz, hz.ln() + 0.001, 20.0, 20_000.0);
    let down = hz_from_log_slider(hz, hz.ln() - 0.001, 20.0, 20_000.0);
    assert_eq!(up, 41.0);
    assert_eq!(down, 39.0);
}

#[test]
fn a_slider_move_that_crosses_a_hertz_keeps_the_rounded_value() {
    let hz = 1_000.0;
    let next = hz_from_log_slider(hz, 1_008.4_f32.ln(), 20.0, 20_000.0);
    assert_eq!(next, 1_008.0);
}

#[test]
fn the_frequency_slider_stays_inside_the_band() {
    assert_eq!(
        hz_from_log_slider(20.0, 20.0_f32.ln() - 0.001, 20.0, 400.0),
        20.0
    );
    assert_eq!(
        hz_from_log_slider(400.0, 400.0_f32.ln() + 0.001, 20.0, 400.0),
        400.0
    );
}
