use std::sync::Arc;

use super::{tracked_slots_alive, Slot, SlotPark};

#[test]
fn a_deferred_slot_drains_through_its_own_park() {
    let _main = crate::audio::claim_main_thread();
    let alive = tracked_slots_alive;
    let before = alive();
    let first = SlotPark::new();
    let second = SlotPark::new();

    first.defer(vec![Slot::tracked_for_test()]);
    assert_eq!(alive(), before + 1);
    assert_eq!(first.len(), 1);
    assert_eq!(second.len(), 0);

    second.drain();
    assert_eq!(alive(), before + 1, "another owner's drain frees nothing");
    assert_eq!(first.len(), 1);

    first.drain();
    assert_eq!(alive(), before);
    assert_eq!(first.len(), 0);
}

#[test]
fn a_park_dropped_off_the_main_thread_destroys_nothing() {
    let _main = crate::audio::claim_main_thread();
    let park = SlotPark::new();
    let slot = Slot::empty(crate::audio::plugins::PluginPref::default());
    let controls = Arc::clone(&slot.controls);
    park.defer(vec![slot]);
    std::thread::scope(|scope| {
        scope.spawn(move || drop(park));
    });
    assert_eq!(
        Arc::strong_count(&controls),
        2,
        "the slots were forgotten, not destroyed"
    );
}
