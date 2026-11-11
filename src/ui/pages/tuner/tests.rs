use super::*;
use crate::audio::TunerReading;

fn lead(string: Option<usize>, midi: i16) -> Lead {
    Lead {
        valid: true,
        string,
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
fn a_centered_strobe_holds_still() {
    assert_eq!(super::gauge::strobe_phase(0.0, 3.0), 0.0);
    assert_eq!(super::gauge::strobe_phase(0.04, 3.0), 0.0);
    assert_eq!(super::gauge::strobe_phase(-0.04, 3.0), 0.0);
    assert_eq!(super::gauge::strobe_phase(-2.0, 3.0), 3.0);
    assert_eq!(super::gauge::strobe_phase(2.0, 3.0), -3.0);
}

#[test]
fn count_keeps_the_preset_length_before_the_first_frame() {
    let frame = TunerFrame {
        mono: TunerReading::silent(),
        strings: Vec::new(),
    };
    assert_eq!(tuning_count(&frame, 6), (0, 6));
}
