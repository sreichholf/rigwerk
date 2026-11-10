use std::sync::atomic::Ordering;

use crate::audio::CushionMode;

/// A bridge like the BT-W3 case: 128-sample input, 480-sample output step.
fn cushion_bridge(mode: CushionMode, manual_ms: f32) -> super::Shared {
    let shared = super::Shared::new(48_000);
    shared.set_cushion(mode, manual_ms);
    shared.set_source_rate(48_000);
    shared.set_target_frames(super::initial_target(128));
    shared.note_input_block(128);
    shared.note_output_block(480);
    shared
}

#[test]
fn scope_lane_keeps_peak_hold_bins_in_order() {
    let shared = super::Shared::new(48_000);
    shared.set_source_rate(48_000);
    let hop = ((48_000u64 * 250) / 1000 / super::SCOPE_LEN as u64).clamp(1, 512) as usize;
    // Quiet samples, then a louder one that must win the first closed bin.
    for _ in 0..hop - 1 {
        shared.push_frame(0.1, 0.1);
    }
    shared.push_frame(0.8, 0.8);
    for _ in 0..hop {
        shared.push_frame(-0.5, -0.5);
    }
    let mut input = vec![0.0; super::SCOPE_LEN];
    let mut output = vec![0.0; super::SCOPE_LEN];
    shared.copy_scope(&mut input, &mut output);
    assert!(
        (input[super::SCOPE_LEN - 2] - 0.8).abs() < 1.0e-5,
        "first closed bin should hold the louder sample, got {}",
        input[super::SCOPE_LEN - 2]
    );
    assert!(
        (input[super::SCOPE_LEN - 1] + 0.5).abs() < 1.0e-5,
        "second bin should keep sign of the peak, got {}",
        input[super::SCOPE_LEN - 1]
    );
    assert!(output.iter().all(|sample| *sample == 0.0));
}

#[test]
fn safe_cushion_keeps_two_output_blocks_and_grows_256() {
    let shared = cushion_bridge(CushionMode::Safe, 10.0);
    assert_eq!(shared.target_frames(), 960);
    shared.raise_target_after_underrun();
    assert_eq!(shared.target_frames(), 1216);
    shared.relax_target();
    assert_eq!(shared.target_frames(), 1152);
}

#[test]
fn set_output_lifts_the_safe_cushion_for_a_large_exclusive_step() {
    // Bridge opens output before input has filled anything; exclusive may
    // wake with a 10 ms (or larger) GetBufferSize.  The cushion must already
    // cover two of those steps or the first audible block underruns.
    let shared = super::Shared::new(48_000);
    shared.set_cushion(CushionMode::Safe, 10.0);
    shared.set_source_rate(48_000);
    shared.set_target_frames(super::initial_target(128));
    shared.note_input_block(128);
    assert_eq!(shared.target_frames(), 256);
    shared.set_output(48_000, 480, "Exclusive · test");
    assert_eq!(shared.target_frames(), 960);
    assert_eq!(shared.output_step_frames.load(Ordering::Relaxed), 480);
}

#[test]
fn low_latency_cushion_is_one_block_from_each_side() {
    let shared = cushion_bridge(CushionMode::LowLatency, 10.0);
    assert_eq!(shared.target_frames(), 608);
    shared.raise_target_after_underrun();
    assert_eq!(shared.target_frames(), 728);
    shared.relax_target();
    assert_eq!(shared.target_frames(), 696);
}

#[test]
fn manual_cushion_stays_where_it_is_put() {
    let shared = cushion_bridge(CushionMode::Manual, 7.5);
    assert_eq!(shared.target_frames(), 360);
    shared.raise_target_after_underrun();
    assert_eq!(shared.target_frames(), 360);
    // Changing the size while running applies at once.
    shared.set_cushion(CushionMode::Manual, 12.0);
    assert_eq!(shared.target_frames(), 576);
}

#[test]
fn switching_mode_while_running_applies_at_once() {
    let shared = cushion_bridge(CushionMode::Safe, 10.0);
    shared.raise_target_after_underrun();
    shared.set_cushion(CushionMode::LowLatency, 10.0);
    assert_eq!(shared.target_frames(), 608);
}

