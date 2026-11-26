use super::{
    body_note, control_value, editor_button, state_word, FAILED_CONTROLS, NOT_LOADED, NO_CONTROLS,
    OWN_CONTROLS, STATE_ACTIVE, STATE_BYPASSED, STATE_NOT_LOADED,
};
use crate::audio::{PluginFormat, PluginParamView, PluginView};

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

/// A published view in one of the states the sheet draws.
fn view(loaded: bool, has_editor: bool, error: Option<&str>) -> PluginView {
    view_of(PluginFormat::Vst3, loaded, has_editor, error)
}

fn view_of(
    format: PluginFormat,
    loaded: bool,
    has_editor: bool,
    error: Option<&str>,
) -> PluginView {
    PluginView {
        format,
        has_editor,
        error: error.map(str::to_owned),
        instance: loaded.then_some(1),
        ..PluginView::default()
    }
}

#[test]
fn the_state_word_names_a_failed_or_unopened_effect_first() {
    assert_eq!(state_word(false, false, true), STATE_ACTIVE);
    assert_eq!(state_word(true, false, true), STATE_BYPASSED);
    assert_eq!(state_word(false, true, true), "Didn't load");
    assert_eq!(state_word(true, true, true), "Didn't load");
    assert_eq!(state_word(false, false, false), STATE_NOT_LOADED);
    assert_eq!(state_word(false, true, false), "Didn't load");
    assert_eq!(state_word(true, false, false), STATE_NOT_LOADED);
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

#[test]
fn an_unopened_effect_offers_the_button_that_loads_it() {
    // Its editor is unknown until the plugin loads, so the button loads it
    // on the way instead of claiming the effect has none.
    assert_eq!(editor_button(&view(false, false, None)), ("Editor", true));
    // LV2 never has an editor here, but its unopened slot still needs a way
    // to load and show its controls.
    assert_eq!(
        editor_button(&view_of(PluginFormat::Lv2, false, false, None)),
        ("Controls", true)
    );
    assert_eq!(editor_button(&view(true, true, None)), ("Editor", true));
    // Once loaded, no editor means the controls button, disabled.
    assert_eq!(editor_button(&view(true, false, None)), ("Controls", false));
    // A failed load never opens an editor.
    assert_eq!(
        editor_button(&view(false, false, Some("no such file"))),
        ("Controls", false)
    );
    // A processing failure keeps the editor label, but disables it.
    assert_eq!(
        editor_button(&view(true, true, Some("processing failed"))),
        ("Editor", false)
    );
}

#[test]
fn the_body_tells_an_unloaded_effect_from_one_without_controls() {
    assert_eq!(body_note(&view(false, false, None)), NOT_LOADED);
    assert_eq!(body_note(&view(true, true, None)), OWN_CONTROLS);
    assert_eq!(body_note(&view(true, false, None)), NO_CONTROLS);
    assert_eq!(
        body_note(&view(false, false, Some("no such file"))),
        FAILED_CONTROLS
    );
    assert_eq!(
        body_note(&view(true, false, Some("processing failed"))),
        FAILED_CONTROLS
    );
}
