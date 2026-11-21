use super::*;
use crate::audio::TunerReading;

fn lead(string: Option<usize>, midi: i16) -> Lead {
    Lead {
        valid: true,
        string,
        pinned: false,
        following: None,
        midi,
        cents: 0.0,
        target_hz: 0.0,
        detected_hz: 0.0,
        level_db: -20.0,
        confidence: 0.9,
        state: TuneState::InTune,
    }
}

#[test]
fn readout_keeps_the_preset_spelling() {
    let eb = crate::audio::tuning_by_id("eb-standard").expect("Eb standard");
    assert_eq!(
        note_label(&lead(Some(0), 39), Some(eb)),
        Some(("Eb", "2".to_owned()))
    );
    assert_eq!(
        note_label(&lead(Some(1), 44), Some(eb)),
        Some(("Ab", "2".to_owned()))
    );
    assert_eq!(
        note_label(&lead(Some(2), 49), Some(eb)),
        Some(("Db", "3".to_owned()))
    );
    assert_eq!(
        note_label(&lead(None, 49), None),
        Some(("C#", "3".to_owned()))
    );
    assert_eq!(note_label(&Lead::silent(), Some(eb)), None);
}

#[test]
fn the_display_control_lists_needle_first() {
    use crate::app::tuner::TunerDisplay;
    assert_eq!(display_choice(TunerDisplay::Needle), 0);
    assert_eq!(display_choice(TunerDisplay::Strobe), 1);
    assert_eq!(display_from_choice(0), TunerDisplay::Needle);
    assert_eq!(display_from_choice(1), TunerDisplay::Strobe);
}

#[test]
fn the_strobe_holds_inside_one_cent_and_gears_the_rest() {
    assert_eq!(super::gauge::strobe_offset(0.0, 4.0, 1.0), 0.0);
    assert_eq!(super::gauge::strobe_offset(1.0, 4.0, 5.0), 0.0);
    assert_eq!(super::gauge::strobe_offset(-1.0, 4.0, 2.0), 0.0);
    assert!(super::gauge::strobe_offset(1.1, 1.0, 1.0) > 0.0);
    assert!(super::gauge::strobe_offset(-1.1, 1.0, 1.0) < 0.0);
    let gear1 = super::gauge::strobe_offset(6.0, 2.0, 1.0);
    assert_eq!(gear1, 6.0 * 14.0 * 2.0);
    assert_eq!(super::gauge::strobe_offset(6.0, 2.0, 5.0), gear1 * 5.0);
    assert_eq!(super::gauge::strobe_offset(-6.0, 2.0, 1.0), -gear1);
    assert_eq!(super::gauge::strobe_offset(80.0, 1.0, 1.0), 24.0 * 14.0);
    assert_eq!(
        super::gauge::strobe_offset(24.0, 1.0, 2.0),
        24.0 * 14.0 * 2.0
    );
}

#[test]
fn the_mark_sits_at_the_centre_and_clamps_past_fifty() {
    assert_eq!(mark_fraction(0.0), 0.0);
    assert_eq!(mark_fraction(-50.0), -1.0);
    assert_eq!(mark_fraction(50.0), 1.0);
    assert_eq!(mark_fraction(-80.0), -1.0);
    assert_eq!(mark_fraction(90.0), 1.0);
    assert_eq!(mark_fraction(25.0), 0.5);
    assert_eq!(mark_fraction(f32::NAN), 0.0);
}

#[test]
fn pitch_colour_ignores_direction() {
    let colours = &crate::ui::theme::DARK;
    assert_eq!(
        state_colour(colours, TuneState::InTune, 0.0),
        colours.path[2]
    );
    assert_eq!(
        state_colour(colours, TuneState::Centered, -0.4),
        colours.path[2]
    );
    assert_eq!(state_colour(colours, TuneState::Flat, -12.0), colours.warn);
    assert_eq!(state_colour(colours, TuneState::Flat, -38.0), colours.error);
    assert_eq!(state_colour(colours, TuneState::Sharp, 12.0), colours.warn);
    assert_eq!(state_colour(colours, TuneState::Sharp, 38.0), colours.error);
    assert_eq!(state_colour(colours, TuneState::Sharp, 25.0), colours.warn);
    assert_eq!(
        state_colour(colours, TuneState::Silent, 40.0),
        colours.text3
    );
}

#[test]
fn a_ghost_does_not_count_as_in_tune() {
    assert!(counts_in_tune(true, false, TuneState::Centered));
    assert!(counts_in_tune(true, false, TuneState::InTune));
    assert!(!counts_in_tune(true, true, TuneState::InTune));
    assert!(!counts_in_tune(true, true, TuneState::Centered));
    assert!(!counts_in_tune(false, false, TuneState::InTune));
    assert!(!counts_in_tune(true, false, TuneState::Flat));
}

#[test]
fn count_keeps_the_preset_length_before_the_first_frame() {
    let frame = TunerFrame {
        mono: TunerReading::silent(),
        strings: Vec::new(),
    };
    assert_eq!(tuning_count(&frame, 6), (0, 6));
}
