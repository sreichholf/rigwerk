//! Which plan slots the supervisor opens on their own cpal stream.

use super::super::super::shared::{FollowerLink, FollowerPlan};
use super::generic_followers;

/// A cpal id of this platform's follower host.
#[cfg(target_os = "linux")]
const GENERIC: &str = "alsa:hw:1,0";
#[cfg(target_os = "windows")]
const GENERIC: &str = "wasapi:speakers";
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
const GENERIC: &str = "coreaudio:speakers";

fn link(device_id: &str) -> FollowerLink {
    FollowerLink {
        device_id: device_id.to_owned(),
        ..FollowerLink::default()
    }
}

fn plan(links: &[FollowerLink]) -> FollowerPlan {
    let mut plan = FollowerPlan {
        count: links.len(),
        ..FollowerPlan::default()
    };
    plan.slots[..links.len()].clone_from_slice(links);
    plan
}

#[test]
fn a_generic_follower_keeps_its_slot() {
    let wanted = generic_followers(&plan(&[link(""), link(GENERIC)]));
    assert_eq!(wanted, vec![(1, GENERIC.to_owned())]);
}

#[test]
fn slots_past_the_count_are_not_opened() {
    let mut plan = plan(&[link(GENERIC)]);
    plan.slots[1] = link(GENERIC);
    assert_eq!(generic_followers(&plan), vec![(0, GENERIC.to_owned())]);
}

#[test]
fn an_unparsable_id_is_not_opened() {
    assert!(generic_followers(&plan(&[link("no host here")])).is_empty());
}

#[cfg(target_os = "linux")]
#[test]
fn pipewire_nodes_and_rigwerk_out_are_not_opened_on_linux() {
    let virtual_out = FollowerLink {
        device_id: crate::audio::output_map::VIRTUAL_OUT_ID.to_owned(),
        virtual_out: true,
        publish: true,
        ..FollowerLink::default()
    };
    let generic_virtual = FollowerLink {
        device_id: GENERIC.to_owned(),
        virtual_out: true,
        publish: true,
        ..FollowerLink::default()
    };
    let wanted = generic_followers(&plan(&[
        link("pipewire:alsa_output.usb"),
        virtual_out,
        generic_virtual,
        link(GENERIC),
    ]));
    assert_eq!(wanted, vec![(3, GENERIC.to_owned())]);
}

#[cfg(target_os = "windows")]
#[test]
fn a_vb_cable_virtual_out_plays_only_while_published_on_windows() {
    let cable = |publish: bool| FollowerLink {
        device_id: GENERIC.to_owned(),
        virtual_out: true,
        publish,
        ..FollowerLink::default()
    };
    let wanted = generic_followers(&plan(&[cable(true), cable(false)]));
    assert_eq!(wanted, vec![(0, GENERIC.to_owned())]);
}
