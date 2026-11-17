use super::PathControls;
use crate::audio::LockStats;

#[test]
fn a_path_with_no_inserts_does_not_count_a_chain_attempt() {
    let path = PathControls::new(48_000);
    let mut left = vec![0.0f32; 64];
    let mut right = vec![0.0f32; 64];
    // No active insert and no latency: the lock-free early return does no
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
    // The inserts never ran, so their own counters stay at zero.
    assert_eq!(
        report.inserts,
        vec![LockStats::default()],
        "an insert counted a chunk it was kept out of"
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
    assert_eq!(report.inserts, vec![LockStats::default()]);
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
    assert_eq!(report.inserts[0], LockStats::default());
    assert_eq!(report.inserts[1], LockStats::default());
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
        report.inserts[0],
        LockStats {
            attempts: 1,
            skips: 0,
        },
        "the insert processes again"
    );
    assert_eq!(report.inserts[1], LockStats::default());
    let saved = path.plugin_prefs();
    assert!(!saved[0].bypass);
    assert!(saved[1].bypass);
    assert_eq!(path.latency_samples(), 24);
    path.edit_chain(|chain| chain.restore(Vec::new()));
}
