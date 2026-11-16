use super::load::{
    apply_parameter_changes, merge_synced_params, params_to_restore, Capture, ParamChange, Restore,
};
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
    let old: PluginPref = ron::from_str(
        r#"(path:"C:/A.clap",state:Some((version:1,data:"AQ==",params:[(id:1,value:0.3)])))"#,
    )
    .expect("a saved snapshot");
    assert_eq!(old.state.expect("a state").params, vec![param(1, 0.3)]);
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

/// The snapshot of a blob saved with values still queued restores them
/// through the plain rule (`params_to_restore`, which an older build
/// shares): no marker is needed.
#[test]
fn a_snapshot_with_undelivered_values_restores_them_through_the_plain_rule() {
    use super::handle::capture_snapshot;
    // 1 and 2 were queued when the blob was saved: 1 is saved at 0.2, and 2
    // was set back to its default (so it is not saved any more); 3 was
    // delivered.
    let saved = [param(1, 0.2), param(3, 0.5)];
    let views = [view(1, 0.2, 0.0), view(2, 0.4, 0.4), view(3, 0.5, 0.0)];
    let snapshot = capture_snapshot(&saved, &[1, 2], &views);
    assert_eq!(snapshot, vec![param(3, 0.5), param(2, 0.4)]);
    assert_eq!(
        params_to_restore(&saved, Some(&snapshot)),
        vec![(1, Some(0.2)), (2, None)]
    );
    // Nothing queued: the snapshot is the saved values, and nothing is
    // applied on top.
    let snapshot = capture_snapshot(&saved, &[], &views);
    assert!(params_to_restore(&saved, Some(&snapshot)).is_empty());
}