/// The CLAP main thread is process-wide.  These cases claim it for the
/// calling thread and must not overlap.  The thread id is restored
/// before the serial lock is released.
pub(crate) fn claim_main_thread() -> MainClaim {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let serial = SERIAL.lock().unwrap_or_else(|poison| poison.into_inner());
    MainClaim {
        _thread: crate::audio::clap::claim_main_thread_for_test(),
        _serial: serial,
    }
}

pub(crate) struct MainClaim {
    // Dropped first, while `serial` is still held.
    _thread: crate::audio::clap::MainThreadClaim,
    _serial: std::sync::MutexGuard<'static, ()>,
}

fn chain_rate(shared: &super::Shared) -> u32 {
    shared.paths[0]
        .plugins
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .rate()
}

fn built_rates(shared: &super::Shared) -> Vec<Option<u32>> {
    shared.paths[0]
        .plugins
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .built_rates()
}

#[test]
fn publishing_output_rate_reloads_only_on_the_main_thread() {
    let _main = claim_main_thread();
    let shared = super::Shared::new(48_000);
    shared.restore_plugins(vec![crate::audio::PluginPref {
        path: "missing-rate-probe.vst3".to_owned(),
        ..crate::audio::PluginPref::default()
    }]);
    assert_eq!(chain_rate(&shared), 48_000);
    assert_eq!(built_rates(&shared), vec![None]);

    // Publishing the device rate does not itself rebuild the chain.
    shared.set_output(44_100, 128, "test output");
    assert_eq!(chain_rate(&shared), 48_000);
    assert_eq!(built_rates(&shared), vec![None]);

    std::thread::scope(|scope| {
        scope.spawn(|| shared.align_plugins_to_output_rate());
    });
    assert_eq!(
        built_rates(&shared),
        vec![None],
        "a bridge thread must not open the empty slot"
    );

    // The follow-up `service_plugins` calls, invoked directly.
    shared.align_plugins_to_output_rate();
    assert_eq!(chain_rate(&shared), 44_100);
    assert_eq!(built_rates(&shared), vec![Some(44_100)]);

    // A later device rate still waits for that follow-up, including the
    // one `service_plugins` runs each frame.
    shared.set_output(48_000, 128, "test output");
    assert_eq!(chain_rate(&shared), 44_100);
    assert_eq!(built_rates(&shared), vec![Some(44_100)]);
    shared.service_plugins();
    assert_eq!(chain_rate(&shared), 48_000);
    assert_eq!(built_rates(&shared), vec![Some(48_000)]);
}

#[test]
fn off_thread_drop_defers_plugin_slots_until_the_main_thread() {
    let _main = claim_main_thread();
    let plugins = crate::audio::plugins::tracked_slots_alive;
    let queued = crate::audio::plugins::deferred_slot_count;

    // The UI thread drops the chain immediately.
    {
        let shared = super::Shared::new(48_000);
        shared.paths[0]
            .plugins
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert_tracked_slot_for_test();
        assert_eq!(plugins(), 1);
        drop(shared);
        assert_eq!(plugins(), 0);
        assert_eq!(queued(), 0);
    }

    let shared = super::Shared::new(48_000);
    shared.paths[0]
        .plugins
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .insert_tracked_slot_for_test();
    assert_eq!(plugins(), 1);
    std::thread::scope(|scope| {
        scope.spawn(move || drop(shared));
    });
    assert_eq!(plugins(), 1, "the slot stays alive on the deferred queue");
    assert_eq!(queued(), 1);
    crate::audio::plugins::drain_deferred_plugins();
    assert_eq!(plugins(), 0, "the main thread drops the deferred slot");
    assert_eq!(queued(), 0);
}

fn missing(path: &str) -> crate::audio::PluginPref {
    crate::audio::PluginPref {
        path: path.to_owned(),
        name: path.to_owned(),
        ..crate::audio::PluginPref::default()
    }
}

fn names(shared: &super::Shared) -> Vec<String> {
    shared
        .plugin_views()
        .iter()
        .map(|view| view.name.clone())
        .collect()
}

/// Asserts that a publish happened since `last` and remembers it.
fn assert_published(shared: &super::Shared, last: &mut u64) {
    let version = shared.plugin_views_version();
    assert!(version > *last, "the edit did not republish the views");
    *last = version;
}

