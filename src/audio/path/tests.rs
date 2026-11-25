use super::{PathControls, PathMix, PATH_MATCH_MAX_DB};
use crate::audio::LockStats;

#[test]
fn a_path_match_is_clamped_and_a_nan_becomes_zero() {
    let control = PathControls::new(48_000);
    control.set_match_db(100.0);
    assert_eq!(control.match_db(), PATH_MATCH_MAX_DB);
    control.set_match_db(-100.0);
    assert_eq!(control.match_db(), -PATH_MATCH_MAX_DB);
    control.set_match_db(f32::NAN);
    assert_eq!(control.match_db(), 0.0);
    assert_eq!(control.match_gain(), 1.0);
}

#[test]
fn the_path_mix_sanitizes_its_match() {
    let mix = PathMix {
        match_db: f32::INFINITY,
        ..PathMix::default()
    };
    assert_eq!(mix.sanitized().match_db, PATH_MATCH_MAX_DB);
    let mix = PathMix {
        match_db: f32::NAN,
        ..PathMix::default()
    };
    assert_eq!(mix.sanitized().match_db, 0.0);
}

#[test]
fn a_path_with_no_effects_does_not_count_a_chain_attempt() {
    let path = PathControls::new(48_000);
    let mut left = vec![0.0f32; 64];
    let mut right = vec![0.0f32; 64];
    // No active effect and no latency: the lock-free early return does no
    // work, so it is not an attempt.
    assert!(path.process_plugins(&mut left, &mut right, 64));
    assert_eq!(path.plugin_lock_report().chain, LockStats::default());
}

#[test]
fn lock_stats_clamp_skips_to_attempts() {
    // A reset racing the audio thread can zero the attempts after a skip
    // was counted.
    let reset_race = LockStats {
        attempts: 0,
        skips: 1,
    };
    assert_eq!(reset_race.skipped(), 0);
    assert_eq!(reset_race.share(), 0.0);
    let clamped = LockStats {
        attempts: 2,
        skips: 5,
    };
    assert_eq!(clamped.skipped(), 2);
    assert_eq!(clamped.share(), 1.0);
}

#[test]
fn a_missed_chain_is_counted_as_one_attempt_and_one_skip() {
    let path = PathControls::new(48_000);
    path.edit_chain(|chain| chain.insert_delay_for_test(16));
    let mut left = vec![0.5f32; 64];
    let mut right = vec![-0.25f32; 64];
    // Someone else holds the runtime: this block skips the chain.
    let guard = path.hold_chain_for_test();
    assert!(!path.process_plugins(&mut left, &mut right, 64));
    let report = path.plugin_lock_report();
    assert_eq!(
        report.chain,
        LockStats {
            attempts: 1,
            skips: 1,
        }
    );
    assert_eq!(report.chain.share(), 1.0);
    // The effects never ran, so their own counters stay at zero.
    assert_eq!(
        report.effects,
        vec![LockStats::default()],
        "an effect counted a chunk it was kept out of"
    );
    drop(guard);
    // The next chunk finds the runtime parked: one more attempt, no new skip.
    assert!(path.process_plugins(&mut left, &mut right, 64));
    assert_eq!(
        path.plugin_lock_report().chain,
        LockStats {
            attempts: 2,
            skips: 1,
        }
    );
    path.reset_plugin_lock_counters();
    let report = path.plugin_lock_report();
    assert_eq!(report.chain, LockStats::default());
    assert_eq!(report.effects, vec![LockStats::default()]);
    // Release the slot here: a slot left on the deferred-drop queue flaked
    // the parallel suite.
    path.edit_chain(|chain| chain.restore(Vec::new()));
}

#[test]
fn a_poisoned_chain_is_reported_without_counting_a_skip() {
    // The chain cannot be edited or restored once poisoned, so the claim
    // makes sure its slots are destroyed on this thread instead of being
    // left for the deferred queue.
    let _main = crate::audio::claim_main_thread();
    let path = PathControls::new(48_000);
    path.insert_delay_for_test(16);
    path.poison_chain_for_test();
    let mut left = vec![0.0f32; 64];
    let mut right = vec![0.0f32; 64];
    // The runtime is parked but the chain is dead: the block takes the dry
    // fallback.
    assert!(!path.process_plugins(&mut left, &mut right, 64));
    let report = path.plugin_lock_report();
    assert_eq!(
        report.chain,
        LockStats {
            attempts: 1,
            skips: 0,
        },
        "a dead chain is not contention"
    );
    assert!(report.chain_poisoned);
    // Resetting the counters cannot make the chain usable again.
    path.reset_plugin_lock_counters();
    assert!(path.plugin_lock_report().chain_poisoned);
}

