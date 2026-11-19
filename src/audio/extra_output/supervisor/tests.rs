//! Which plan slots the supervisor opens on their own cpal stream, and how a
//! slot table opens, retries and closes them.

use std::sync::atomic::Ordering;

use super::super::super::shared::{ExtraOutputLink, ExtraOutputPlan, Shared};
use super::{generic_extra_outputs, ExtraOutputSlots};

/// A cpal id of this platform's extra output host.
#[cfg(target_os = "linux")]
const GENERIC: &str = "alsa:hw:1,0";
#[cfg(target_os = "windows")]
const GENERIC: &str = "wasapi:speakers";
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
const GENERIC: &str = "coreaudio:speakers";

fn link(device_id: &str) -> ExtraOutputLink {
    ExtraOutputLink {
        device_id: device_id.to_owned(),
        ..ExtraOutputLink::default()
    }
}

fn plan(links: &[ExtraOutputLink]) -> ExtraOutputPlan {
    let mut plan = ExtraOutputPlan {
        count: links.len(),
        ..ExtraOutputPlan::default()
    };
    plan.slots[..links.len()].clone_from_slice(links);
    plan
}

#[test]
fn a_generic_extra_output_keeps_its_slot() {
    let wanted = generic_extra_outputs(&plan(&[link(""), link(GENERIC)]));
    assert_eq!(wanted, vec![(1, GENERIC.to_owned())]);
}

#[test]
fn slots_past_the_count_are_not_opened() {
    let mut plan = plan(&[link(GENERIC)]);
    plan.slots[1] = link(GENERIC);
    assert_eq!(generic_extra_outputs(&plan), vec![(0, GENERIC.to_owned())]);
}

#[test]
fn an_unparsable_id_is_not_opened() {
    assert!(generic_extra_outputs(&plan(&[link("no host here")])).is_empty());
}

#[cfg(target_os = "linux")]
#[test]
fn pipewire_nodes_and_rigwerk_out_are_not_opened_on_linux() {
    let virtual_output = ExtraOutputLink {
        device_id: crate::audio::output_map::VIRTUAL_OUTPUT_ID.to_owned(),
        virtual_output: true,
        virtual_output_on: true,
        ..ExtraOutputLink::default()
    };
    let generic_virtual = ExtraOutputLink {
        device_id: GENERIC.to_owned(),
        virtual_output: true,
        virtual_output_on: true,
        ..ExtraOutputLink::default()
    };
    let wanted = generic_extra_outputs(&plan(&[
        link("pipewire:alsa_output.usb"),
        virtual_output,
        generic_virtual,
        link(GENERIC),
    ]));
    assert_eq!(wanted, vec![(3, GENERIC.to_owned())]);
}

#[cfg(target_os = "windows")]
#[test]
fn a_vb_cable_virtual_output_plays_only_while_on_on_windows() {
    let cable = |virtual_output_on: bool| ExtraOutputLink {
        device_id: GENERIC.to_owned(),
        virtual_output: true,
        virtual_output_on,
        ..ExtraOutputLink::default()
    };
    let wanted = generic_extra_outputs(&plan(&[cable(true), cable(false)]));
    assert_eq!(wanted, vec![(0, GENERIC.to_owned())]);
}

fn missing(shared: &Shared, slot: usize) -> bool {
    shared
        .extra_output_bus(slot)
        .missing
        .load(Ordering::Relaxed)
}

fn feeding(shared: &Shared, slot: usize) -> bool {
    shared
        .extra_output_ring_active(slot)
        .load(Ordering::Acquire)
}

fn wanted(slot: usize, key: &str) -> Vec<(usize, String)> {
    vec![(slot, key.to_owned())]
}

#[test]
fn an_opened_slot_feeds_its_ring_and_is_not_reopened() {
    let shared = Shared::new(48_000);
    let mut table = ExtraOutputSlots::<()>::new();
    let mut opens = 0;
    for _ in 0..3 {
        table.sync(&shared, &wanted(1, "sink"), |_, _| {
            opens += 1;
            Ok(())
        });
    }
    assert_eq!(opens, 1);
    assert!(feeding(&shared, 1));
    assert!(!missing(&shared, 1));
    assert!(!feeding(&shared, 0));
}

#[test]
fn a_failed_open_is_missing_and_waits_before_it_retries() {
    let shared = Shared::new(48_000);
    let mut table = ExtraOutputSlots::<()>::new();
    let mut opens = 0;
    for _ in 0..2 {
        table.sync(&shared, &wanted(0, "sink"), |_, _| {
            opens += 1;
            Err("gone".to_owned())
        });
    }
    assert_eq!(opens, 1, "the second tick is inside the backoff");
    assert!(missing(&shared, 0));
    assert!(!feeding(&shared, 0));
}

#[test]
fn a_stream_that_reports_itself_lost_is_reopened() {
    let shared = Shared::new(48_000);
    let mut table = ExtraOutputSlots::<()>::new();
    let opens = std::cell::Cell::new(0);
    let open = |_: usize, _: &str| {
        opens.set(opens.get() + 1);
        Ok(())
    };
    table.sync(&shared, &wanted(0, "sink"), open);
    shared
        .extra_output_bus(0)
        .missing
        .store(true, Ordering::Relaxed);
    for _ in 0..2 {
        table.sync(&shared, &wanted(0, "sink"), open);
    }
    assert_eq!(opens.get(), 1, "a lost stream waits out the backoff");
    assert!(missing(&shared, 0), "and stays missing until it reopens");
    assert!(!feeding(&shared, 0));

    // The backoff runs out.
    table.retry_at[0] = Some(std::time::Instant::now());
    table.sync(&shared, &wanted(0, "sink"), open);
    assert_eq!(opens.get(), 2);
    assert!(feeding(&shared, 0));
    assert!(!missing(&shared, 0));
}

#[test]
fn a_new_key_reopens_and_a_slot_that_leaves_stops_its_ring() {
    let shared = Shared::new(48_000);
    let mut table = ExtraOutputSlots::<String>::new();
    let open = |_: usize, key: &str| Ok(key.to_owned());
    table.sync(&shared, &wanted(0, "a"), open);
    let mut reopened = None;
    table.sync(&shared, &wanted(0, "b"), |_, key| {
        reopened = Some(key.to_owned());
        Ok(key.to_owned())
    });
    assert_eq!(reopened.as_deref(), Some("b"));
    assert!(feeding(&shared, 0));

    table.sync(&shared, &[], open);
    assert!(!feeding(&shared, 0));
    assert!(!missing(&shared, 0));
}

#[test]
fn a_table_leaves_the_slots_of_another_table_alone() {
    // A table clears only the flags of the slots it opened or retries.
    let shared = Shared::new(48_000);
    let mut cpal = ExtraOutputSlots::<()>::new();
    let mut nodes = ExtraOutputSlots::<()>::new();
    let plan = [(0, "sink".to_owned()), (1, "gone".to_owned())];
    nodes.sync(&shared, &plan, |slot, _| match slot {
        0 => Ok(()),
        _ => Err("gone".to_owned()),
    });
    cpal.sync(&shared, &[], |_, _| Ok(()));
    assert!(feeding(&shared, 0));
    assert!(missing(&shared, 1));
}