#[test]
fn a_saved_value_of_minus_one_restores_like_any_other() {
    // -1.0 is a plain CLAP value like any other.
    let saved = [param(1, -1.0)];
    assert!(params_to_restore(&saved, Some(&[param(1, -1.0)])).is_empty());
    assert_eq!(
        params_to_restore(&saved, Some(&[param(1, 0.5)])),
        vec![(1, Some(-1.0))]
    );
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
fn a_read_only_control_is_never_saved() {
    let previous = [];
    let live = [
        view(1, 0.7, 0.0),
        PluginParamView {
            read_only: true,
            ..view(2, 0.9, 0.0)
        },
    ];
    assert_eq!(merge_synced_params(&previous, &live), vec![param(1, 0.7)]);
}

#[test]
fn the_same_control_reported_twice_keeps_the_last_value() {
    // A drag can report one id many times before the next service frame.
    let mut parameters = vec![view(1, 0.2, 0.0)];
    let mut saved = Vec::new();
    let changes = [
        ParamChange {
            id: 1,
            value: 0.5,
            display: None,
        },
        ParamChange {
            id: 1,
            value: 0.9,
            display: None,
        },
    ];
    assert!(apply_parameter_changes(
        &mut parameters,
        &mut saved,
        &changes
    ));
    assert_eq!(parameters[0].value, 0.9);
    assert_eq!(saved, vec![param(1, 0.9)]);
}

#[test]
fn a_change_keeps_a_saved_id_the_plugin_does_not_list() {
    let mut parameters = vec![view(1, 0.2, 0.0)];
    let mut saved = vec![param(1, 0.2), param(9, 0.4)];
    let changes = [ParamChange {
        id: 1,
        value: 0.6,
        display: None,
    }];
    assert!(apply_parameter_changes(
        &mut parameters,
        &mut saved,
        &changes
    ));
    assert_eq!(saved, vec![param(1, 0.6), param(9, 0.4)]);
}

#[test]
fn reported_changes_update_only_the_controls_they_name() {
    let mut parameters = vec![view(1, 0.2, 0.0), view(2, 0.5, 0.5), view(3, 0.9, 0.0)];
    let mut saved = vec![param(1, 0.2), param(3, 0.9), param(9, 0.4)];
    let changes = [
        ParamChange {
            id: 1,
            value: 0.7,
            display: Some("70 %".to_owned()),
        },
        // Not a control this plugin lists: ignored.
        ParamChange {
            id: 4,
            value: 0.5,
            display: None,
        },
    ];
    // One id in this batch is not a control the plugin lists, so the call is
    // asked for a full read, even though it applies the known ones.
    assert!(!apply_parameter_changes(
        &mut parameters,
        &mut saved,
        &changes
    ));
    assert_eq!(parameters[0].value, 0.7);
    assert_eq!(parameters[0].display, "70 %");
    // The views and saved entries it did not name stay as they were.
    assert_eq!(parameters[1].value, 0.5);
    assert_eq!(parameters[2].value, 0.9);
    assert_eq!(saved, vec![param(1, 0.7), param(3, 0.9), param(9, 0.4)]);
    // A batch of only known ids applies cleanly and asks for nothing.
    let mut fresh = vec![view(1, 0.2, 0.0)];
    let mut fresh_saved = Vec::new();
    let known = [ParamChange {
        id: 1,
        value: 0.3,
        display: None,
    }];
    assert!(apply_parameter_changes(
        &mut fresh,
        &mut fresh_saved,
        &known
    ));
    assert_eq!(fresh[0].value, 0.3);
    assert_eq!(fresh_saved, vec![param(1, 0.3)]);
}

#[test]
fn a_reported_change_back_to_default_removes_it_from_saved() {
    let mut parameters = vec![view(1, 0.6, 0.5), view(2, 0.4, 0.0)];
    let mut saved = vec![param(1, 0.6), param(2, 0.4)];
    let changes = [ParamChange {
        id: 1,
        value: 0.5,
        display: None,
    }];
    apply_parameter_changes(&mut parameters, &mut saved, &changes);
    assert_eq!(parameters[0].value, 0.5);
    // No text was reported, so the cached one is kept.
    assert_eq!(parameters[0].display, "");
    assert_eq!(saved, vec![param(2, 0.4)]);
}

#[test]
fn no_reported_changes_leave_everything_unchanged() {
    let mut parameters = vec![view(1, 0.6, 0.0)];
    let mut saved = vec![param(1, 0.6)];
    apply_parameter_changes(&mut parameters, &mut saved, &[]);
    assert_eq!(parameters[0].value, 0.6);
    assert_eq!(saved, vec![param(1, 0.6)]);
}

/// A slot holding a fake CLAP plugin, the way a real load fills one in.
fn fake_clap_slot(fail_process: bool) -> Slot {
    use super::super::clap::tests::fake_plugin_for_test;
    Slot::with_plugin_for_test(
        PluginPref::default(),
        fake_plugin_for_test(fail_process),
        48_000,
    )
}

/// A CLAP port change takes the insert out of the chain at once, without
/// an error, and keeps its latency counted while it waits for the reload.
#[test]
fn a_port_change_takes_the_insert_out_and_keeps_its_latency() {
    let slot = Slot::with_plugin_for_test(
        PluginPref::default(),
        super::super::clap::tests::fake_plugin_with_latency_for_test(32),
        48_000,
    );
    assert!(slot.active() && !slot.reload_wanted());
    super::super::clap::tests::rescan_ports_for_test(slot.plugin.as_ref().unwrap().handle());
    assert!(slot.reload_wanted());
    assert!(!slot.active());
    assert_eq!(slot.latency_samples(), 32);
    assert!(slot.resolved_error().is_none());
}

#[test]
fn a_failing_process_only_records_the_failure_and_the_view_formats_it() {
    use super::super::clap::tests::fake_plugin_for_test;
    use super::link::{ClapFailure, ProcessFailure};
    use super::PluginChain;

    let mut chain = PluginChain::new(48_000);
    chain.insert_clap_for_test(fake_plugin_for_test(true));
    assert_eq!(chain.active(), 1);
    let mut left = vec![0.5f32; 64];
    let mut right = vec![0.5f32; 64];
    assert!(chain.process(&mut left, &mut right, 64), "a slot failed");

    // The audio thread kept the plugin's own error and wrote no text.
    let slot = chain.slot_mut_for_test(0);
    assert_eq!(
        slot.plugin.as_ref().unwrap().shared().failure(),
        Some(ProcessFailure::Clap(ClapFailure::ProcessError))
    );
    assert!(slot.error.is_none());
    assert!(slot.failed());
    assert_eq!(chain.active(), 0, "the failed insert left the chain");
    // The block is the input.
    assert!(left.iter().chain(&right).all(|&sample| sample == 0.5));

    // The text is written when the UI thread builds the views.
    assert_eq!(
        chain.views()[0].error.as_deref(),
        Some("Processing failed: The CLAP plugin reported a processing error.")
    );
    // A failed insert is not run again, so it does not fail again.
    assert!(!chain.process(&mut left, &mut right, 64));
}

#[test]
fn a_vst3_processing_failure_is_formatted_from_its_error() {
    use super::link::ProcessFailure;

    let mut slot = fake_clap_slot(false);
    slot.plugin
        .as_ref()
        .unwrap()
        .shared()
        .fail(ProcessFailure::Vst3(1));
    assert!(slot.failed());
    assert_eq!(
        slot.view().error.as_deref(),
        Some("Processing failed: Plugin process() failed: 0x1")
    );
    // A load error still comes first.
    slot.error = Some("Failed to load".to_owned());
    assert_eq!(slot.resolved_error().as_deref(), Some("Failed to load"));
}

#[test]
fn the_chain_latency_counts_bypassed_inserts_but_not_failed_ones() {
    use super::super::clap::tests::{
        fake_plugin_with_latency_for_test, restart_fake_with_latency_for_test,
    };
    use super::PluginChain;
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
    chain
        .slot_mut_for_test(0)
        .plugin
        .as_ref()
        .unwrap()
        .shared()
        .fail(super::link::ProcessFailure::Clap(
            super::link::ClapFailure::ProcessError,
        ));
    assert_eq!(chain.latency_samples(), 96);

    let views = chain.views();
    let latencies: Vec<u32> = views.iter().map(|view| view.latency).collect();
    assert_eq!(latencies, vec![0, 32, 64, 0]);
    assert_eq!(views[2].latency_label().as_deref(), Some("· 64 smp"));
    assert_eq!(views[3].latency_label(), None);

    let mut left = vec![0.0f32; 32];
    let mut right = vec![0.0f32; 32];
    let services: Vec<_> = chain
        .slot_links()
        .into_iter()
        .filter_map(|link| link.plugin)
        .collect();
    let service_all = || {
        services
            .iter()
            .map(super::PluginHandle::service)
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
    // The main thread asks for a pause; the audio thread stops it first.
    // The latency has not moved yet.
    assert!(!service_all());
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

/// A rate rebuild restores what the handle saved since the load (the
/// slot's own pref no longer holds it), and refuses a plan the handle moved
/// past before the install.
#[test]
fn a_rate_rebuild_restores_what_the_handle_saved_since_load() {
    use super::super::clap::tests::{
        fake_plugin_with_state_for_test, fake_restoring_for_test, set_fake_state_for_test,
    };
    use super::PluginChain;

    let _main = crate::audio::claim_main_thread();
    let state_bytes = |pref: &PluginPref| pref.state.as_ref().and_then(PluginState::bytes);
    let mut chain = PluginChain::new(48_000);
    chain.insert_clap_for_test(fake_plugin_with_state_for_test(b"one"));
    let handle = chain.slot_links()[0].plugin.clone().expect("loaded");
    handle.mark_state_stale();
    handle.capture_state();
    assert!(!handle.wants_capture());

    let plan = chain.reload_plan(96_000).expect("a new rate reloads");
    assert_eq!(plan.jobs.len(), 1);
    assert_eq!(state_bytes(&plan.jobs[0].1).as_deref(), Some(&b"one"[..]));

    // The plugin's state moves on before the rebuilt slot is installed.
    set_fake_state_for_test(b"two");
    handle.mark_state_stale();
    handle.capture_state();
    let rebuild = |jobs: Vec<(usize, PluginPref)>| -> Vec<(usize, PluginPref, Slot)> {
        jobs.into_iter()
            .map(|(index, pref)| {
                let plugin = fake_restoring_for_test(&pref);
                let slot = Slot::with_plugin_for_test(pref.clone(), plugin, 96_000);
                (index, pref, slot)
            })
            .collect()
    };
    let refused = chain.install_reload(plan.previous_rate, 96_000, rebuild(plan.jobs));
    let Err(stale) = refused else {
        panic!("a plan older than the saved state was installed");
    };
    drop(stale);

    let plan = chain.reload_plan(96_000).expect("still a new rate");
    assert_eq!(state_bytes(&plan.jobs[0].1).as_deref(), Some(&b"two"[..]));
    let Ok(old) = chain.install_reload(plan.previous_rate, 96_000, rebuild(plan.jobs)) else {
        panic!("the fresh plan matches the chain");
    };
    drop(old);
    drop(handle);
    let rebuilt = chain.slot_links()[0].plugin.clone().expect("loaded");
    let (_, state) = rebuilt.saved();
    assert_eq!(
        state.as_ref().and_then(PluginState::bytes).as_deref(),
        Some(&b"two"[..])
    );
    drop(rebuilt);
    drop(chain.restore(Vec::new()));
}

/// An impulse at frame `at` in a block of `frames`.
fn impulse(frames: usize, at: usize) -> Vec<f32> {
    let mut block = vec![0.0f32; frames];
    block[at] = 1.0;
    block
}

#[test]
fn a_rate_reload_keeps_the_insert_counters() {
    use super::PluginChain;
    let mut chain = PluginChain::new(48_000);
    chain.insert_delay_for_test(16);
    let mut left = vec![0.0f32; 32];
    let mut right = vec![0.0f32; 32];
    chain.process(&mut left, &mut right, 32);
    let before = chain.slot_mut_for_test(0).controls.lock_stats();
    assert_eq!(before.attempts, 1, "the insert was tried once");

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
    // A bypassed or mix-0 insert never counts an attempt: the delay comes
    // from the dry fallback, not from a paused one.
    for index in 0..2 {
        assert_eq!(
            chain.slot_mut_for_test(index).controls.lock_stats(),
            crate::audio::LockStats::default(),
            "insert {index}"
        );
    }
}

/// A CLAP insert has no instance lock the audio thread could find busy:
/// its editor working on the UI side never makes a block skip it.  A
/// restart pauses it instead, and the paused block still comes out by the
/// insert's latency.
#[test]
fn a_clap_insert_keeps_processing_while_its_editor_works_and_keeps_its_latency_in_a_restart() {
    use super::super::clap::tests::{
        fake_plugin_with_latency_for_test, restart_fake_with_latency_for_test,
    };
    use super::PluginChain;
    let mut chain = PluginChain::new(48_000);
    chain.insert_clap_for_test(fake_plugin_with_latency_for_test(64));
    let mut left = vec![0.0f32; 128];
    let mut right = vec![0.0f32; 128];
    // A normal block: the fake writes its constant.
    chain.process(&mut left, &mut right, 128);
    assert_eq!(left[0], 1.0);

    let Some(handle) = chain.slot_links()[0].plugin.clone() else {
        panic!("a CLAP insert");
    };
    // Its editor holds the plugin's UI side: the block still runs it.
    let ran = handle.with_clap(|_editor| {
        let mut left = impulse(128, 36);
        let mut right = impulse(128, 36);
        chain.process(&mut left, &mut right, 128);
        left
    });
    assert_eq!(ran, Some(vec![1.0; 128]));
    // The normal block and the one under the editor: no skip.
    assert_eq!(
        chain.slot_mut_for_test(0).controls.lock_stats(),
        crate::audio::LockStats {
            attempts: 2,
            skips: 0,
        }
    );

    // A restart: the UI asks for a pause, the block passes through, 64
    // frames late.
    restart_fake_with_latency_for_test(64);
    assert!(!handle.service());
    let mut left = impulse(128, 36);
    let mut right = impulse(128, 36);
    chain.process(&mut left, &mut right, 128);
    // The switch from the plugin's output smooths its step over 2 ms
    // (96 frames) from the constant down; the impulse after it comes 64
    // frames late.
    assert_eq!(left[0], 1.0);
    assert!(left.windows(2).take(95).all(|pair| pair[1] < pair[0]));
    assert_eq!(left[96..], impulse(128, 100)[96..]);
    // The restart's pass-through is counted as a paused block.
    assert_eq!(
        chain.slot_mut_for_test(0).controls.lock_stats(),
        crate::audio::LockStats {
            attempts: 3,
            skips: 1,
        }
    );
    // Restarted and resumed: it processes again.
    handle.service();
    assert!(!handle.shared().restart_pending());
    chain.process(&mut left, &mut right, 128);
    assert_eq!(left[127], 1.0);
    drop(handle);
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

#[test]
fn chain_bypass_skips_processing_and_keeps_each_slots_bypass_and_latency() {
    use super::PluginChain;
    use std::sync::atomic::Ordering;

    let mut chain = PluginChain::new(48_000);
    let total = chain.latency_handle();
    chain.insert_delay_for_test(16);
    chain.insert_delay_for_test(8);
    chain.set_bypass(1, true);
    assert!(!chain.slot_mut_for_test(0).controls.bypass());
    assert!(chain.slot_mut_for_test(1).controls.bypass());
    assert_eq!(chain.latency_samples(), 24);
    assert_eq!(total.load(Ordering::Acquire), 24);

    chain.set_chain_bypass(true);
    let mut left = impulse(64, 0);
    let mut right = impulse(64, 1);
    assert!(!chain.process(&mut left, &mut right, 64));
    assert_eq!(left, impulse(64, 24));
    assert_eq!(right, impulse(64, 25));
    assert!(
        !chain.slot_mut_for_test(0).controls.bypass(),
        "the active insert's bypass is not written"
    );
    assert!(chain.slot_mut_for_test(1).controls.bypass());
    for index in 0..2 {
        assert_eq!(
            chain.slot_mut_for_test(index).controls.lock_stats(),
            crate::audio::LockStats::default(),
            "insert {index} did not run"
        );
    }
    assert_eq!(chain.latency_samples(), 24);
    assert_eq!(total.load(Ordering::Acquire), 24);

    chain.set_chain_bypass(false);
    let mut left = vec![0.0f32; 64];
    let mut right = vec![0.0f32; 64];
    assert!(!chain.process(&mut left, &mut right, 64));
    assert!(!chain.slot_mut_for_test(0).controls.bypass());
    assert!(chain.slot_mut_for_test(1).controls.bypass());
    // Turning chain bypass off runs the active insert again.
    assert_eq!(chain.slot_mut_for_test(0).controls.lock_stats().attempts, 1);
    assert_eq!(chain.slot_mut_for_test(1).controls.lock_stats().attempts, 0);
    assert_eq!(chain.latency_samples(), 24);
    assert_eq!(total.load(Ordering::Acquire), 24);
    drop(chain.restore(Vec::new()));
}

// Real VST3 plugins: Steinberg's SDK samples, built by
// `scripts/vst3-fixtures.sh` (see `super::fixtures`).  Each test goes through
// RigWerk's own entry points (`load_plugin`, `PathControls`, the saved
// `PluginPref`), not the VST3 host underneath, so they keep passing when
// that host is replaced.  With `RIGWERK_VST3_FIXTURES` unset they return at
// once.

/// The class ids a scan stores in [`PluginPref::class_id`] for the three
/// fixtures (32 hex digits, as `moduleinfo.json` writes them).  Saved
/// preferences hold these, so a new host has to open them.
const AGAIN_CLASS: &str = "84E8DE5F92554F5396FAE4133C935A18";
const ADELAY_CLASS: &str = "0CDBB66985D548A9BFD8371909D24BB3";
const HOST_CHECKER_CLASS: &str = "23FC190E02DD4499A8D2230E50617DA3";

const FIXTURES: [(&str, &str); 3] = [
    ("again", AGAIN_CLASS),
    ("adelay", ADELAY_CLASS),
    ("host-checker", HOST_CHECKER_CLASS),
];

/// The block the fixture tests process: well under [`super::MAX_BLOCK`].
const FIXTURE_BLOCK: usize = 256;

/// The peak of the quiet test tone.
const TONE_PEAK: f32 = 0.1;

/// The saved insert a scan would add for fixture `name`, or `None` when
/// the fixtures are not configured.
fn fixture_pref(name: &str, class_id: &str) -> Option<PluginPref> {
    let path = super::vst3_fixture(name)?;
    Some(PluginPref {
        path: path.to_string_lossy().into_owned(),
        format: PluginFormat::Vst3,
        class_id: Some(class_id.to_owned()),
        name: name.to_owned(),
        ..PluginPref::default()
    })
}

/// A 440 Hz tone at [`TONE_PEAK`], continuing from frame `start`.
fn tone(start: usize) -> Vec<f32> {
    (start..start + FIXTURE_BLOCK)
        .map(|frame| TONE_PEAK * (frame as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin())
        .collect()
}

/// Runs `blocks` blocks of the tone through the path's chain, the way the
/// output callback does.  Fails on a missed chain lock (nothing else holds
/// it here) and on a sample that is not finite.  Returns the last block's
/// peak.
fn run_tone(path: &crate::audio::path::PathControls, blocks: usize) -> f32 {
    let mut peak = 0.0f32;
    for block in 0..blocks {
        let mut left = tone(block * FIXTURE_BLOCK);
        let mut right = left.clone();
        assert!(
            path.process_plugins(&mut left, &mut right, FIXTURE_BLOCK),
            "the chain lock was busy"
        );
        assert!(
            left.iter().chain(&right).all(|sample| sample.is_finite()),
            "block {block} is not finite"
        );
        peak = left
            .iter()
            .chain(&right)
            .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
    }
    peak
}

/// A path at 48 kHz with `pref` added the way the Plugins page adds it.
fn fixture_path(pref: PluginPref) -> crate::audio::path::PathControls {
    let path = crate::audio::path::PathControls::new(48_000);
    path.add_plugin(pref).expect("add the fixture");
    path
}

/// The only insert's published view, with its error checked.
fn only_view(path: &crate::audio::path::PathControls) -> super::PluginView {
    let views = path.plugin_views();
    assert_eq!(views.len(), 1);
    let view = views[0].clone();
    assert_eq!(view.error, None, "{}", view.name);
    view
}

/// The id of the control called `name`.
fn param_id(view: &super::PluginView, name: &str) -> u32 {
    view.parameters
        .iter()
        .find(|parameter| parameter.name == name)
        .unwrap_or_else(|| panic!("{} has no {name} control", view.name))
        .id
}

/// The normalized value of the control called `name`.
fn param_value(parameters: &[PluginParamView], name: &str) -> f64 {
    parameters
        .iter()
        .find(|parameter| parameter.name == name)
        .unwrap_or_else(|| panic!("no {name} control"))
        .value
}

#[test]
fn vst3_fixture_each_sample_loads_lists_controls_and_processes() {
    let _main = crate::audio::claim_main_thread();
    for (name, class_id) in FIXTURES {
        let Some(pref) = fixture_pref(name, class_id) else {
            return;
        };
        let (handle, _) = super::load::load_plugin(&pref, 48_000)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        assert!(
            !handle.handle().parameters().is_empty(),
            "{name} lists no controls"
        );
        drop(handle);

        let path = fixture_path(pref);
        assert!(!only_view(&path).parameters.is_empty(), "{name}");
        run_tone(&path, 16);
        // A process failure is recorded on the audio thread and published
        // by the UI's service pass.
        path.service_plugins();
        only_view(&path);
    }
}

#[test]
fn vst3_fixture_again_gain_sets_the_output_level() {
    let _main = crate::audio::claim_main_thread();
    let Some(pref) = fixture_pref("again", AGAIN_CLASS) else {
        return;
    };
    let path = fixture_path(pref);
    let view = only_view(&path);
    let gain = param_id(&view, "Gain");
    // AGain's processor multiplies by the normalized value and opens at
    // 1.0, although the control's default is 0.5.
    assert!((param_value(&view.parameters, "Gain") - 1.0).abs() < 1e-6);
    let unity = run_tone(&path, 8);
    assert!((unity - TONE_PEAK).abs() < 0.01, "unity gain peak {unity}");

    path.set_plugin_parameter(0, gain, 0.5).expect("set Gain");
    let half = run_tone(&path, 8);
    assert!(
        (half - TONE_PEAK * 0.5).abs() < 0.01,
        "half gain peak {half}"
    );
    assert!((param_value(&only_view(&path).parameters, "Gain") - 0.5).abs() < 1e-6);

    path.set_plugin_parameter(0, gain, 0.0).expect("set Gain");
    let silent = run_tone(&path, 8);
    assert!(silent < 1e-6, "zero gain peak {silent}");
}

/// The saved-state envelope a VST3 insert's [`PluginState`] holds: 16 bytes
/// of magic, then little-endian u32 version (1), component length and
/// controller length (`u32::MAX` for none), then the two payloads.  Saved
/// preferences carry it, so a new host has to keep reading it.
const STATE_MAGIC: &[u8; 16] = b"VST3HOST_STATE\0\0";
const STATE_HEADER: usize = 28;

fn le_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

#[test]
fn vst3_fixture_again_state_round_trips_and_keeps_its_envelope() {
    let _main = crate::audio::claim_main_thread();
    let Some(pref) = fixture_pref("again", AGAIN_CLASS) else {
        return;
    };
    let path = fixture_path(pref);
    let gain = param_id(&only_view(&path), "Gain");
    path.set_plugin_parameter(0, gain, 0.25).expect("set Gain");
    run_tone(&path, 2);
    // What a preference save stores: the parameter and a fresh blob.
    let saved = path.plugin_prefs();
    assert_eq!(saved.len(), 1);
    let saved = saved.into_iter().next().expect("one insert");
    assert_eq!(saved.class_id.as_deref(), Some(AGAIN_CLASS));
    assert!(saved
        .params
        .iter()
        .any(|param| param.id == gain && (param.value - 0.25).abs() < 1e-6));
    let state = saved.state.clone().expect("a captured state");
    assert_eq!(state.version, PLUGIN_STATE_VERSION);
    let blob = state.bytes().expect("the blob decodes");

    assert_eq!(&blob[..16], STATE_MAGIC);
    assert_eq!(le_u32(&blob, 16), 1, "envelope version");
    let component = le_u32(&blob, 20) as usize;
    let controller = le_u32(&blob, 24);
    let controller_len = if controller == u32::MAX {
        0
    } else {
        controller as usize
    };
    assert!(component > 0, "AGain writes a component state");
    assert_eq!(blob.len(), STATE_HEADER + component + controller_len);

    // A session restore: the saved insert reopens with the same value.
    let restored = crate::audio::path::PathControls::new(48_000);
    restored.restore_plugins(vec![saved.clone()]);
    restored.ensure_plugin_loaded(0);
    let view = only_view(&restored);
    assert!((param_value(&view.parameters, "Gain") - 0.25).abs() < 1e-6);
    let peak = run_tone(&restored, 8);
    assert!(
        (peak - TONE_PEAK * 0.25).abs() < 0.01,
        "restored peak {peak}"
    );

    // The blob alone restores the value, with no saved parameters.
    let blob_only = PluginPref {
        params: Vec::new(),
        state: Some(PluginState::capture(&blob, &[])),
        ..saved.clone()
    };
    let (handle, current) = super::load::load_plugin(&blob_only, 48_000).expect("load");
    assert!(current, "the envelope restored");
    let parameters = handle.handle().parameters();
    assert!((param_value(&parameters, "Gain") - 0.25).abs() < 1e-6);
    drop(handle);

    // An envelope built here, not by the writer: the reader is pinned on
    // its own.  The writer is pinned above, and a new host has to keep
    // writing this layout too, because an older RigWerk reads any other
    // blob as raw component state.
    let component_bytes = &blob[STATE_HEADER..STATE_HEADER + component];
    let mut built = STATE_MAGIC.to_vec();
    built.extend_from_slice(&1u32.to_le_bytes());
    built.extend_from_slice(&(component_bytes.len() as u32).to_le_bytes());
    built.extend_from_slice(&u32::MAX.to_le_bytes());
    built.extend_from_slice(component_bytes);
    let hand_built = PluginPref {
        params: Vec::new(),
        state: Some(PluginState::capture(&built, &[])),
        ..saved.clone()
    };
    let (handle, current) = super::load::load_plugin(&hand_built, 48_000).expect("load");
    assert!(current, "the hand-built envelope restored");
    let parameters = handle.handle().parameters();
    assert!((param_value(&parameters, "Gain") - 0.25).abs() < 1e-6);
    drop(handle);

    // A blob saved before the envelope was the raw component state.
    let legacy = PluginPref {
        params: Vec::new(),
        state: Some(PluginState::capture(component_bytes, &[])),
        ..saved
    };
    let (handle, current) = super::load::load_plugin(&legacy, 48_000).expect("load");
    assert!(current, "the raw component state restored");
    let parameters = handle.handle().parameters();
    assert!((param_value(&parameters, "Gain") - 0.25).abs() < 1e-6);
}

/// A value set after the load survives a rate change (the rebuilt AGain
/// opens with it), and a value set between a reload's plan and its install
/// makes the install refuse the stale plan.
#[test]
fn vst3_fixture_again_a_rate_reload_keeps_values_set_since_load() {
    let _main = crate::audio::claim_main_thread();
    let Some(pref) = fixture_pref("again", AGAIN_CLASS) else {
        return;
    };
    let path = fixture_path(pref);
    let gain = param_id(&only_view(&path), "Gain");
    path.set_plugin_parameter(0, gain, 0.25).expect("set Gain");
    path.reload_plugins_for_rate(96_000);
    assert_eq!(path.chain_rate(), 96_000);
    let view = only_view(&path);
    assert!((param_value(&view.parameters, "Gain") - 0.25).abs() < 1e-6);
    let (saved, _) = path.plugin_handle(0).expect("loaded").saved();
    assert!(saved
        .iter()
        .any(|param| param.id == gain && (param.value - 0.25).abs() < 1e-6));
    let peak = run_tone(&path, 8);
    assert!(
        (peak - TONE_PEAK * 0.25).abs() < 0.01,
        "rebuilt peak {peak}"
    );

    let plan = path
        .edit_chain(|chain| chain.reload_plan(48_000))
        .flatten()
        .expect("a new rate reloads");
    path.set_plugin_parameter(0, gain, 0.75).expect("set Gain");
    let built: Vec<_> = plan
        .jobs
        .into_iter()
        .map(|(index, pref)| {
            let (slot, _) = super::PluginChain::load_slot(pref.clone(), 48_000);
            (index, pref, slot)
        })
        .collect();
    let installed = path
        .edit_chain(|chain| chain.install_reload(plan.previous_rate, 48_000, built))
        .expect("the chain is not poisoned");
    assert!(installed.is_err(), "a stale plan was installed");
    drop(installed);
    assert_eq!(path.chain_rate(), 96_000);
}

/// ADelay's delay is the normalized value times the rate, in samples
/// (`adelayprocessor.cpp`), so an impulse has to come out that many frames
/// later through the chain.
#[test]
fn vst3_fixture_adelay_delays_an_impulse_by_its_delay() {
    const DELAY: usize = 256;
    let _main = crate::audio::claim_main_thread();
    let Some(pref) = fixture_pref("adelay", ADELAY_CLASS) else {
        return;
    };
    let path = fixture_path(pref);
    let delay = param_id(&only_view(&path), "Delay");
    // Half a sample over, so the plugin's truncation lands on DELAY.
    let value = (DELAY as f64 + 0.5) / 48_000.0;
    path.set_plugin_parameter(0, delay, value)
        .expect("set Delay");
    let mut output = Vec::new();
    for block in 0..3 {
        let mut left = vec![0.0f32; FIXTURE_BLOCK];
        // The first block only delivers the new delay.
        if block == 1 {
            left[0] = 0.25;
        }
        let mut right = left.clone();
        assert!(path.process_plugins(&mut left, &mut right, FIXTURE_BLOCK));
        assert_eq!(left, right);
        output.extend(left);
    }
    let peak = output
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
        .map(|(frame, sample)| (frame, *sample));
    assert_eq!(peak, Some((FIXTURE_BLOCK + DELAY, 0.25)));
    path.service_plugins();
    assert_eq!(only_view(&path).latency, 0, "ADelay reports no latency");
}

/// The host checker logs the host's call sequence for its own editor; none
/// of that is readable as a control, and its "Scoring" counts supported
/// features, not errors.  So this is mostly a call-sequence test: a load,
/// many blocks, a latency change, a state capture and a restore, and
/// nothing may fail.
///
/// Its "Latency" control (0..10 s) reaches the processor, which reports it
/// on an output parameter; the controller gets that and asks for a
/// `kLatencyChanged` restart, and the plugin applies the new latency on
/// activation.  So the latency only reads 256 if output parameters reach
/// the controller and the restart reads it after reactivating.
#[test]
fn vst3_fixture_host_checker_survives_processing_and_a_state_round_trip() {
    let _main = crate::audio::claim_main_thread();
    let Some(pref) = fixture_pref("host-checker", HOST_CHECKER_CLASS) else {
        return;
    };
    let path = fixture_path(pref);
    run_tone(&path, 100);
    path.service_plugins();
    let view = only_view(&path);
    if let Some(score) = view
        .parameters
        .iter()
        .find(|parameter| parameter.name == "Scoring")
    {
        eprintln!("[vst3_fixture] host-checker Scoring: {}", score.display);
    }
    let latency = param_id(&view, "Latency");
    path.set_plugin_parameter(0, latency, 256.5 / (10.0 * 48_000.0))
        .expect("set Latency");
    for _ in 0..10 {
        run_tone(&path, 4);
        path.service_plugins();
        if only_view(&path).latency == 256 {
            break;
        }
    }
    let reported = only_view(&path).latency;
    eprintln!("[vst3_fixture] host-checker latency after asking for 256: {reported}");
    assert_eq!(reported, 256);
    // It processes again after the restart.
    run_tone(&path, 8);
    path.service_plugins();
    assert_eq!(only_view(&path).latency, 256);
    let saved = path.plugin_prefs();
    assert!(saved[0].state.is_some(), "the host checker saves a state");

    let restored = crate::audio::path::PathControls::new(48_000);
    restored.restore_plugins(saved);
    restored.ensure_plugin_loaded(0);
    only_view(&restored);
    run_tone(&restored, 100);
    restored.service_plugins();
    only_view(&restored);
}

/// A latency restart while an audio thread processes the host checker
/// waits for that thread: the UI asks for a pause, the audio side stops
/// the plugin with its next block and skips it until the UI has
/// reactivated it, and then it runs again.  No block fails.
#[test]
fn vst3_fixture_restart_waits_for_the_audio_thread() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    let _main = crate::audio::claim_main_thread();
    let Some(pref) = fixture_pref("host-checker", HOST_CHECKER_CLASS) else {
        return;
    };
    let path = fixture_path(pref);
    let latency = param_id(&only_view(&path), "Latency");
    let handle = path.plugin_handle(0).expect("loaded");
    let stop = AtomicBool::new(false);
    let reported = std::thread::scope(|scope| {
        let audio = scope.spawn(|| {
            while !stop.load(Ordering::Relaxed) {
                let mut left = tone(0);
                let mut right = left.clone();
                assert!(path.process_plugins(&mut left, &mut right, FIXTURE_BLOCK));
                assert!(left.iter().chain(&right).all(|sample| sample.is_finite()));
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        // Started and processing before the request.
        let since = Instant::now();
        while !handle.shared().pause.processing() {
            assert!(since.elapsed() < Duration::from_secs(5), "never started");
            std::thread::sleep(Duration::from_millis(1));
        }
        let paused_before = handle.shared().pause.paused_blocks();
        path.set_plugin_parameter(0, latency, 256.5 / (10.0 * 48_000.0))
            .expect("set Latency");
        let since = Instant::now();
        while only_view(&path).latency != 256 && since.elapsed() < Duration::from_secs(5) {
            path.service_plugins();
            std::thread::sleep(Duration::from_millis(5));
        }
        let paused = handle.shared().pause.paused_blocks() - paused_before;
        // It runs again: blocks are served and none pauses any more.
        let served = handle.shared().pause.served();
        let since = Instant::now();
        while handle.shared().pause.served() < served + 8 {
            assert!(since.elapsed() < Duration::from_secs(5), "no blocks after");
            std::thread::sleep(Duration::from_millis(1));
        }
        let after = handle.shared().pause.paused_blocks();
        std::thread::sleep(Duration::from_millis(20));
        let resumed = handle.shared().pause.paused_blocks() == after;
        stop.store(true, Ordering::Relaxed);
        audio.join().expect("the audio side panicked");
        (paused, resumed)
    });
    path.service_plugins();
    assert_eq!(handle.failure(), None);
    assert_eq!(only_view(&path).latency, 256);
    let (paused, resumed) = reported;
    assert!(paused > 0, "the audio side never paused the plugin");
    assert!(resumed, "the plugin stayed paused");
    assert!(handle.shared().pause.processing(), "not processing again");
}

/// A value set while the insert is bypassed reaches the processor through
/// an empty flush block, so the state saved afterwards holds it, with no
/// saved parameter to help.  That state also survives a rate reload.
#[test]
fn vst3_fixture_again_a_value_set_while_bypassed_reaches_the_state() {
    let _main = crate::audio::claim_main_thread();
    let Some(pref) = fixture_pref("again", AGAIN_CLASS) else {
        return;
    };
    let path = fixture_path(pref);
    let gain = param_id(&only_view(&path), "Gain");
    run_tone(&path, 2);
    path.set_plugin_bypass(0, true);
    // The bypassed chain no longer runs on its own (no latency to keep).
    run_tone(&path, 2);
    path.set_plugin_parameter(0, gain, 0.25).expect("set Gain");
    let bypassed = run_tone(&path, 2);
    assert!(
        (bypassed - TONE_PEAK).abs() < 0.01,
        "bypassed peak {bypassed}"
    );
    let saved = path.plugin_prefs().into_iter().next().expect("one insert");
    let blob = saved
        .state
        .as_ref()
        .and_then(PluginState::bytes)
        .expect("a captured state");

    // The blob alone, with no saved parameters and an empty snapshot.
    let blob_only = PluginPref {
        params: Vec::new(),
        state: Some(PluginState::capture(&blob, &[])),
        bypass: false,
        ..saved
    };
    let restored = fixture_path(blob_only);
    assert!((param_value(&only_view(&restored).parameters, "Gain") - 0.25).abs() < 1e-6);
    restored.reload_plugins_for_rate(96_000);
    assert_eq!(restored.chain_rate(), 96_000);
    assert!((param_value(&only_view(&restored).parameters, "Gain") - 0.25).abs() < 1e-6);
    let peak = run_tone(&restored, 8);
    assert!(
        (peak - TONE_PEAK * 0.25).abs() < 0.01,
        "restored peak {peak}"
    );
}

/// An edit AGain's editor reports (`performEdit`) is the user's setting:
/// the next service frame saves it and marks the state for capture.
#[test]
fn vst3_fixture_again_an_editor_edit_is_saved_by_the_service() {
    let _main = crate::audio::claim_main_thread();
    let Some(pref) = fixture_pref("again", AGAIN_CLASS) else {
        return;
    };
    let path = fixture_path(pref);
    let gain = param_id(&only_view(&path), "Gain");
    run_tone(&path, 2);
    path.capture_plugin_states();
    let handle = path.plugin_handle(0).expect("loaded");
    assert!(!handle.wants_capture());
    assert!(handle.edit_for_test(gain, 0.3));
    path.service_plugins();
    let (saved, _) = handle.saved();
    assert!(saved
        .iter()
        .any(|param| param.id == gain && (param.value - 0.3).abs() < 1e-6));
    assert!(handle.wants_capture());
    assert!((param_value(&only_view(&path).parameters, "Gain") - 0.3).abs() < 1e-6);
    let peak = run_tone(&path, 4);
    assert!((peak - TONE_PEAK * 0.3).abs() < 0.01, "edited peak {peak}");
}

/// With no audio running a value set on AGain never reaches its
/// processor, but a save still captures its state; the restore applies
/// the value on top.
#[test]
fn vst3_fixture_again_a_save_with_audio_stopped_keeps_a_value_set() {
    let _main = crate::audio::claim_main_thread();
    let Some(pref) = fixture_pref("again", AGAIN_CLASS) else {
        return;
    };
    let path = fixture_path(pref);
    let gain = param_id(&only_view(&path), "Gain");
    path.set_plugin_parameter(0, gain, 0.3).expect("set Gain");
    let saved = path.plugin_prefs();
    assert!(saved[0].state.is_some(), "captured without audio");
    let restored = crate::audio::path::PathControls::new(48_000);
    restored.restore_plugins(saved);
    restored.ensure_plugin_loaded(0);
    assert!((param_value(&only_view(&restored).parameters, "Gain") - 0.3).abs() < 1e-6);
    let peak = run_tone(&restored, 8);
    assert!(
        (peak - TONE_PEAK * 0.3).abs() < 0.01,
        "restored peak {peak}"
    );
}

/// AGain asked to be reloaded (`kReloadComponent`, raised here on its
/// behalf): the path rebuilds it from its state and saved values, and the
/// new instance processes with the gain set before.
#[test]
fn vst3_fixture_again_a_reload_request_rebuilds_it_with_its_values() {
    let _main = crate::audio::claim_main_thread();
    let Some(pref) = fixture_pref("again", AGAIN_CLASS) else {
        return;
    };
    let path = fixture_path(pref);
    let gain = param_id(&only_view(&path), "Gain");
    path.set_plugin_parameter(0, gain, 0.25).expect("set Gain");
    run_tone(&path, 2);
    let old = path.plugin_handle(0).expect("loaded");
    old.shared()
        .request_restart(crate::audio::vst3::RELOAD_COMPONENT);
    path.service_plugins();
    let new = path.plugin_handle(0).expect("reloaded");
    assert_ne!(new.instance_id(), old.instance_id());
    drop(old);
    let view = only_view(&path);
    assert!((param_value(&view.parameters, "Gain") - 0.25).abs() < 1e-6);
    let peak = run_tone(&path, 8);
    assert!(
        (peak - TONE_PEAK * 0.25).abs() < 0.01,
        "reloaded peak {peak}"
    );
}

/// The audio side processes AGain as fast as it can while the UI side moves
/// its gain through the normal control path.  Neither the chain nor the
/// insert is ever skipped: a control set takes no lock the audio thread
/// takes.  The output must stay finite; the counts are printed so a CI log
/// records them.
#[test]
fn vst3_fixture_again_keeps_processing_while_the_ui_sets_controls() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    let _main = crate::audio::claim_main_thread();
    let Some(pref) = fixture_pref("again", AGAIN_CLASS) else {
        return;
    };
    let path = fixture_path(pref);
    let gain = param_id(&only_view(&path), "Gain");
    path.reset_plugin_lock_counters();
    /// Stops the audio side however the UI loop ends, so a panic there
    /// cannot leave the scope waiting on it forever.
    struct StopOnDrop<'a>(&'a AtomicBool);
    impl Drop for StopOnDrop<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    let stop = AtomicBool::new(false);
    let mut sets = 0u64;
    let mut failures = Vec::new();
    let (blocks, missed) = std::thread::scope(|scope| {
        let audio = scope.spawn(|| {
            let (mut blocks, mut missed) = (0u64, 0u64);
            while !stop.load(Ordering::Relaxed) {
                let mut left = tone(0);
                let mut right = left.clone();
                if !path.process_plugins(&mut left, &mut right, FIXTURE_BLOCK) {
                    missed += 1;
                }
                assert!(left.iter().chain(&right).all(|sample| sample.is_finite()));
                blocks += 1;
            }
            (blocks, missed)
        });
        let stopper = StopOnDrop(&stop);
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(300) {
            let value = if sets.is_multiple_of(2) { 0.2 } else { 0.8 };
            if let Err(error) = path.set_plugin_parameter(0, gain, value) {
                failures.push(error);
                break;
            }
            sets += 1;
            if sets.is_multiple_of(64) {
                path.service_plugins();
            }
        }
        drop(stopper);
        audio.join().expect("the audio side panicked")
    });
    let report = path.plugin_lock_report();
    let insert = report.inserts[0];
    eprintln!(
        "[vst3_fixture] lock baseline: {sets} sets, {blocks} blocks ({missed} without the chain); \
         chain lock {}/{} skipped, insert {}/{} paused ({:.1}%)",
        report.chain.skipped(),
        report.chain.attempts,
        insert.skipped(),
        insert.attempts,
        insert.share() * 100.0
    );
    assert_eq!(failures, Vec::<String>::new(), "a control set failed");
    assert!(blocks > 0 && sets > 0);
    assert_eq!(missed, 0, "a block missed the chain");
    assert_eq!(report.chain.skipped(), 0, "the chain lock was skipped");
    assert_eq!(insert.skipped(), 0, "a control set paused the insert");
    path.service_plugins();
    only_view(&path);
}

#[test]
fn a_latency_total_the_ui_stored_meanwhile_is_not_overwritten() {
    use super::chain::publish_total;
    use std::sync::atomic::{AtomicU32, Ordering};

    let cell = AtomicU32::new(16);
    // The audio thread read 16 and summed 32; the UI stored 48 meanwhile.
    cell.store(48, Ordering::Release);
    assert!(!publish_total(&cell, 16, 32));
    assert_eq!(cell.load(Ordering::Acquire), 48, "the newer value wins");
    // Nothing in between: the sum goes in and counts as a change.
    assert!(publish_total(&cell, 48, 32));
    assert_eq!(cell.load(Ordering::Acquire), 32);
    // The same total again is no change.
    assert!(!publish_total(&cell, 32, 32));
    assert_eq!(cell.load(Ordering::Acquire), 32);
}
