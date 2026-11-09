use std::sync::Arc;

use super::super::clap::{HostState, AUDIO_PORTS_CHANGED};
use super::load::{merge_synced_params, params_to_restore, Capture, Restore};
use super::pref::{
    base64_decode, base64_encode, PluginFormat, PluginParamPref, PluginParamView, PluginPref,
    PluginState, PLUGIN_STATE_VERSION,
};
use super::slot::Slot;

fn param(id: u32, value: f64) -> PluginParamPref {
    PluginParamPref { id, value }
}

fn view(id: u32, value: f64, default: f64) -> PluginParamView {
    PluginParamView {
        id,
        value,
        default,
        ..PluginParamView::default()
    }
}

#[test]
fn base64_round_trips_every_length() {
    let data: Vec<u8> = (0..=255u8).collect();
    for len in 0..data.len() {
        let encoded = base64_encode(&data[..len]);
        assert_eq!(encoded.len() % 4, 0);
        assert_eq!(base64_decode(&encoded).as_deref(), Some(&data[..len]));
    }
    assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    assert_eq!(base64_encode(b"fo"), "Zm8=");
    assert_eq!(base64_decode("Zm8="), Some(b"fo".to_vec()));
}

#[test]
fn base64_rejects_damage() {
    assert_eq!(base64_decode("Zm8"), None);
    assert_eq!(base64_decode("Zm*="), None);
    assert_eq!(base64_decode("Z==="), None);
    assert_eq!(base64_decode("Zg==Zm8="), None);
}

#[test]
fn state_blob_needs_a_known_version_and_data() {
    let state = PluginState::capture(&[1, 2, 3], &[param(1, 0.5)]);
    assert_eq!(state.version, PLUGIN_STATE_VERSION);
    assert_eq!(state.bytes(), Some(vec![1, 2, 3]));
    assert_eq!(state.params, vec![param(1, 0.5)]);
    let newer = PluginState {
        version: PLUGIN_STATE_VERSION + 1,
        ..state.clone()
    };
    assert_eq!(newer.bytes(), None);
    let damaged = PluginState {
        data: "!!".to_owned(),
        ..state.clone()
    };
    assert_eq!(damaged.bytes(), None);
    assert_eq!(PluginState::capture(&[], &[]).bytes(), None);
}

#[test]
fn pref_round_trips_with_and_without_state() {
    let mut pref = PluginPref {
        path: "C:/Plugins/Verb.clap".to_owned(),
        format: PluginFormat::Clap,
        class_id: Some("com.example.verb".to_owned()),
        params: vec![param(4, 0.75)],
        ..PluginPref::default()
    };
    let plain = ron::to_string(&pref).expect("serialize");
    assert!(!plain.contains("state"), "{plain}");
    let back: PluginPref = ron::from_str(&plain).expect("deserialize");
    assert!(back.state.is_none());
    assert_eq!(back.params, pref.params);

    pref.state = Some(PluginState::capture(b"\x00\xffstate", &pref.params));
    let text = ron::to_string(&pref).expect("serialize");
    let back: PluginPref = ron::from_str(&text).expect("deserialize");
    assert_eq!(back.state, pref.state);
    assert_eq!(
        back.state.and_then(|state| state.bytes()),
        Some(b"\x00\xffstate".to_vec())
    );
}