#[test]
fn a_zero_latency_edit_panic_is_reported_before_the_callback() {
    // The chain cannot be edited or restored once poisoned, so the claim
    // makes sure its slots are destroyed on this thread instead of being
    // left for the deferred queue.
    let _main = crate::audio::claim_main_thread();
    let path = PathControls::new(48_000);
    path.insert_delay_for_test(0);
    path.poison_chain_for_test();
    // Published by the unwind guard.  The callback has not run yet.
    assert!(path.plugin_lock_report().chain_poisoned);
    let mut left = vec![0.0f32; 64];
    let mut right = vec![0.0f32; 64];
    // Active is cleared and latency is still 0, so this returns before the
    // dead chain and counts no attempt.
    assert!(path.process_plugins(&mut left, &mut right, 64));
    assert_eq!(path.plugin_lock_report().chain.attempts, 0);
}

#[test]
fn chain_bypass_skips_the_callback_and_leaves_stored_bypass_alone() {
    let path = PathControls::new(48_000);
    path.insert_delay_for_test(16);
    path.insert_delay_for_test(8);
    path.edit_chain(|chain| chain.set_bypass(1, true));
    assert_eq!(path.latency_samples(), 24);
    let saved = path.plugin_prefs();
    assert!(!saved[0].bypass);
    assert!(saved[1].bypass);

    let mut mix = path.mix();
    assert!(!mix.chain_bypass);
    mix.chain_bypass = true;
    path.set_mix(&mix);
    assert!(path.chain_bypassed());
    assert!(path.mix().chain_bypass);

    let mut left = vec![0.0f32; 64];
    let mut right = vec![0.0f32; 64];
    left[0] = 1.0;
    right[1] = 1.0;
    assert!(path.process_plugins(&mut left, &mut right, 64));
    assert_eq!(left[24], 1.0, "latency still delays the block");
    assert_eq!(right[25], 1.0);
    assert_eq!(path.latency_samples(), 24);
    let report = path.plugin_lock_report();
    assert_eq!(report.effects[0], LockStats::default());
    assert_eq!(report.effects[1], LockStats::default());
    let saved = path.plugin_prefs();
    assert!(!saved[0].bypass);
    assert!(saved[1].bypass);

    mix.chain_bypass = false;
    path.set_mix(&mix);
    assert!(!path.chain_bypassed());
    path.reset_plugin_lock_counters();
    let mut left = vec![0.0f32; 64];
    let mut right = vec![0.0f32; 64];
    assert!(path.process_plugins(&mut left, &mut right, 64));
    let report = path.plugin_lock_report();
    assert_eq!(
        report.effects[0],
        LockStats {
            attempts: 1,
            skips: 0,
        },
        "the effect processes again"
    );
    assert_eq!(report.effects[1], LockStats::default());
    let saved = path.plugin_prefs();
    assert!(!saved[0].bypass);
    assert!(saved[1].bypass);
    assert_eq!(path.latency_samples(), 24);
    path.edit_chain(|chain| chain.restore(Vec::new()));
}

/// The "Gain" control the fake CLAP plugin offers (native 0 to 2, so
/// normalized 0 to 1, opening at 0.5; see
/// `clap::tests::fake_plugin_with_params_for_test`).
const FAKE_GAIN: u32 = 7;

/// A path with two loaded fake effects, each listing that plugin's controls.
fn path_with_two_fake_effects() -> PathControls {
    use crate::audio::clap::tests::fake_plugin_with_params_for_test;
    let path = PathControls::new(48_000);
    path.edit_chain(|chain| {
        chain.insert_clap_for_test(fake_plugin_with_params_for_test());
        chain.insert_clap_for_test(fake_plugin_with_params_for_test());
    });
    path
}

/// The normalized value of control `id` in `view`'s list.
fn control_value(view: &crate::audio::PluginView, id: u32) -> f64 {
    view.parameters
        .iter()
        .find(|parameter| parameter.id == id)
        .unwrap_or_else(|| panic!("the effect has no control {id}"))
        .value
}

