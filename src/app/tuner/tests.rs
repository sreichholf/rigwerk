use super::*;
use crate::app::settings::page_key;
use crate::audio::{TuneState, TuningGroup};

fn chromatic() -> Preferences {
    Preferences::default()
}

#[test]
fn closing_the_tuner_reports_whether_the_output_comes_back() {
    let mut app = crate::app::blank_app();
    app.engine.state = EngineState::Running;
    app.tuner.auto_mute = true;
    app.page = Page::Tuner;
    app.log_tuner_page();
    assert!(
        app.log
            .iter()
            .any(|entry| entry.text.contains("output muted")),
        "{:?}",
        app.log
            .iter()
            .map(|entry| entry.text.as_str())
            .collect::<Vec<_>>()
    );

    app.log.clear();
    app.mute = true;
    app.page = Page::Live;
    app.log_tuner_page();
    assert!(app
        .log
        .iter()
        .any(|entry| entry.text.contains("still muted")));
    assert!(app.log.iter().all(|entry| !entry.text.contains("restored")));

    app.log.clear();
    app.page = Page::Tuner;
    app.log_tuner_page();
    app.mute = false;
    app.page = Page::Live;
    app.log_tuner_page();
    assert!(app
        .log
        .iter()
        .any(|entry| entry.text.contains("output restored")));
}

#[test]
fn auto_mute_follows_the_truth_table() {
    let mut tuner = TunerState::new(&chromatic(), false);
    let states = [
        EngineState::Idle,
        EngineState::Preview,
        EngineState::Starting,
        EngineState::Running,
        EngineState::Stopping,
        EngineState::Error,
    ];
    for auto in [false, true] {
        tuner.auto_mute = auto;
        for page in [Page::Live, Page::Mixer, Page::Tuner] {
            for state in &states {
                let expected = auto
                    && page == Page::Tuner
                    && matches!(state, EngineState::Running | EngineState::Preview);
                assert_eq!(
                    tuner.auto_muted(page, *state),
                    expected,
                    "auto={auto} page={} state={state:?}",
                    page_key(page)
                );
            }
        }
    }
}

#[test]
fn defaults_are_chromatic_single_and_standard() {
    let tuner = TunerState::new(&chromatic(), false);
    assert!(tuner.preset.is_none());
    assert_eq!(tuner.preset_id(), DEFAULT_PRESET_ID);
    assert_eq!(tuner.reference_hz, DEFAULT_REFERENCE_HZ);
    assert_eq!(tuner.channel, TunerChannel::Sum);
    assert_eq!(tuner.display, TunerDisplay::Strobe);
    assert_eq!(tuner.listen, TunerListen::Single);
    assert!(tuner.auto_mute);
    assert_eq!(tuner.tolerance, TunerTolerance::Standard);
    assert!(tuner.frame().strings.is_empty());
    assert!(!tuner.frame().mono.valid);
}

#[test]
fn persisted_values_round_trip_through_the_state() {
    let mut preferences = chromatic();
    preferences.tuner_tuning = "drop-c".to_owned();
    preferences.tuner_reference_hz = 432.0;
    preferences.tuner_channel = TunerChannel::Right.to_u8();
    preferences.tuner_display = TunerDisplay::Needle.to_u8();
    preferences.tuner_listen = TunerListen::All.to_u8();
    preferences.tuner_auto_mute = false;
    preferences.tuner_tolerance = TunerTolerance::Live.to_u8();

    let tuner = TunerState::new(&preferences, false);
    assert_eq!(tuner.preset_id(), "drop-c");
    assert_eq!(tuner.preset().map(|tuning| tuning.id), Some("drop-c"));
    assert_eq!(tuner.reference_hz, 432.0);
    assert_eq!(tuner.channel, TunerChannel::Right);
    assert_eq!(tuner.display, TunerDisplay::Needle);
    assert_eq!(tuner.listen, TunerListen::All);
    assert!(!tuner.auto_mute);
    assert_eq!(tuner.tolerance, TunerTolerance::Live);
}