#[test]
fn published_views_follow_every_chain_edit() {
    let _main = claim_main_thread();
    let shared = super::Shared::new(48_000);
    assert!(shared.plugin_views().is_empty());
    let mut version = shared.plugin_views_version();

    shared.restore_plugins(vec![missing("a.vst3"), missing("b.vst3")]);
    assert_published(&shared, &mut version);
    assert_eq!(names(&shared), ["a.vst3", "b.vst3"]);

    shared.set_plugin_bypass(1, true);
    assert_published(&shared, &mut version);
    assert!(shared.plugin_views()[1].bypass);

    shared.set_plugin_mix(0, 0.25);
    assert_published(&shared, &mut version);
    assert_eq!(shared.plugin_views()[0].mix, 0.25);

    shared.move_plugin(0, 1);
    assert_published(&shared, &mut version);
    assert_eq!(names(&shared), ["b.vst3", "a.vst3"]);

    // Opening a slot whose file is missing leaves its error visible.
    shared.ensure_plugin_loaded(0);
    assert_published(&shared, &mut version);
    assert!(shared.plugin_views()[0].error.is_some());

    assert!(shared.set_plugin_parameter(1, 7, 0.5).is_err());
    assert_published(&shared, &mut version);
    assert!(shared.plugin_views()[1].error.is_some());

    shared.remove_plugin(0);
    assert_published(&shared, &mut version);
    assert_eq!(names(&shared), ["a.vst3"]);

    // A failed add stays in the list with its error.
    assert!(shared.add_plugin(missing("c.vst3")).is_err());
    assert_published(&shared, &mut version);
    assert_eq!(names(&shared), ["a.vst3", "c.vst3"]);
    assert!(shared.plugin_views()[1].error.is_some());

    // A snapshot the UI already holds is not changed by later edits.
    let held = shared.plugin_views();
    shared.restore_plugins(Vec::new());
    assert_eq!(held.len(), 2);
    assert!(shared.plugin_views().is_empty());
}

/// Holds the chain lock on this thread while the UI-side calls run on a
/// worker that stands in for the main thread.  None of them may wait for
/// the lock: the worker must finish while it is still held.
#[test]
fn drawing_and_quiet_service_frames_never_wait_for_the_chain_lock() {
    let _main = claim_main_thread();
    let shared = super::Shared::new(48_000);
    shared.restore_plugins(vec![missing("a.vst3")]);
    shared.set_output(48_000, 128, "test output");
    // Opens the restored slot (it fails), so the chain is settled.
    shared.service_plugins();
    assert!(shared.plugin_views()[0].error.is_some());
    shared.paths[0].edit_chain(|chain| {
        chain.insert_clap_for_test(crate::audio::clap::tests::fake_plugin_for_test(false))
    });
    let Some(handle) = shared.plugin_handle(1) else {
        panic!("the fake is loaded");
    };
    let editor = match &handle {
        crate::audio::PluginHandle::Clap(plugin) => std::sync::Arc::clone(plugin),
        crate::audio::PluginHandle::Vst3(_) => unreachable!(),
    };
    drop(handle);

    const HELD: std::time::Duration = std::time::Duration::from_secs(5);
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let chain = shared.paths[0].plugins.lock().unwrap();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let _worker_main = crate::audio::clap::claim_main_thread_for_test();
            for _ in 0..100 {
                let views = shared.plugin_views();
                assert_eq!(views.len(), 2);
                // The open-editor poll: identity from the published view.
                assert_eq!(
                    views[1].instance,
                    Some(crate::audio::plugin_instance_id(&editor))
                );
                shared.service_plugins();
                shared.align_plugins_to_output_rate();
            }
            done_tx.send(()).unwrap();
        });
        let finished = done_rx.recv_timeout(HELD);
        drop(chain);
        assert!(finished.is_ok(), "a UI-side call waited for the chain lock");
    });
}