#[test]
fn old_pref_without_state_still_loads() {
    let back: PluginPref = ron::from_str(
        r#"(path:"C:/A.clap",format:clap,class_id:Some("a"),params:[(id:1,value:0.3)])"#,
    )
    .expect("old layout");
    assert!(back.state.is_none());
    assert_eq!(back.params, vec![param(1, 0.3)]);
    let damaged: PluginPref =
        ron::from_str(r#"(path:"C:/A.clap",state:Some((version:"x")))"#).expect("damaged blob");
    assert!(damaged.state.is_none());
}

#[test]
fn without_a_restored_state_every_saved_value_is_applied() {
    let saved = [param(1, 0.2), param(2, 0.9)];
    assert_eq!(
        params_to_restore(&saved, None),
        vec![(1, Some(0.2)), (2, Some(0.9))]
    );
}

#[test]
fn a_restored_state_takes_precedence_over_unchanged_values() {
    let saved = [param(1, 0.2), param(2, 0.9)];
    assert!(params_to_restore(&saved, Some(&saved)).is_empty());
}

#[test]
fn values_changed_after_the_capture_win_over_the_state() {
    let snapshot = [param(1, 0.2), param(2, 0.9), param(3, 0.4)];
    // 1 moved, 2 unchanged, 3 back to default (no longer saved), 4 new.
    let saved = [param(1, 0.6), param(2, 0.9), param(4, 0.1)];
    assert_eq!(
        params_to_restore(&saved, Some(&snapshot)),
        vec![(1, Some(0.6)), (4, Some(0.1)), (3, None)]
    );
}

#[test]
fn restore_ignores_the_snapshot_when_the_state_did_not_load() {
    let pref = PluginPref {
        params: vec![param(1, 0.2)],
        state: Some(PluginState::capture(&[7], &[param(1, 0.2)])),
        ..PluginPref::default()
    };
    let restore = Restore::from_pref(&pref);
    assert_eq!(restore.state, Some(vec![7]));
    assert!(restore.params(true).is_empty());
    assert_eq!(restore.params(false), vec![(1, Some(0.2))]);
}

#[test]
fn only_a_deferred_capture_keeps_the_slot_stale() {
    assert!(Capture::Deferred.leaves_stale());
    assert!(!Capture::Failed.leaves_stale());
    assert!(!Capture::Saved(PluginState::capture(&[1], &[])).leaves_stale());
}

#[test]
fn a_kept_older_blob_still_restores_newer_values() {
    // A panel move was still queued at save time, so the capture was
    // deferred and the older blob (with its own snapshot) was kept.
    let pref = PluginPref {
        params: vec![param(1, 0.9)],
        state: Some(PluginState::capture(&[7], &[param(1, 0.2), param(2, 0.4)])),
        ..PluginPref::default()
    };
    assert_eq!(
        Restore::from_pref(&pref).params(true),
        vec![(1, Some(0.9)), (2, None)]
    );
}

#[test]
fn sync_keeps_changed_values_and_ids_the_plugin_does_not_list() {
    let previous = [param(1, 0.8), param(9, 0.3)];
    let live = [view(1, 0.5, 0.5), view(2, 0.7, 0.1)];
    assert_eq!(
        merge_synced_params(&previous, &live),
        vec![param(2, 0.7), param(9, 0.3)]
    );
}

#[test]
fn a_port_change_is_visible_without_processing_a_block() {
    let mut slot = Slot::empty(PluginPref::default());
    slot.clap_host = Some(Arc::new(HostState::for_test(true)));
    assert!(slot.ports_changed());
    assert_eq!(slot.resolved_error().as_deref(), Some(AUDIO_PORTS_CHANGED));

    slot.clap_host = Some(Arc::new(HostState::for_test(false)));
    assert!(!slot.ports_changed());
    assert!(slot.resolved_error().is_none());
}

#[test]
fn the_chain_latency_counts_bypassed_inserts_but_not_failed_ones() {
    use super::super::clap::tests::{
        fake_plugin_with_latency_for_test, restart_fake_with_latency_for_test,
    };
    use super::{service_clap, PluginChain};
    use std::sync::atomic::Ordering;

    let mut chain = PluginChain::new(48_000);
    let total = chain.latency_handle();
    chain.insert_clap_for_test(fake_plugin_with_latency_for_test(16));
    chain.insert_clap_for_test(fake_plugin_with_latency_for_test(32));
    // The fake loaded last is the one `restart_fake_with_latency_for_test` moves.
    chain.insert_clap_for_test(fake_plugin_with_latency_for_test(64));
    // A saved insert that was never opened.
    assert!(chain
        .push_slot(Slot::empty(PluginPref::default()), 48_000)
        .is_ok());
    assert_eq!(chain.latency_samples(), 112);
    assert_eq!(total.load(Ordering::Acquire), 112);

    // Bypass keeps the latency, so the alignment does not move.
    chain.set_bypass(1, true);
    assert_eq!(chain.latency_samples(), 112);
    // A failed insert is out of the chain.
    chain.slot_mut_for_test(0).error = Some("Processing failed".to_owned());
    assert_eq!(chain.latency_samples(), 96);

    let views = chain.views();
    let latencies: Vec<u32> = views.iter().map(|view| view.latency).collect();
    assert_eq!(latencies, vec![0, 32, 64, 0]);
    assert_eq!(views[2].latency_label().as_deref(), Some("· 64 smp"));
    assert_eq!(views[3].latency_label(), None);

    let mut left = vec![0.0f32; 32];
    let mut right = vec![0.0f32; 32];
    let services = chain.clap_services();
    let service_all = || {
        services
            .iter()
            .map(|(host, plugin)| service_clap(host, plugin))
            .fold(false, |any, changed| any | changed)
    };
    // The failure set above (outside an edit) reaches the atomic with the
    // first block, which asks for a republish; the next block does not.
    assert!(chain.process(&mut left, &mut right, 32));
    assert_eq!(total.load(Ordering::Acquire), 96);
    assert!(!chain.process(&mut left, &mut right, 32));

    // The last fake switches modes the CLAP way: it asks for a restart and
    // reports the new latency when reactivated.
    restart_fake_with_latency_for_test(8);
    // The audio thread stops it first; the latency has not moved yet.
    assert!(!chain.process(&mut left, &mut right, 32));
    // The main thread restarts it and reads the new value.
    assert!(service_all());
    assert_eq!(chain.latency_samples(), 40);
    // The next block publishes it and asks for the views to be rebuilt,
    // so the stale `· 64 smp` label goes.
    assert!(chain.process(&mut left, &mut right, 32));
    assert_eq!(total.load(Ordering::Acquire), 40);
    assert_eq!(chain.views()[2].latency, 8);
    assert!(!chain.process(&mut left, &mut right, 32));

    // Destroy the plugins here rather than through the chain's deferred
    // teardown, which another test's main-thread claim could trigger.
    drop(services);
    drop(chain.restore(Vec::new()));
    assert_eq!(total.load(Ordering::Acquire), 0);
}

/// An impulse at frame `at` in a block of `frames`.
fn impulse(frames: usize, at: usize) -> Vec<f32> {
    let mut block = vec![0.0f32; frames];
    block[at] = 1.0;
    block
}

#[test]
fn a_rate_reload_keeps_the_insert_lock_counters() {
    use super::PluginChain;
    let mut chain = PluginChain::new(48_000);
    chain.insert_delay_for_test(16);
    let mut left = vec![0.0f32; 32];
    let mut right = vec![0.0f32; 32];
    chain.process(&mut left, &mut right, 32);
    let before = chain.slot_mut_for_test(0).controls.lock_stats();
    assert_eq!(before.attempts, 1, "the insert lock was tried once");

    let plan = chain.reload_plan(96_000).expect("a new rate reloads");
    assert_eq!(plan.jobs.len(), 1);
    let built: Vec<_> = plan
        .jobs
        .into_iter()
        .map(|(index, pref)| (index, pref, Slot::delay_for_test(16)))
        .collect();
    let replaced = match chain.install_reload(plan.previous_rate, 96_000, built) {
        Ok(replaced) => replaced,
        Err(_) => panic!("the rebuilt slots match the chain"),
    };
    drop(replaced);
    // `install_reload` reuses the old `SlotControls`, so the UI-side mix,
    // bypass and lock counters survive the rebuild.
    assert_eq!(chain.slot_mut_for_test(0).controls.lock_stats(), before);
    // Destroy the rebuilt slot here rather than through the chain's deferred
    // teardown, which another test's main-thread claim could trigger.
    drop(chain.restore(Vec::new()));
}

#[test]
fn a_partial_mix_lines_the_dry_side_up_with_the_wet_one() {
    use super::PluginChain;
    let mut chain = PluginChain::new(48_000);
    chain.insert_delay_for_test(32);
    chain.set_mix(0, 0.5);
    let mut left = impulse(128, 10);
    let mut right = impulse(128, 20);
    chain.process(&mut left, &mut right, 128);
    // Dry and wet both arrive 32 frames late: one impulse, not two.
    assert_eq!(left, impulse(128, 42));
    assert_eq!(right, impulse(128, 52));
}

#[test]
fn a_skipped_insert_still_delays_by_its_latency() {
    use super::PluginChain;
    let mut chain = PluginChain::new(48_000);
    chain.insert_delay_for_test(16);
    chain.insert_delay_for_test(8);
    assert_eq!(chain.latency_samples(), 24);
    // Bypass, mix at 0 and a running insert all add up to the same delay.
    chain.set_bypass(0, true);
    chain.set_mix(1, 0.0);
    let mut left = impulse(64, 0);
    let mut right = impulse(64, 1);
    chain.process(&mut left, &mut right, 64);
    assert_eq!(left, impulse(64, 24));
    assert_eq!(right, impulse(64, 25));
}

#[test]
fn a_clap_insert_skipped_by_its_editor_or_a_restart_keeps_its_latency() {
    use super::super::clap::tests::{
        fake_plugin_with_latency_for_test, restart_fake_with_latency_for_test,
    };
    use super::{PluginChain, PluginHandle};
    let mut chain = PluginChain::new(48_000);
    chain.insert_clap_for_test(fake_plugin_with_latency_for_test(64));
    let mut left = vec![0.0f32; 128];
    let mut right = vec![0.0f32; 128];
    // A normal block: the fake writes its constant.
    chain.process(&mut left, &mut right, 128);
    assert_eq!(left[0], 1.0);

    // Its editor holds the plugin: the block is the input, 64 frames late.
    let Some(PluginHandle::Clap(plugin)) = chain.handle(0) else {
        panic!("a CLAP insert");
    };
    {
        let _editor = plugin.lock().unwrap();
        let mut left = impulse(128, 36);
        let mut right = impulse(128, 36);
        chain.process(&mut left, &mut right, 128);
        // The switch from the plugin's output smooths its step over 2 ms
        // (96 frames) from the constant down; the impulse after it comes
        // 64 frames late.
        assert_eq!(left[0], 1.0);
        assert!(left.windows(2).take(95).all(|pair| pair[1] < pair[0]));
        assert_eq!(left[96..], impulse(128, 100)[96..]);
    }
    // The normal block before and the locked one now: one lock skip.
    assert_eq!(
        chain.slot_mut_for_test(0).controls.lock_stats(),
        crate::audio::LockStats {
            attempts: 2,
            skips: 1,
        }
    );
    // A restart passes the block through: still 64 frames late.
    restart_fake_with_latency_for_test(64);
    let mut left = impulse(128, 0);
    let mut right = impulse(128, 0);
    chain.process(&mut left, &mut right, 128);
    assert_eq!(left, impulse(128, 64));
    // The restart's pass-through is an attempt, not a missed lock.
    assert_eq!(
        chain.slot_mut_for_test(0).controls.lock_stats(),
        crate::audio::LockStats {
            attempts: 3,
            skips: 1,
        }
    );
    drop(plugin);
    drop(chain.restore(Vec::new()));
}

#[test]
fn a_bypass_after_a_latency_change_delays_by_the_new_latency_at_once() {
    use super::PluginChain;
    let mut chain = PluginChain::new(48_000);
    chain.insert_delay_for_test(0);
    let mut left = vec![0.0f32; 128];
    let mut right = vec![0.0f32; 128];
    chain.process(&mut left, &mut right, 128);
    // The insert switches to 64 frames while it runs fully wet: its dry
    // side is not read, but has to follow.
    chain
        .slot_mut_for_test(0)
        .test_delay
        .as_mut()
        .unwrap()
        .latency = 64;
    for _ in 0..4 {
        chain.process(&mut left, &mut right, 128);
    }
    chain.set_bypass(0, true);
    let mut left = impulse(128, 0);
    let mut right = impulse(128, 0);
    chain.process(&mut left, &mut right, 128);
    assert_eq!(left, impulse(128, 64), "no fade from the old delay");
}
