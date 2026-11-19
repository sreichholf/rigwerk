use super::{control_value, state_word, NO_CONTROLS, OWN_CONTROLS, STATE_ACTIVE};
use crate::audio::PluginParamView;

fn parameter(display: &str, unit: &str, value: f64) -> PluginParamView {
    PluginParamView {
        id: 1,
        name: "Gain".to_owned(),
        value,
        default: 0.5,
        step_count: 0,
        unit: unit.to_owned(),
        display: display.to_owned(),
        read_only: false,
    }
}

#[test]
fn the_state_word_names_a_failed_effect_before_its_bypass() {
    assert_eq!(state_word(false, false), STATE_ACTIVE);
    assert_eq!(state_word(true, false), "Bypassed");
    assert_eq!(state_word(false, true), "Didn't load");
    assert_eq!(state_word(true, true), "Didn't load");
}

#[test]
fn a_control_prefers_the_plugin_own_display() {
    assert_eq!(
        control_value(&parameter("440.00 Hz", "Hz", 0.25)),
        "440.00 Hz"
    );
    assert_eq!(control_value(&parameter("", "", 0.25)), "25 %");
    assert_eq!(control_value(&parameter("", "dB", 0.25)), "0.25 dB");
}

#[test]
fn the_effect_body_texts_name_what_it_shows() {
    assert!(NO_CONTROLS.contains("no controls"));
    assert!(OWN_CONTROLS.contains("draws its own"));
}