/// One control change rebuilds one effect's list: every other effect keeps
/// the control storage it was published with, and the set it replaced stays
/// readable with its old value.
#[test]
fn a_single_control_publish_shares_the_other_effects_controls() {
    let _main = crate::audio::claim_main_thread();
    let path = path_with_two_fake_effects();
    let before = path.plugin_views();
    assert_eq!(before.len(), 2);
    // The fake opens its "Gain" at half of its range.
    assert_eq!(control_value(&before[1], FAKE_GAIN), 0.5);

    path.set_plugin_parameter(1, FAKE_GAIN, 0.25)
        .expect("set Gain");
    let after = path.plugin_views();

    assert!(
        std::sync::Arc::ptr_eq(&before[0].parameters, &after[0].parameters),
        "the unchanged effect copied its controls"
    );
    assert!(
        !std::sync::Arc::ptr_eq(&before[1].parameters, &after[1].parameters),
        "the changed effect shows the old list"
    );
    assert_eq!(control_value(&after[1], FAKE_GAIN), 0.25);
    // The set the change replaced is still the one it was.
    assert_eq!(control_value(&before[1], FAKE_GAIN), 0.5);
    assert_eq!(control_value(&after[0], FAKE_GAIN), 0.5);
    path.edit_chain(|chain| chain.restore(Vec::new()));
}

/// A change outside the control list (mix, bypass) copies no list at all.
#[test]
fn a_bypass_or_mix_publish_copies_no_control_list() {
    let _main = crate::audio::claim_main_thread();
    let path = path_with_two_fake_effects();
    let before = path.plugin_views();
    assert!(before.iter().all(|view| !view.parameters.is_empty()));

    path.set_plugin_bypass(1, true);
    path.set_plugin_mix(0, 0.5);
    let after = path.plugin_views();

    for index in 0..before.len() {
        assert!(
            std::sync::Arc::ptr_eq(&before[index].parameters, &after[index].parameters),
            "effect {index} rebuilt its control list"
        );
    }
    assert!(after[1].bypass);
    assert_eq!(after[0].mix, 0.5);
    path.edit_chain(|chain| chain.restore(Vec::new()));
}

/// The "Mode" control the fake CLAP plugin offers (native 0 to 3, so normalized
/// 0 to 1; see `clap::tests::fake_plugin_reporting_for_test`).
const FAKE_MODE: u32 = 9;

/// As [`path_with_two_fake_effects`], except that the second plugin reports a
/// new value for its "Mode" control from every block, the way a CLAP plugin
/// does when its own state changes.
fn path_with_a_reporting_effect() -> PathControls {
    use crate::audio::clap::tests::{
        fake_plugin_reporting_for_test, fake_plugin_with_params_for_test,
    };
    let path = PathControls::new(48_000);
    path.edit_chain(|chain| {
        chain.insert_clap_for_test(fake_plugin_with_params_for_test());
        chain.insert_clap_for_test(fake_plugin_reporting_for_test(FAKE_MODE, 3.0));
    });
    path
}

/// A value a plugin reports goes through the audio side and the UI's service
/// pass, which republishes every view.  A knob drag followed by that refresh
/// copies the dragged effect's controls once and no other effect's: the
/// refresh hands back the lists it already has.
#[test]
fn a_refresh_shares_the_controls_of_the_effects_that_did_not_change() {
    let _main = crate::audio::claim_main_thread();
    let path = path_with_a_reporting_effect();
    let before = path.plugin_views();
    assert_eq!(before.len(), 2);
    assert_eq!(control_value(&before[1], FAKE_MODE), 0.0);

    // The person drags the first effect's "Gain".  The second keeps reporting
    // its own "Mode" value from every block.
    path.set_plugin_parameter(0, FAKE_GAIN, 0.25)
        .expect("set Gain");
    let dragged = path.plugin_views();
    assert!(!std::sync::Arc::ptr_eq(
        &before[0].parameters,
        &dragged[0].parameters
    ));
    assert_eq!(control_value(&dragged[0], FAKE_GAIN), 0.25);

    // One block carries the second plugin's value to the UI side; the next
    // service pass drains it and republishes every view.
    let mut left = vec![0.0f32; 64];
    let mut right = left.clone();
    assert!(path.process_plugins(&mut left, &mut right, 64));
    path.service_plugins();
    let after = path.plugin_views();

    assert!(
        std::sync::Arc::ptr_eq(&dragged[0].parameters, &after[0].parameters),
        "the refresh copied the effect the drag already published"
    );
    assert_eq!(control_value(&after[0], FAKE_GAIN), 0.25);
    assert!(
        !std::sync::Arc::ptr_eq(&dragged[1].parameters, &after[1].parameters),
        "the effect that reported a value kept its old list"
    );
    assert_eq!(control_value(&after[1], FAKE_MODE), 1.0);
    assert_eq!(control_value(&dragged[1], FAKE_MODE), 0.0);
    path.edit_chain(|chain| chain.restore(Vec::new()));
}
