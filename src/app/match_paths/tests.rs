use super::{MatchPathReason, MatchPathsSettings, MATCH_PATHS_TARGET_DEFAULT_LUFS};
use crate::app::blank_app;
use crate::audio::{LOUDNESS_TARGET_MAX_LUFS, LOUDNESS_TARGET_MIN_LUFS, SILENCE_LUFS};

/// Two input paths with their readings filled in, as if a measurement just
/// finished.
fn measured_app(readings: &[(usize, f32)]) -> crate::app::RigWerkApp {
    let mut app = blank_app();
    app.match_paths.target_lufs = -18.0;
    while app.path_count() < readings.len() {
        let slot = app.duplicate_path(0).expect("a free path");
        if let Some(path) = app.paths[slot].as_mut() {
            path.mix.mute = false;
        }
    }
    for &(slot, lufs) in readings {
        app.engine.publish_path_match_for_test(slot, lufs, 40);
    }
    app.finish_match_paths();
    app
}

#[test]
fn the_match_reference_sanitizes_its_target_and_path() {
    let settings = MatchPathsSettings {
        target_lufs: f32::NAN,
        reference_path: Some(crate::audio::MAX_PATHS),
    }
    .sanitized();
    assert_eq!(settings.target_lufs, MATCH_PATHS_TARGET_DEFAULT_LUFS);
    assert_eq!(settings.reference_path, None);

    let settings = MatchPathsSettings {
        target_lufs: 10.0,
        reference_path: Some(1),
    }
    .sanitized();
    assert_eq!(settings.target_lufs, LOUDNESS_TARGET_MAX_LUFS);
    assert_eq!(settings.reference_path, Some(1));

    let settings = MatchPathsSettings {
        target_lufs: -100.0,
        ..MatchPathsSettings::default()
    }
    .sanitized();
    assert_eq!(settings.target_lufs, LOUDNESS_TARGET_MIN_LUFS);
}

#[test]
fn two_paths_are_brought_to_the_target() {
    let mut app = measured_app(&[(0, -24.0), (1, -12.0)]);
    let result = app.match_paths_result.clone().expect("a result");
    assert_eq!(result.reference_lufs, -18.0);
    for row in &result.rows {
        // The meter reads before the match, so the new trim is what closes
        // the gap to the target.
        assert!(
            (row.measured_lufs + row.new_db - (-18.0)).abs() < 0.3,
            "{row:?}"
        );
    }
    app.apply_match_paths(&[true, true]);
    assert_eq!(app.paths[0].as_ref().unwrap().mix.match_db, 6.0);
    assert_eq!(app.paths[1].as_ref().unwrap().mix.match_db, -6.0);
    assert!(app
        .match_paths_status
        .as_deref()
        .is_some_and(|status| status.starts_with("Applied: ")));
    let result = app.match_paths_result.clone().expect("still shown");
    assert!(result.rows.iter().all(|row| row.change_db == 0.0));
}

#[test]
fn a_path_reference_keeps_its_trim_and_matches_the_others_to_it() {
    let mut app = measured_app(&[(0, -20.0), (1, -14.0)]);
    app.paths[0].as_mut().unwrap().mix.match_db = 3.0;
    app.match_paths_reference = Some(0);
    app.engine.publish_path_match_for_test(0, -20.0, 40);
    app.engine.publish_path_match_for_test(1, -14.0, 40);
    app.finish_match_paths();
    let result = app.match_paths_result.clone().expect("a result");
    assert_eq!(result.reference_path, Some(0));
    let reference = result.rows.iter().find(|row| row.slot == 0).unwrap();
    let other = result.rows.iter().find(|row| row.slot == 1).unwrap();
    assert_eq!(reference.new_db, 3.0, "the reference keeps its trim");
    let reference_plays_at = reference.measured_lufs + reference.current_db;
    assert!(
        (other.measured_lufs + other.new_db - reference_plays_at).abs() < 0.3,
        "{other:?} against {reference_plays_at:.2}"
    );
}

#[test]
fn the_too_quiet_gate_uses_what_the_reference_path_plays_at() {
    let mut app = measured_app(&[(0, -20.0), (1, -36.0)]);
    app.paths[0].as_mut().unwrap().mix.match_db = 6.0;
    app.match_paths_reference = Some(0);
    app.engine.publish_path_match_for_test(0, -20.0, 40);
    app.engine.publish_path_match_for_test(1, -36.0, 40);
    app.finish_match_paths();
    let result = app.match_paths_result.clone().expect("a result");
    assert_eq!(result.reference_lufs, -14.0);
    let quiet = result.rows.iter().find(|row| row.slot == 1).unwrap();
    assert_eq!(quiet.reason, Some(MatchPathReason::TooQuiet));
}