/// Dragging an insert's mix, toggling its bypass and moving a plugin
/// control never take the chain lock, so the audio thread never misses
/// the chain for them: the worker finishes while this thread holds it.
#[test]
fn mix_bypass_and_controls_never_wait_for_the_chain_lock() {
    let _main = claim_main_thread();
    let shared = super::Shared::new(48_000);
    let path = &shared.paths[0];
    path.edit_chain(|chain| {
        chain.insert_clap_for_test(crate::audio::clap::tests::fake_plugin_for_test(false))
    });
    const HELD: std::time::Duration = std::time::Duration::from_secs(5);
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let chain = path.plugins.lock().unwrap();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let _worker_main = crate::audio::clap::claim_main_thread_for_test();
            for step in 0..100 {
                path.set_plugin_mix(0, step as f32 / 100.0);
                path.set_plugin_bypass(0, step % 2 == 0);
                // The fake has no controls: this fails, but at the
                // plugin, without the chain lock.
                let _ = path.set_plugin_parameter(0, 7, 0.5);
                // Neither reading nor resetting the counters may take the
                // chain lock the main thread holds.
                let _ = path.plugin_lock_report();
                path.reset_plugin_lock_counters();
            }
            path.set_plugin_mix(0, 0.25);
            path.set_plugin_bypass(0, true);
            done_tx.send(()).unwrap();
        });
        let finished = done_rx.recv_timeout(HELD);
        drop(chain);
        assert!(finished.is_ok(), "a mix, bypass or control change waited");
    });
    // The views show the change at once, and so does a save.
    let views = path.plugin_views();
    assert_eq!((views[0].mix, views[0].bypass), (0.25, true));
    let saved = &path.plugin_prefs()[0];
    assert_eq!((saved.mix, saved.bypass), (0.25, true));
    drop(path.edit_chain(|chain| chain.restore(Vec::new())));
}

#[test]
fn a_plugin_latency_change_republishes_the_views() {
    use crate::audio::clap::tests::{
        fake_plugin_with_latency_for_test, restart_fake_with_latency_for_test,
    };
    let _main = claim_main_thread();
    let shared = super::Shared::new(48_000);
    shared.paths[0]
        .edit_chain(|chain| chain.insert_clap_for_test(fake_plugin_with_latency_for_test(64)));
    assert_eq!(shared.plugin_views()[0].latency, 64);
    assert_eq!(shared.paths[0].latency_samples(), 64);
    // A quiet frame publishes nothing.
    let version = shared.paths[0].plugin_views_version();
    shared.service_plugins();
    assert_eq!(shared.paths[0].plugin_views_version(), version);

    // The plugin restarts with a new latency (it has not processed, so
    // the main thread restarts it right away).
    restart_fake_with_latency_for_test(8);
    shared.service_plugins();
    assert_eq!(shared.plugin_views()[0].latency, 8);
    assert_eq!(shared.paths[0].latency_samples(), 8);
}

#[test]
fn a_panicking_edit_clears_what_it_published() {
    let _main = claim_main_thread();
    let shared = super::Shared::new(48_000);
    shared.paths[0].edit_chain(|chain| {
        chain.insert_clap_for_test(crate::audio::clap::tests::fake_plugin_for_test(false))
    });
    assert_eq!(shared.plugin_views().len(), 1);
    assert_eq!(shared.paths[0].plugin_services.lock().unwrap().len(), 1);
    assert!(shared.paths[0].has_plugins());

    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        shared.paths[0].edit_chain(|chain| {
            chain.set_bypass(0, true);
            panic!("an edit failed halfway");
        })
    }));
    assert!(panicked.is_err());
    assert!(shared.paths[0].plugins.is_poisoned());
    assert!(shared.plugin_views().is_empty());
    assert!(shared.paths[0].plugin_services.lock().unwrap().is_empty());
    assert!(!shared.paths[0].has_plugins());
    // A poisoned chain refuses further edits and publishes nothing new.
    shared.set_plugin_bypass(0, false);
    assert!(shared.plugin_views().is_empty());
}

#[test]
fn views_name_the_loaded_instance() {
    let _main = claim_main_thread();
    let shared = super::Shared::new(48_000);
    shared.restore_plugins(vec![missing("a.vst3")]);
    assert_eq!(shared.plugin_views()[0].instance, None);
    shared.paths[0].edit_chain(|chain| {
        chain.insert_clap_for_test(crate::audio::clap::tests::fake_plugin_for_test(false))
    });
    let Some(handle) = shared.plugin_handle(1) else {
        panic!("the fake is loaded");
    };
    assert_eq!(
        shared.plugin_views()[1].instance,
        Some(handle.instance_id())
    );
    // An editor still holding the old instance no longer matches once
    // the insert is replaced.
    shared.remove_plugin(1);
    shared.paths[0].edit_chain(|chain| {
        chain.insert_clap_for_test(crate::audio::clap::tests::fake_plugin_for_test(false))
    });
    assert_ne!(
        shared.plugin_views()[1].instance,
        Some(handle.instance_id())
    );
}

