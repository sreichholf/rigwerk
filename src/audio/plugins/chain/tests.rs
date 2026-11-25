use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use super::super::model::ChainModel;
#[cfg(debug_assertions)]
use super::release_slot;
use super::{ChainRuntime, RETURN_QUEUE};

fn model() -> ChainModel {
    ChainModel::new(48_000, Arc::new(AtomicBool::new(false)))
}

fn names(runtime: &ChainRuntime) -> Vec<String> {
    runtime.views().into_iter().map(|view| view.name).collect()
}

fn pref(name: &str) -> super::PluginPref {
    super::PluginPref {
        name: name.to_owned(),
        ..super::PluginPref::default()
    }
}

#[test]
fn a_home_hands_its_runtime_to_one_holder_at_a_time() {
    let model = model();
    let home = model.home();
    let lease = home.take().expect("parked at first");
    assert!(lease.is_from(&home));
    assert!(home.take().is_none(), "already held");
    drop(lease);
    let again = home.take().expect("parked again by the lease");
    assert!(!again.is_from(
        &super::super::model::ChainModel::new(48_000, Arc::new(AtomicBool::new(false))).home()
    ));
}

/// While an audio thread holds the runtime, edits queue; the runtime
/// applies them in order with its next block and hands back what it let
/// go of, which the next flush collects.
#[test]
fn held_edits_apply_in_order_and_come_back_to_the_ui() {
    let mut model = model();
    let home = model.home();
    let mut runtime = home.take().expect("parked");

    model.restore(vec![pref("a"), pref("b"), pref("c")]);
    model.move_slot(0, 1);
    model.remove(2);
    assert!(model.flush().is_empty(), "the runtime is held");
    assert!(model.pending());
    assert_eq!(runtime.len(), 0, "nothing applies before the block");

    assert!(runtime.apply_commands());
    assert_eq!(names(&runtime), ["b", "a"]);
    assert!(!runtime.apply_commands(), "nothing left");

    let returned = model.flush();
    assert_eq!(returned.len(), 1, "the removed slot came back");
    assert_eq!(returned[0].pref.name, "c");
    assert!(!model.pending());
    drop(runtime);
}

/// With the runtime parked (no bridge), an edit applies at once and its
/// slots come back in the same flush.
#[test]
fn a_parked_runtime_applies_edits_in_the_flush() {
    let mut model = model();
    let home = model.home();
    model.restore(vec![pref("a"), pref("b")]);
    assert!(model.flush().is_empty());
    model.restore(vec![pref("c")]);
    let returned = model.flush();
    assert_eq!(returned.len(), 2);
    assert!(!model.pending());
    let runtime = home.take().expect("parked again");
    assert_eq!(names(&runtime), ["c"]);
}

/// The runtime never drops a slot itself: a command that would hand one
/// back waits while the return queue is full, and the commands behind it
/// wait too, in the backlog when their own queue is full.
#[test]
fn a_full_return_queue_holds_commands_back_until_the_ui_collects() {
    let mut model = model();
    let home = model.home();
    model.restore(vec![pref("first")]);
    assert!(model.flush().is_empty());
    let mut runtime = home.take().expect("parked");

    let edits = RETURN_QUEUE + 3;
    for edit in 0..edits {
        model.restore(vec![pref(&format!("slot {edit}"))]);
    }
    assert!(model.flush().is_empty());
    runtime.apply_commands();
    // The last remove that fit found the return queue full.
    assert_eq!(runtime.len(), 1);
    let first = model.flush();
    assert_eq!(first.len(), RETURN_QUEUE);
    assert!(model.pending(), "the rest waits for room");

    runtime.apply_commands();
    drop(runtime);
    let rest = model.flush();
    assert_eq!(first.len() + rest.len(), edits);
    assert!(!model.pending());
    let runtime = home.take().expect("parked");
    assert_eq!(names(&runtime), [format!("slot {}", edits - 1)]);
}

#[test]
fn a_replace_keeps_the_controls_and_a_new_rate_reaches_the_runtime() {
    let mut model = model();
    let home = model.home();
    model.insert_delay_for_test(16);
    assert!(model.flush().is_empty());
    let controls = Arc::clone(&model.links()[0].controls);
    controls.set_mix(0.25);

    let plan = model.reload_plan(96_000).expect("a new rate reloads");
    assert_eq!(plan.jobs.len(), 1);
    let built = plan
        .jobs
        .into_iter()
        .map(|(index, _)| (index, super::Slot::delay_for_test(16)))
        .collect();
    model.install(96_000, built);
    let returned = model.flush();
    assert_eq!(returned.len(), 1, "the old slot came back");
    assert!(Arc::ptr_eq(&model.links()[0].controls, &controls));
    assert_eq!(model.rate(), 96_000);

    let mut runtime = home.take().expect("parked");
    assert_eq!(runtime.rate(), 96_000);
    assert!(Arc::ptr_eq(
        &runtime.slot_mut_for_test(0).controls,
        &controls
    ));
    assert_eq!(runtime.slot_mut_for_test(0).controls.mix(), 0.25);
}

/// A slot released inside a callback would destroy a plugin or take the
/// deferred-slot lock on the audio thread: the last owner is the UI thread
/// or the bridge, and a debug build asserts that at the one choke point.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "a plugin slot was released inside an audio callback")]
fn releasing_a_slot_inside_a_callback_is_caught() {
    use super::super::super::rt_guard::RtGuard;
    use super::super::slot::Slot;

    let _guard = RtGuard::enter();
    release_slot(Box::new(Slot::tracked_for_test()));
}