#[test]
fn the_reference_is_saved_as_a_position_and_loaded_as_a_slot() {
    let mut app = blank_app();
    let second = app.duplicate_path(0).expect("a second path");
    if let Some(path) = app.paths[second].as_mut() {
        path.mix.mute = false;
    }
    app.match_paths_reference = Some(second);
    let saved = app.saved_match_paths(&[second, 0]);
    assert_eq!(
        saved.reference_path,
        Some(0),
        "the second path is first in the saved order"
    );

    // The same save into the current order [0, second] names slot 0.
    app.install_match_paths(saved);
    assert_eq!(app.match_paths_reference, Some(0));

    app.match_paths_reference = Some(second);
    assert!(app.remove_path(second));
    assert_eq!(app.match_paths_reference, None, "the path is gone");
}

#[test]
fn a_muted_path_is_ticked_and_a_silent_path_is_not_heard() {
    let mut app = measured_app(&[(0, -18.0), (1, -18.0)]);
    app.paths[0].as_mut().unwrap().mix.mute = true;
    app.engine.publish_path_match_for_test(0, -24.0, 40);
    app.engine.publish_path_match_for_test(1, SILENCE_LUFS, 0);
    app.finish_match_paths();
    let result = app.match_paths_result.clone().expect("a result");
    assert_eq!(result.rows[0].reason, None);
    assert!(result.rows[0].ticked, "a muted path is still a candidate");
    assert_eq!(result.rows[0].new_db, 6.0);
    assert_eq!(result.rows[1].reason, Some(MatchPathReason::NotHeard));
    assert!(!result.rows[1].ticked);
}

#[test]
fn a_path_silenced_by_another_paths_solo_is_still_ticked() {
    let mut app = measured_app(&[(0, -18.0), (1, -18.0)]);
    app.paths[0].as_mut().unwrap().mix.solo = true;
    app.engine.publish_path_match_for_test(0, -18.0, 40);
    app.engine.publish_path_match_for_test(1, -18.0, 40);
    app.finish_match_paths();
    let result = app.match_paths_result.clone().expect("a result");
    assert_eq!(result.rows[0].reason, None);
    assert_eq!(result.rows[1].reason, None);
    assert!(result.rows[0].ticked && result.rows[1].ticked);
}

#[test]
fn a_return_is_unticked_by_default() {
    let mut app = measured_app(&[(0, -18.0), (1, -18.0)]);
    app.paths[1].as_mut().unwrap().mix.feed = crate::audio::PathFeed::Sends;
    app.engine.publish_path_match_for_test(0, -18.0, 40);
    app.engine.publish_path_match_for_test(1, -18.0, 40);
    app.finish_match_paths();
    let result = app.match_paths_result.clone().expect("a result");
    let ret = result.rows.iter().find(|row| row.slot == 1).unwrap();
    assert_eq!(ret.reason, Some(MatchPathReason::Return));
    assert!(!ret.ticked);
    assert!(
        MatchPathReason::Return.blocks_apply(),
        "the user can tick it"
    );
}

#[test]
fn cancel_writes_nothing() {
    let mut app = measured_app(&[(0, -24.0), (1, -12.0)]);
    app.paths[0].as_mut().unwrap().mix.match_db = 2.0;
    app.cancel_match_paths();
    assert_eq!(app.paths[0].as_ref().unwrap().mix.match_db, 2.0);
    assert!(app.match_paths_result.is_none());
}

#[test]
fn measuring_again_on_matched_paths_proposes_small_changes() {
    let mut app = measured_app(&[(0, -24.0), (1, -12.0)]);
    app.apply_match_paths(&[true, true]);
    // The meter reads before the match, so a second measurement sees the
    // same readings and proposes the same trims, not another +6 dB.
    app.engine.publish_path_match_for_test(0, -24.0, 40);
    app.engine.publish_path_match_for_test(1, -12.0, 40);
    app.finish_match_paths();
    let result = app.match_paths_result.clone().expect("a result");
    assert!(result.rows.iter().all(|row| row.change_db.abs() < 0.3));
    assert_eq!(result.rows[0].new_db, 6.0);
    assert_eq!(result.rows[1].new_db, -6.0);
}
