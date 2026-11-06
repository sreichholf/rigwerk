//! Device restore helpers applied once the first inventory arrives.

use crate::app::devices::{
    default_index, device_restore_note, held_device, restore_device, HeldDevice,
};
use crate::app::settings::Preferences;
use crate::audio::{self, Inventory};

/// Saved device choices applied once the first background device scan finishes.
pub(crate) struct StartupGate {
    pub(crate) preferences: Preferences,
}

/// Indices and held identities from one startup restore.
pub(crate) struct RestoredDevices {
    pub(crate) asio_idx: usize,
    pub(crate) input_idx: usize,
    pub(crate) output_idx: usize,
    pub(crate) asio_held: Option<HeldDevice>,
    pub(crate) input_held: Option<HeldDevice>,
    pub(crate) output_held: Option<HeldDevice>,
    pub(crate) notes: Vec<String>,
}

/// Selects the saved devices, and holds any stored identity the scan did not confirm.
///
/// The window still shows a fallback (the system default, or the first real
/// device; the system input stays at index 0) so the app can be used. `save`
/// writes a held identity back instead of that fallback.
pub(crate) fn restore_saved_devices(
    inventory: &Inventory,
    preferences: &Preferences,
) -> RestoredDevices {
    let mut asio_idx = default_index(&inventory.asio);
    let mut output_idx = default_index(&inventory.outputs);
    let mut input_idx = 0usize;
    let asio_restore = restore_device(
        &inventory.asio,
        &preferences.asio_id,
        &preferences.asio_backend,
        preferences.asio_name.as_deref().unwrap_or(""),
    );
    if let Some(index) = asio_restore.index() {
        asio_idx = index;
    }
    let input_restore = restore_device(
        &inventory.inputs,
        &preferences.input_id,
        &preferences.input_backend,
        preferences.input_name.as_deref().unwrap_or(""),
    );
    if let Some(index) = input_restore.index() {
        input_idx = index;
    }
    let output_restore = restore_device(
        &inventory.outputs,
        &preferences.output_id,
        &preferences.output_backend,
        preferences.output_name.as_deref().unwrap_or(""),
    );
    if let Some(index) = output_restore.index() {
        output_idx = index;
    }
    let mut notes = Vec::new();
    for (role, name, id, restore) in [
        (
            "Low-latency input",
            preferences.asio_name.as_deref().unwrap_or(""),
            preferences.asio_id.as_str(),
            asio_restore,
        ),
        (
            "Input",
            preferences.input_name.as_deref().unwrap_or(""),
            preferences.input_id.as_str(),
            input_restore,
        ),
        (
            "Output",
            preferences.output_name.as_deref().unwrap_or(""),
            preferences.output_id.as_str(),
            output_restore,
        ),
    ] {
        let label = if !name.is_empty() { name } else { id };
        if let Some(note) = device_restore_note(role, label, restore) {
            notes.push(note);
        }
    }
    RestoredDevices {
        asio_idx,
        input_idx,
        output_idx,
        asio_held: held_device(
            asio_restore,
            preferences.asio_name.clone(),
            preferences.asio_id.clone(),
            preferences.asio_backend.clone(),
        ),
        input_held: held_device(
            input_restore,
            preferences.input_name.clone(),
            preferences.input_id.clone(),
            preferences.input_backend.clone(),
        ),
        output_held: held_device(
            output_restore,
            preferences.output_name.clone(),
            preferences.output_id.clone(),
            preferences.output_backend.clone(),
        ),
        notes,
    }
}

/// The name, id, and backend `save` writes for one side.
pub(crate) fn persisted_identity(
    held: Option<&HeldDevice>,
    live: Option<&audio::DeviceItem>,
) -> (Option<String>, String, String) {
    if let Some(held) = held {
        return (held.name.clone(), held.id.clone(), held.backend.clone());
    }
    match live {
        Some(device) => (
            Some(device.name.clone()),
            device.id.clone(),
            device.backend.clone(),
        ),
        None => (None, String::new(), String::new()),
    }
}