#[test]
fn invalid_persisted_values_fall_back() {
    let mut preferences = chromatic();
    preferences.tuner_tuning = "not-a-tuning".to_owned();
    preferences.tuner_reference_hz = f32::NAN;
    preferences.tuner_channel = 9;
    preferences.tuner_display = 9;
    preferences.tuner_listen = 9;
    preferences.tuner_tolerance = 9;

    let tuner = TunerState::new(&preferences, false);
    assert!(tuner.preset.is_none());
    assert_eq!(tuner.reference_hz, DEFAULT_REFERENCE_HZ);
    assert_eq!(tuner.channel, TunerChannel::Sum);
    assert_eq!(tuner.display, TunerDisplay::Strobe);
    assert_eq!(tuner.listen, TunerListen::Single);
    assert_eq!(tuner.tolerance, TunerTolerance::Standard);

    preferences.tuner_reference_hz = 500.0;
    assert_eq!(
        TunerState::new(&preferences, false).reference_hz,
        REFERENCE_MAX_HZ
    );
    preferences.tuner_reference_hz = 400.0;
    assert_eq!(
        TunerState::new(&preferences, false).reference_hz,
        REFERENCE_MIN_HZ
    );
}

#[test]
fn all_strings_needs_a_preset() {
    let mut preferences = chromatic();
    preferences.tuner_listen = TunerListen::All.to_u8();
    preferences.tuner_tuning = "chromatic".to_owned();
    let tuner = TunerState::new(&preferences, false);
    assert_eq!(tuner.listen, TunerListen::Single);

    preferences.tuner_tuning = "drop-c".to_owned();
    let tuner = TunerState::new(&preferences, false);
    assert_eq!(tuner.listen, TunerListen::All);

    let mut tuner = tuner;
    tuner.set_preset(None);
    assert_eq!(tuner.listen, TunerListen::Single);
    assert!(tuner.preset().is_none());
}

#[test]
fn channel_codes_round_trip() {
    for channel in TunerChannel::ALL {
        assert_eq!(TunerChannel::from_u8(channel.to_u8()), channel);
    }
}

#[test]
fn every_preset_round_trips_by_id() {
    let mut preferences = chromatic();
    let mut tuner = TunerState::new(&preferences, false);
    for (index, tuning) in PRESETS.iter().enumerate() {
        preferences.tuner_tuning = tuning.id.to_owned();
        let restored = TunerState::new(&preferences, false);
        assert_eq!(restored.preset, Some(index));
        assert_eq!(restored.preset_id(), tuning.id);
        assert_eq!(restored.preset().map(|t| t.id), Some(tuning.id));

        tuner.set_preset(Some(index));
        assert_eq!(tuner.preset_id(), tuning.id);
    }
    assert_eq!(PRESETS.len(), 20);
    assert_eq!(
        PRESETS
            .iter()
            .filter(|tuning| tuning.group == TuningGroup::Bass)
            .count(),
        6
    );
}

#[test]
fn screenshot_uses_the_drop_c_demo() {
    let preferences = Preferences {
        tuner_channel: TunerChannel::Right.to_u8(),
        tuner_reference_hz: 432.0,
        tuner_display: TunerDisplay::Needle.to_u8(),
        tuner_listen: TunerListen::Single.to_u8(),
        ..Preferences::default()
    };

    let tuner = TunerState::new(&preferences, true);
    assert_eq!(tuner.preset_id(), "drop-c");
    assert_eq!(tuner.display, TunerDisplay::Strobe);
    assert_eq!(tuner.listen, TunerListen::All);
    assert_eq!(tuner.channel, TunerChannel::Sum);
    assert_eq!(tuner.reference_hz, DEFAULT_REFERENCE_HZ);
    assert!(tuner.auto_mute);
    assert!(!tuner.auto_muted(Page::Tuner, EngineState::Idle));
    assert!(tuner.auto_muted(Page::Tuner, EngineState::Running));

    let frame = tuner.frame();
    assert!(frame.mono.valid);
    assert_eq!(frame.mono.midi, 36);
    assert_eq!(frame.strings.len(), 6);
    let states: Vec<TuneState> = frame.strings.iter().map(|row| row.state).collect();
    assert_eq!(
        states,
        [
            TuneState::Flat,
            TuneState::InTune,
            TuneState::Centered,
            TuneState::Sharp,
            TuneState::InTune,
            TuneState::InTune,
        ]
    );
    let tuned = frame
        .strings
        .iter()
        .filter(|row| {
            row.harmonic_of.is_none()
                && matches!(row.state, TuneState::Centered | TuneState::InTune)
        })
        .count();
    assert_eq!(tuned, 4);
}
