use std::sync::Arc;

use super::super::transfer::param_transfer;
use super::{
    ClapFailure, Lv2Failure, ParamIds, ParamOut, PluginShared, ProcessFailure, RESTART_ACTIVATION,
};
use crate::audio::plugins::MAX_ALIGN;

#[test]
fn only_the_first_failure_is_kept_and_reads_back() {
    let shared = PluginShared::new(0);
    assert_eq!(shared.failure(), None);
    shared.fail(ProcessFailure::Vst3(-1));
    shared.fail(ProcessFailure::Clap(ClapFailure::NotActive));
    assert_eq!(shared.failure(), Some(ProcessFailure::Vst3(-1)));
    for failure in [
        ProcessFailure::Vst3(0x7fff_ffff),
        ProcessFailure::Vst3StartFailed(1),
        ProcessFailure::Vst3StartFailed(-1),
        ProcessFailure::Vst3Inactive,
        ProcessFailure::ReloadLoop,
        ProcessFailure::PauseStranded,
        ProcessFailure::Clap(ClapFailure::NotActive),
        ProcessFailure::Clap(ClapFailure::ProcessError),
        ProcessFailure::Clap(ClapFailure::StartFailed),
        ProcessFailure::Lv2(Lv2Failure::NotActive),
    ] {
        let shared = PluginShared::new(0);
        shared.fail(failure);
        assert_eq!(shared.failure(), Some(failure));
    }
}

#[test]
fn failure_messages_keep_the_host_texts() {
    assert_eq!(
        ProcessFailure::Vst3(1).message(),
        "Plugin process() failed: 0x1"
    );
    assert_eq!(
        ProcessFailure::Vst3StartFailed(1).message(),
        "Plugin setProcessing() failed: 0x1"
    );
    assert_eq!(
        ProcessFailure::Vst3Inactive.message(),
        "The VST3 plugin is not active."
    );
    assert_eq!(
        ProcessFailure::PauseStranded.message(),
        "The plugin stopped responding while it was paused and no longer processes audio. \
         Restart RigWerk to recover."
    );
    // The CLAP processor reports the failure, not a text.
    for (failure, text) in [
        (ClapFailure::NotActive, "The CLAP plugin is not active."),
        (
            ClapFailure::ProcessError,
            "The CLAP plugin reported a processing error.",
        ),
        (
            ClapFailure::StartFailed,
            "The CLAP plugin could not start processing.",
        ),
    ] {
        assert_eq!(ProcessFailure::Clap(failure).message(), text);
    }
    assert_eq!(
        ProcessFailure::Lv2(Lv2Failure::NotActive).message(),
        "The LV2 plugin is not active."
    );
}

#[test]
fn a_latency_change_is_clamped_and_seen_once() {
    let shared = PluginShared::new(64);
    assert_eq!(shared.latency(), 64);
    assert!(!shared.set_latency(64));
    assert!(!shared.take_latency_change());
    assert!(shared.set_latency(u32::MAX));
    assert_eq!(shared.latency(), MAX_ALIGN);
    assert!(shared.take_latency_change());
    assert!(!shared.take_latency_change());
}

#[test]
fn restart_bits_collect_until_taken_and_a_reload_request_stays() {
    let shared = PluginShared::new(0);
    shared.request_restart(1);
    shared.request_restart(RESTART_ACTIVATION);
    assert!(shared.restart_pending());
    assert_eq!(shared.take_restart(), 1 | RESTART_ACTIVATION);
    assert!(!shared.restart_pending());

    assert!(!shared.reload_wanted());
    shared.want_reload();
    shared.want_reload();
    assert!(shared.reload_wanted());

    assert!(!shared.state_dirty());
    shared.mark_state_dirty();
    assert!(shared.state_dirty());
    assert!(shared.take_state_dirty());
    assert!(!shared.state_dirty());
}

#[test]
fn param_ids_map_both_ways() {
    let ids = ParamIds::new(vec![900, 3, 42]);
    assert_eq!(ids.len(), 3);
    assert_eq!(ids.id(0), Some(900));
    assert_eq!(ids.id(3), None);
    assert_eq!(ids.index_of(3), Some(1));
    assert_eq!(ids.index_of(42), Some(2));
    assert_eq!(ids.index_of(900), Some(0));
    assert_eq!(ids.index_of(7), None);
}

#[test]
fn editor_edits_reach_the_transfer_and_keep_the_newest_value_per_id() {
    let ids = Arc::new(ParamIds::new(vec![10, 20]));
    let (sender, mut receiver) = param_transfer(ids.len());
    let out = ParamOut::new(ids, sender);
    assert!(!out.has_queued());
    assert!(out.push_edit(20, 0.25));
    assert!(out.push_edit(20, 0.5));
    assert!(out.push_edit(10, 0.75));
    assert!(!out.push_edit(30, 0.1), "an unknown id is refused");
    assert!(out.push(0, 1.0));
    assert!(out.has_queued());

    let mut edits = Vec::with_capacity(2);
    out.take_edits(&mut edits);
    assert_eq!(edits, vec![(20, 0.5), (10, 0.75)]);
    out.take_edits(&mut edits);
    assert!(edits.is_empty());

    let mut seen = Vec::new();
    receiver.drain(|index, value, offset| seen.push((index, value, offset)));
    assert_eq!(
        seen,
        vec![(1, 0.25, 0), (1, 0.5, 0), (0, 0.75, 0), (0, 1.0, 0)]
    );
    assert!(!out.has_queued());
}

#[test]
fn an_edit_from_another_thread_never_waits_for_the_ui() {
    let ids = Arc::new(ParamIds::new(vec![10]));
    let (sender, _receiver) = param_transfer(ids.len());
    let out = ParamOut::new(ids, sender);
    let held = out.sender.lock().unwrap();
    let out = &out;
    std::thread::scope(|scope| {
        let (tx, rx) = std::sync::mpsc::channel();
        scope.spawn(move || tx.send(out.push_edit(10, 0.5)).unwrap());
        let pushed = rx.recv_timeout(std::time::Duration::from_secs(5));
        // Released before the scope joins, so a blocked push fails the
        // assertion below instead of hanging the test.
        drop(held);
        assert_eq!(pushed, Ok(false), "dropped while the UI holds the sender");
    });
    std::thread::scope(|scope| {
        assert!(scope.spawn(|| out.push_edit(10, 0.5)).join().unwrap());
    });
}