#[test]
fn a_processing_failure_is_published_on_the_next_service_frame() {
    let _main = claim_main_thread();
    let shared = super::Shared::new(48_000);
    shared.paths[0].edit_chain(|chain| {
        chain.insert_clap_for_test(crate::audio::clap::tests::fake_plugin_for_test(true))
    });
    assert!(shared.paths[0].has_plugins());
    assert!(shared.plugin_views()[0].error.is_none());

    let mut left = vec![0.5f32; 64];
    let mut right = vec![0.5f32; 64];
    let version = shared.plugin_views_version();
    shared.paths[0].process_plugins(&mut left, &mut right, 64);
    assert!(
        !shared.paths[0].has_plugins(),
        "the failed insert left the chain"
    );
    assert_eq!(
        shared.plugin_views_version(),
        version,
        "the audio thread must not rebuild the views"
    );
    assert!(shared.plugin_views()[0].error.is_none());

    shared.service_plugins();
    let error = shared.plugin_views()[0].error.clone();
    assert!(
        error
            .as_deref()
            .is_some_and(|text| text.starts_with("Processing failed")),
        "{error:?}"
    );
    // Nothing else changed, so the next frame does not publish again.
    let version = shared.plugin_views_version();
    shared.service_plugins();
    assert_eq!(shared.plugin_views_version(), version);
}

#[test]
fn a_clap_port_change_is_published_on_the_next_service_frame() {
    let _main = claim_main_thread();
    let shared = super::Shared::new(48_000);
    shared.paths[0].edit_chain(|chain| {
        chain.insert_clap_for_test(crate::audio::clap::tests::fake_plugin_for_test(false))
    });
    assert!(shared.plugin_views()[0].error.is_none());
    let Some(crate::audio::PluginHandle::Clap(plugin)) = shared.plugin_handle(0) else {
        panic!("the fake is a loaded CLAP slot");
    };
    crate::audio::clap::tests::rescan_ports_for_test(&plugin.lock().unwrap());
    drop(plugin);
    assert!(shared.plugin_views()[0].error.is_none());

    shared.service_plugins();
    assert_eq!(
        shared.plugin_views()[0].error.as_deref(),
        Some(crate::audio::clap::AUDIO_PORTS_CHANGED)
    );
    assert!(!shared.paths[0].has_plugins());
}

#[test]
fn followers_are_pushed_live_and_cleared() {
    use super::{FollowerLink, FollowerPlan};
    use crate::audio::output_map::OutputDeviceSettings;

    let shared = super::Shared::new(48_000);
    let mut plan = FollowerPlan {
        count: 2,
        ..FollowerPlan::default()
    };
    plan.slots[0] = FollowerLink {
        device_id: "ALSA:hw:CARD=1,DEV=0".to_owned(),
        node_name: "speakers".to_owned(),
        virtual_out: false,
        publish: false,
    };
    plan.slots[1] = FollowerLink {
        device_id: String::new(),
        node_name: String::new(),
        virtual_out: true,
        publish: true,
    };
    shared.set_followers(
        &[
            OutputDeviceSettings {
                level_db: -6.0,
                mute: true,
                ..OutputDeviceSettings::default()
            },
            OutputDeviceSettings::default(),
        ],
        &plan,
        &[(CushionMode::LowLatency, 10.0), (CushionMode::Safe, 10.0)],
    );
    assert_eq!(shared.follower_count(), 2);
    assert_eq!(shared.snapshot().follower_count, 2);
    let live = shared.follower_plan();
    assert_eq!(live.slots[0].node_name, "speakers");
    assert_eq!(live.slots[0].device_id, "ALSA:hw:CARD=1,DEV=0");
    assert!(live.slots[1].virtual_out && live.slots[1].publish);
    assert_eq!(
        shared.follower_bus(0).cushion_mode(),
        CushionMode::LowLatency
    );

    shared.set_followers(&[], &FollowerPlan::default(), &[]);
    assert_eq!(shared.follower_count(), 0);
    assert!(shared.follower_plan().slots[0].node_name.is_empty());
}
