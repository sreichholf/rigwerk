use super::{tracked_slots_alive, Slot, SlotPark};

/// Each engine owns its park: a slot deferred into one owner is not visible
/// to another, and only the owner that holds it frees it.
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
