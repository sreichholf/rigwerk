use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use super::super::chain::MAX_INSERTS;
use super::super::pref::PluginPref;
use super::super::slot::Slot;
use super::ChainModel;

fn model() -> ChainModel {
    ChainModel::new(48_000, Arc::new(AtomicBool::new(false)))
}

fn pref(index: usize) -> PluginPref {
    PluginPref {
        name: format!("insert {index}"),
        ..PluginPref::default()
    }
}

#[test]
fn a_rate_of_zero_is_48_khz() {
    assert_eq!(
        ChainModel::new(0, Arc::new(AtomicBool::new(false))).rate(),
        48_000
    );
}

#[test]
fn a_restore_keeps_at_most_max_inserts() {
    let mut model = model();
    model.restore((0..MAX_INSERTS + 3).map(pref).collect());
    assert_eq!(model.links().len(), MAX_INSERTS);
    assert_eq!(
        model.links()[MAX_INSERTS - 1].base.name,
        format!("insert {}", MAX_INSERTS - 1)
    );
    assert!(model.is_full());
    drop(model.flush());
}

#[test]
fn a_full_chain_hands_the_slot_back() {
    let mut model = model();
    model.restore((0..MAX_INSERTS).map(pref).collect());
    let Err(slot) = model.push(Slot::empty(pref(99))) else {
        panic!("a full chain took another insert");
    };
    assert_eq!(slot.pref.name, "insert 99");
    model.remove(0);
    assert!(model.push(Slot::empty(pref(99))).is_ok());
    assert_eq!(model.links().len(), MAX_INSERTS);
    drop(model.flush());
}

#[test]
fn a_move_is_clamped_to_the_chain() {
    let mut model = model();
    model.restore((0..3).map(pref).collect());
    model.move_slot(0, -1);
    model.move_slot(2, 5);
    model.move_slot(7, 1);
    let names: Vec<&str> = model
        .links()
        .iter()
        .map(|link| link.base.name.as_str())
        .collect();
    assert_eq!(names, ["insert 0", "insert 1", "insert 2"]);
    model.move_slot(2, -1);
    let names: Vec<&str> = model
        .links()
        .iter()
        .map(|link| link.base.name.as_str())
        .collect();
    assert_eq!(names, ["insert 0", "insert 2", "insert 1"]);
    drop(model.flush());
}

#[test]
fn restored_slots_are_unopened_until_a_reload_opens_them() {
    let mut model = model();
    model.restore(vec![pref(0)]);
    assert!(model.has_unopened());
    let plan = model.reload_plan(48_000).expect("an unopened slot reloads");
    assert_eq!(plan.jobs.len(), 1);
    assert_eq!(plan.previous_rate, 48_000);
    assert!(plan.requested.is_empty());
    // A rate change with nothing to open only moves the rate.
    let mut empty = super::ChainModel::new(48_000, Arc::new(AtomicBool::new(false)));
    assert!(empty.reload_plan(44_100).is_none());
    assert_eq!(empty.rate(), 44_100);
    drop(model.flush());
}
