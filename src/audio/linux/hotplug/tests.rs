use super::uevent_is_sound_change;

#[test]
fn a_sound_card_add_is_a_device_change() {
    let message = "add@/devices/pci0000:00/sound/card0\0\
ACTION=add\0\
SUBSYSTEM=sound\0\
DEVNAME=snd/controlC0\0";
    assert!(uevent_is_sound_change(message));
}

#[test]
fn a_sound_card_remove_is_a_device_change() {
    let message = "remove@/devices/pci0000:00/sound/card1\0ACTION=remove\0SUBSYSTEM=sound\0";
    assert!(uevent_is_sound_change(message));
}

#[test]
fn other_subsystems_are_ignored() {
    let message = "add@/devices/usb\0ACTION=add\0SUBSYSTEM=usb\0";
    assert!(!uevent_is_sound_change(message));
}
