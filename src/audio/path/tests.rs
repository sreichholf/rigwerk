use super::plugins::needs_full_sync;
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
fn only_a_processed_vst3_insert_can_sync_from_reported_changes() {
    // Full unless the caller did not ask for it, the insert processed a
    // block, and it is a VST3 insert.
    let table = [
        (true, true, true, true),
        (true, false, true, true),
        (true, true, false, true),
        (false, true, true, false),
        (false, false, true, true),
        (false, true, false, true),
        (false, false, false, true),
    ];
    for (requested, processed, has_vst3, expected) in table {
        assert_eq!(
            needs_full_sync(requested, processed, has_vst3),
            expected,
            "requested={requested} processed={processed} has_vst3={has_vst3}"
        );
    }
}

#[test]
fn a_missed_chain_lock_is_counted_as_one_attempt_and_one_skip() {
    let path = PathControls::new(48_000);
    path.edit_chain(|chain| chain.insert_delay_for_test(16));
    let mut left = vec![0.5f32; 64];
    let mut right = vec![-0.25f32; 64];
    // The UI holds the chain: the audio thread skips it for this block.
    let guard = path.plugins.lock().unwrap();
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
        "an insert counted a chunk the chain lock kept out"
    );
    drop(guard);
    // The next chunk finds the lock free: one more attempt, no new skip.
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
    // Drop the guard before releasing the slot: a slot left on the
    // deferred-drop queue flaked the parallel suite.
    drop(path.edit_chain(|chain| chain.restore(Vec::new())));
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
    // The lock is free but unusable: the block takes the dry fallback.
    assert!(!path.process_plugins(&mut left, &mut right, 64));
    let report = path.plugin_lock_report();
    assert_eq!(
        report.chain,
        LockStats {
            attempts: 1,
            skips: 0,
        },
        "a poisoned lock is not contention"
    );
    assert!(report.chain_poisoned);
    // Resetting the counters cannot make the chain usable again.
    path.reset_plugin_lock_counters();
    assert!(path.plugin_lock_report().chain_poisoned);
}
