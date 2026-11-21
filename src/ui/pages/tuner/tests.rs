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
fn the_strobe_holds_inside_one_cent_and_caps_the_rest() {
    use super::gauge::strobe_target_speed;
    assert_eq!(strobe_target_speed(0.0, true), 0.0);
    assert_eq!(strobe_target_speed(1.0, true), 0.0);
    assert_eq!(strobe_target_speed(-1.0, true), 0.0);
    assert_eq!(strobe_target_speed(1.1, false), 0.0);
    assert_eq!(strobe_target_speed(f32::NAN, true), 0.0);
    assert!(strobe_target_speed(1.1, true) > 0.0);
    assert!(strobe_target_speed(-1.1, true) < 0.0);
    assert_eq!(strobe_target_speed(6.0, true), 6.0 * 14.0);
    assert_eq!(strobe_target_speed(-6.0, true), -6.0 * 14.0);
    assert_eq!(strobe_target_speed(80.0, true), 24.0 * 14.0);
}

#[test]
fn a_cents_step_eases_the_speed_and_leaves_the_pattern_put() {
    use super::gauge::StrobeMotion;
    let mut motion = StrobeMotion::default();
    let mut now = 0.0;
    for _ in 0..60 {
        now += 1.0 / 60.0;
        motion.step(6.0, true, now);
    }
    let settled = motion.speed;
    let phase = motion.phase;
    assert!((settled - 6.0 * 14.0).abs() < 1.0, "{settled}");

    now += 1.0 / 60.0;
    motion.step(20.0, true, now);
    let moved = motion.phase - phase;
    // One frame is still near the old speed.  The old placement, speed times
    // the clock, would have relocated the pattern by hundreds of pixels.
    assert!(moved > 0.0, "{moved}");
    assert!(moved < super::gauge::strobe_target_speed(20.0, true) / 60.0);
    assert!(moved < 5.0, "{moved}");
}

#[test]
fn reversing_the_cents_coasts_through_a_stop() {
    use super::gauge::{ease_strobe_speed, strobe_target_speed};
    let forward = strobe_target_speed(8.0, true);
    let back = strobe_target_speed(-8.0, true);
    let speed = ease_strobe_speed(forward, back, 1.0 / 60.0);
    assert!(speed > 0.0, "{speed}");
    assert!(speed < forward, "{speed}");
}

#[test]
fn a_centered_reading_eases_the_discs_to_a_stop() {
    use super::gauge::StrobeMotion;
    let mut motion = StrobeMotion::default();
    let mut now = 0.0;
    for _ in 0..60 {
        now += 1.0 / 60.0;
        motion.step(6.0, true, now);
    }
    assert!(motion.speed > 10.0);
    for _ in 0..90 {
        now += 1.0 / 60.0;
        motion.step(0.2, true, now);
    }
    assert_eq!(motion.speed, 0.0);
}

#[test]
fn the_level_readout_fits_the_reserved_column() {
    let room = METER_VALUE_SAMPLE.chars().count();
    for level in [-100.3, -99.2, -9.9, 0.0, 9.9, 99.2, 100.3] {
        let text = level_readout(level);
        assert!(
            text.chars().count() <= room,
            "{text} is wider than {METER_VALUE_SAMPLE}"
        );
    }
    assert!(level_readout(100.3).chars().count() > level_readout(99.2).chars().count());
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
