//! Device discovery and choosing a stream configuration for a device.

use cpal::traits::{DeviceTrait, HostTrait};

#[cfg(any(target_os = "linux", test))]
use super::linux::alsa::alsa_pcm_is_direct;
#[cfg(target_os = "linux")]
use super::linux::alsa::alsa_pcm_is_placeholder;
#[cfg(any(target_os = "linux", test))]
use super::linux::pipewire;
use super::types::{DeviceItem, Inventory, DEMO_ASIO_ID, DEMO_INPUT_ID, DEMO_OUTPUT_ID};

pub fn scan_devices() -> Inventory {
    scan_devices_from(&Inventory::default(), true)
}

/// Preview rows only — enough to paint the shell before the first real scan.
pub(crate) fn placeholder_inventory() -> Inventory {
    let mut inventory = Inventory {
        os: std::env::consts::OS.to_owned(),
        low_latency_label: if cfg!(target_os = "windows") {
            "ASIO".to_owned()
        } else if cfg!(target_os = "linux") {
            "PIPEWIRE".to_owned()
        } else {
            "SYSTEM".to_owned()
        },
        ..Inventory::default()
    };
    ensure_placeholders(&mut inventory);
    inventory
}

/// Like [`scan_devices`], but a device whose configuration cannot be read is
/// kept when `previous` already listed it.
///
/// `probe_configs` is false while a bridge is opening or running. Enumerating
/// names does not open a PCM. Asking ALSA for a configuration does, and that
/// open fails on a device the bridge already holds, which used to look like
/// an unplug.
pub(crate) fn scan_devices_from(previous: &Inventory, probe_configs: bool) -> Inventory {
    if !probe_configs {
        return presence_from(previous);
    }
    let mut inventory = Inventory {
        // ASIO capture on Windows; JACK or direct ALSA capture on Linux.
        // Playback always comes from a normal system host, never the
        // low-latency driver.
        os: std::env::consts::OS.to_owned(),
        low_latency_label: if cfg!(target_os = "windows") {
            "ASIO".to_owned()
        } else if cfg!(target_os = "linux") {
            "ALSA".to_owned()
        } else {
            "SYSTEM".to_owned()
        },
        ..Inventory::default()
    };

    let mut output_candidates: Vec<DeviceItem> = Vec::new();

    #[cfg(target_os = "windows")]
    {
        match azo::driver::Metadata::enumerate() {
            Ok(drivers) => {
                for driver in drivers {
                    let clsid = driver.clsid.to_u128();
                    let name = driver.description.to_string_lossy();
                    inventory.asio.push(DeviceItem {
                        id: format!("asio:{clsid:032x}"),
                        name: name.clone(),
                        backend: "ASIO".to_owned(),
                        channels: 2,
                        demo: false,
                        preferred: false,
                        asio_clsid: Some(clsid),
                    });
                    // The same driver is also a playback device.  Keeping it in
                    // the output list means the protocol no longer decides which
                    // side of the bridge a device may appear on.
                    output_candidates.push(DeviceItem {
                        id: format!("asio-out:{clsid:032x}"),
                        name,
                        backend: "ASIO".to_owned(),
                        channels: 2,
                        demo: false,
                        preferred: false,
                        asio_clsid: Some(clsid),
                    });
                }
            }
            Err(error) => inventory.notes.push(format!("ASIO registry scan: {error}")),
        }
    }

    let mut input_candidates: Vec<DeviceItem> = Vec::new();

    // The device Windows already uses everywhere else is the right default
    // here too.  Marking every host's default instead would let an arbitrary
    // MME or DirectSound device win the "preferred" sort below.
    let system_host_id = cpal::default_host().id();

    for host_id in cpal::available_hosts() {
        let host = match cpal::host_from_id(host_id) {
            Ok(host) => host,
            Err(_) => continue,
        };
        let host_name = host_id.name();
        let is_system_host = host_id == system_host_id;
        let default_output_id = if is_system_host {
            host.default_output_device()
                .and_then(|device| device.id().ok())
                .map(|id| id.to_string())
        } else {
            None
        };
        let default_input_id = if is_system_host {
            host.default_input_device()
                .and_then(|device| device.id().ok())
                .map(|id| id.to_string())
        } else {
            None
        };
        let devices = match host.devices() {
            Ok(devices) => devices,
            Err(_) => continue,
        };
        for device in devices {
            let name = device
                .description()
                .map(|description| description.name().to_owned())
                .unwrap_or_else(|_| "Audio device".to_owned());
            let id = device.id().map(|id| id.to_string()).ok();
            #[cfg(target_os = "linux")]
            let device_backend =
                if pipewire::is_pipewire_endpoint(id.as_deref().unwrap_or_default(), &name) {
                    "PIPEWIRE".to_owned()
                } else {
                    host_name.to_owned()
                };
            #[cfg(not(target_os = "linux"))]
            let device_backend = host_name.to_owned();

            // ALSA publishes sink/source plugins that cannot carry a guitar:
            // "null" discards everything and "oss" is the legacy emulation.
            // Leaving them out keeps the first real device as the default.
            #[cfg(target_os = "linux")]
            if host_name.eq_ignore_ascii_case("ALSA")
                && id.as_deref().is_some_and(alsa_pcm_is_placeholder)
            {
                continue;
            }

            if let Some(id) = id.as_ref() {
                match output_side(&device) {
                    Side::Ready(channels) => output_candidates.push(DeviceItem {
                        id: id.clone(),
                        name: name.clone(),
                        backend: device_backend.clone(),
                        channels,
                        demo: false,
                        preferred: default_output_id.as_deref() == Some(id.as_str()),
                        asio_clsid: None,
                    }),
                    Side::Unreadable => {
                        if let Some(prior) = remembered(&previous.outputs, id, &device_backend) {
                            output_candidates.push(prior);
                            let note = format!(
                                "{name} is present but its output configuration could not be read (it may be in use)."
                            );
                            if !inventory.notes.contains(&note) {
                                inventory.notes.push(note);
                            }
                        }
                    }
                    Side::Absent => {}
                }
                match input_side(&device) {
                    Side::Ready(channels) => input_candidates.push(DeviceItem {
                        id: id.clone(),
                        name,
                        backend: device_backend.clone(),
                        channels,
                        demo: false,
                        preferred: default_input_id.as_deref() == Some(id.as_str()),
                        asio_clsid: None,
                    }),
                    Side::Unreadable => {
                        if let Some(prior) = remembered(&previous.inputs, id, &device_backend)
                            .or_else(|| remembered(&previous.asio, id, &device_backend))
                        {
                            input_candidates.push(prior);
                            let note = format!(
                                "{name} is present but its input configuration could not be read (it may be in use)."
                            );
                            if !inventory.notes.contains(&note) {
                                inventory.notes.push(note);
                            }
                        }
                    }
                    Side::Absent => {}
                }
            }
        }
    }

    // ALSA publishes several plugin names for one card (front, surround,
    // sysdefault, dmix, …). They share the card's display name, so the menu
    // shows the same row over and over. When the card already has hw: or
    // plughw:, those aliases add nothing the person can choose usefully.
    hide_redundant_alsa_aliases(&mut output_candidates);
    hide_redundant_alsa_aliases(&mut input_candidates);

    // Real PipeWire sources and sinks replace the single ALSA `pipewire`
    // shim. When the server is down, the shim stays so a device can still
    // be opened. The preferred row is chosen after that, so a live sink
    // wins over the shim.
    #[cfg(target_os = "linux")]
    merge_native_nodes(
        &mut output_candidates,
        &mut input_candidates,
        &mut inventory.notes,
    );

    #[cfg(target_os = "linux")]
    let pipewire_output = prefer_pipewire(&mut output_candidates);
    #[cfg(all(test, not(target_os = "linux")))]
    {
        let _ = prefer_pipewire(&mut output_candidates);
    }
    #[cfg(any(target_os = "linux", test))]
    prefer_pipewire(&mut input_candidates);

    // WASAPI is the host that keeps other apps audible in shared mode, which is
    // exactly what an ASIO input needs on the other side of the bridge.  Rank
    // it above DirectSound/MME.  Collapse only a repeated endpoint (same id
    // and backend).  Two backends that share a display name, and two ALSA
    // aliases of one card, both stay in the list.
    let host_rank = |backend: &str| {
        if backend.eq_ignore_ascii_case("WASAPI") {
            0
        } else {
            1
        }
    };
    // ASIO outputs stay at the end of the list.  They are already a different
    // id and backend, so the dedup below would keep them either way; the split
    // is only so the everyday system output remains the first row.
    let mut asio_output_candidates = Vec::new();
    output_candidates.retain(|device| {
        if device.backend.eq_ignore_ascii_case("ASIO") {
            asio_output_candidates.push(device.clone());
            false
        } else {
            true
        }
    });
    output_candidates.sort_by(|a, b| {
        b.preferred
            .cmp(&a.preferred)
            .then_with(|| host_rank(&a.backend).cmp(&host_rank(&b.backend)))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    dedup_devices(&mut output_candidates);
    asio_output_candidates.sort_by_cached_key(|device| device.name.to_lowercase());
    dedup_devices(&mut asio_output_candidates);
    // Keep the everyday system outputs first: the common case is an ASIO
    // input playing through the speakers Windows already uses.  The ASIO
    // outputs stay available at the end for people who want that path.
    output_candidates.extend(asio_output_candidates);

    input_candidates.sort_by(|a, b| {
        b.preferred
            .cmp(&a.preferred)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    dedup_devices(&mut input_candidates);

    // On Linux, PipeWire is the default low-latency backend.  Its inputs and
    // the default PCM come first; JACK follows when present, then direct ALSA
    // hardware PCMs (`hw:`/`plughw:`).  Virtual PulseAudio/default inputs stay
    // on the system side.  On a machine that exposes only virtual ALSA inputs,
    // fall back to those so the side has something to open instead of showing
    // only a preview placeholder.
    #[cfg(target_os = "linux")]
    {
        let mut pipewire_inputs = Vec::new();
        input_candidates.retain(|device| {
            if pipewire::is_pipewire_device(device) {
                pipewire_inputs.push(device.clone());
                false
            } else {
                true
            }
        });

        let mut low_inputs = Vec::new();
        let has_pipewire = !pipewire_inputs.is_empty();
        low_inputs.extend(pipewire_inputs);

        let mut jack_inputs = Vec::new();
        input_candidates.retain(|device| {
            if device.backend.eq_ignore_ascii_case("JACK") {
                jack_inputs.push(device.clone());
                false
            } else {
                true
            }
        });
        let has_jack = !jack_inputs.is_empty();
        low_inputs.extend(jack_inputs);

        let mut direct_inputs = Vec::new();
        input_candidates.retain(|device| {
            if device.backend.eq_ignore_ascii_case("ALSA") && alsa_pcm_is_direct(&device.id) {
                direct_inputs.push(device.clone());
                false
            } else {
                true
            }
        });
        low_inputs.extend(direct_inputs);

        inventory.low_latency_label = if has_pipewire || pipewire_output {
            "PIPEWIRE"
        } else if has_jack {
            "JACK"
        } else {
            "ALSA"
        }
        .to_owned();

        if low_inputs.is_empty() {
            // Containers and some desktops only publish the "default" plugin;
            // keep it usable as the low-latency input too.  The system list
            // still has the same device.
            let alsa_inputs: Vec<DeviceItem> = input_candidates
                .iter()
                .filter(|device| {
                    device.backend.eq_ignore_ascii_case("ALSA")
                        || device.backend.eq_ignore_ascii_case("PIPEWIRE")
                })
                .cloned()
                .collect();
            inventory.asio.extend(alsa_inputs);
        } else {
            inventory.asio.extend(low_inputs);
        }
        // The preferred PipeWire entry must stay first even when a JACK or
        // direct ALSA device also landed on this side.
        inventory.asio.sort_by(|a, b| {
            b.preferred
                .cmp(&a.preferred)
                .then_with(|| low_latency_rank(&a.backend).cmp(&low_latency_rank(&b.backend)))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
    }

    inventory.outputs = output_candidates;
    inventory.inputs = input_candidates;
    ensure_placeholders(&mut inventory);
    inventory
}

/// Ids currently published by a host, without opening a stream configuration.
fn present_ids() -> std::collections::HashSet<String> {
    let mut ids = std::collections::HashSet::new();
    for host_id in cpal::available_hosts() {
        let Ok(host) = cpal::host_from_id(host_id) else {
            continue;
        };
        let Ok(devices) = host.devices() else {
            continue;
        };
        let host_name = host_id.name();
        for device in devices {
            let Ok(id) = device.id() else {
                continue;
            };
            let id = id.to_string();
            #[cfg(target_os = "linux")]
            if host_name.eq_ignore_ascii_case("ALSA") && alsa_pcm_is_placeholder(&id) {
                continue;
            }
            let _ = host_name;
            ids.insert(id);
        }
    }
    ids
}

/// A scan that does not open a PCM. Devices still enumerated stay; anything
/// the kernel no longer lists is dropped. New devices wait for a probing scan,
/// which is the one that asks for a configuration.
fn presence_from(previous: &Inventory) -> Inventory {
    let ids = present_ids();
    let mut inventory = previous.clone();
    inventory.notes.clear();
    for list in [
        &mut inventory.asio,
        &mut inventory.inputs,
        &mut inventory.outputs,
    ] {
        list.retain(|device| still_listed(device, &ids));
    }
    #[cfg(target_os = "linux")]
    refresh_native_presence(&mut inventory);
    ensure_placeholders(&mut inventory);
    inventory
}

fn still_listed(device: &DeviceItem, ids: &std::collections::HashSet<String>) -> bool {
    if device.demo || device.asio_clsid.is_some() || ids.contains(&device.id) {
        return true;
    }
    // Native PipeWire ids are not cpal ids. A presence scan keeps them until
    // the registry round-trip below refreshes the list. A failed round-trip
    // leaves the previous nodes in place.
    #[cfg(target_os = "linux")]
    if super::linux::native::is_native_id(&device.id) {
        return true;
    }
    false
}

fn ensure_placeholders(inventory: &mut Inventory) {
    if inventory.asio.is_empty() {
        inventory.asio.push(DeviceItem {
            id: DEMO_ASIO_ID.to_owned(),
            name: format!("No {} input · preview", inventory.low_latency_label),
            backend: inventory.low_latency_label.clone(),
            channels: 2,
            demo: true,
            preferred: true,
            asio_clsid: None,
        });
    }
    if inventory.inputs.is_empty() {
        inventory.inputs.push(DeviceItem {
            id: DEMO_INPUT_ID.to_owned(),
            name: "Studio microphone · preview".to_owned(),
            backend: "SYSTEM".to_owned(),
            channels: 2,
            demo: true,
            preferred: true,
            asio_clsid: None,
        });
    }
    if inventory.outputs.is_empty() {
        inventory.outputs.push(DeviceItem {
            id: DEMO_OUTPUT_ID.to_owned(),
            name: "System speakers · preview".to_owned(),
            backend: if cfg!(target_os = "windows") {
                "WASAPI".to_owned()
            } else if cfg!(target_os = "linux") {
                inventory.low_latency_label.clone()
            } else {
                "ALSA".to_owned()
            },
            channels: 2,
            demo: true,
            preferred: true,
            asio_clsid: None,
        });
    }
}

enum Side {
    Ready(usize),
    /// The device has this direction, but opening it failed.
    Unreadable,
    /// The device does not offer this direction.
    Absent,
}

fn output_side(device: &cpal::Device) -> Side {
    if let Ok(config) = device.default_output_config() {
        return Side::Ready(config.channels() as usize);
    }
    match device.supported_output_configs() {
        Ok(mut configs) => match configs.next() {
            Some(range) => Side::Ready(range.channels() as usize),
            None => Side::Absent,
        },
        Err(_) => Side::Unreadable,
    }
}

fn input_side(device: &cpal::Device) -> Side {
    if let Ok(config) = device.default_input_config() {
        return Side::Ready(config.channels() as usize);
    }
    match device.supported_input_configs() {
        Ok(mut configs) => match configs.next() {
            Some(range) => Side::Ready(range.channels() as usize),
            None => Side::Absent,
        },
        Err(_) => Side::Unreadable,
    }
}

/// Drops ALSA plugin aliases of a card that is already listed as `hw:` or
/// `plughw:`. `default`, `pulse`, and `pipewire` stay: they are not one card.
fn hide_redundant_alsa_aliases(devices: &mut Vec<DeviceItem>) {
    let direct_cards: std::collections::HashSet<String> = devices
        .iter()
        .filter(|device| pcm_is_direct(&device.id))
        .filter_map(|device| alsa_card_key(&device.id))
        .collect();
    if direct_cards.is_empty() {
        return;
    }
    devices.retain(|device| {
        if !is_card_alias_plugin(&device.id) {
            return true;
        }
        match alsa_card_key(&device.id) {
            Some(card) => !direct_cards.contains(&card),
            None => true,
        }
    });
}

fn pcm_name(device_id: &str) -> &str {
    match device_id.split_once(':') {
        Some((host, pcm)) if host.eq_ignore_ascii_case("alsa") => pcm.trim(),
        _ => device_id.trim(),
    }
}

fn pcm_is_direct(device_id: &str) -> bool {
    let pcm = pcm_name(device_id);
    pcm.starts_with("hw:") || pcm.starts_with("plughw:")
}

/// `hw:CARD=GP5,DEV=0` and `front:CARD=GP5` share the key `GP5`.
fn alsa_card_key(device_id: &str) -> Option<String> {
    let pcm = pcm_name(device_id);
    let (_, args) = pcm.split_once(':')?;
    for part in args.split([',', ':']) {
        let part = part.trim();
        if let Some(card) = part.strip_prefix("CARD=") {
            if !card.is_empty() {
                return Some(card.to_owned());
            }
        }
    }
    let first = args.split(',').next()?.trim();
    if !first.is_empty() && first.chars().all(|c| c.is_ascii_digit()) {
        return Some(first.to_owned());
    }
    None
}

fn pcm_plugin(device_id: &str) -> String {
    let pcm = pcm_name(device_id);
    pcm.split(':')
        .next()
        .unwrap_or(pcm)
        .trim()
        .to_ascii_lowercase()
}

/// Plugin views of one card. These are the rows that repeat the card name.
fn is_card_alias_plugin(device_id: &str) -> bool {
    let plugin = pcm_plugin(device_id);
    matches!(
        plugin.as_str(),
        "sysdefault"
            | "front"
            | "iec958"
            | "spdif"
            | "hdmi"
            | "dmix"
            | "dsnoop"
            | "usbstream"
            | "modem"
            | "phoneline"
    ) || plugin.starts_with("surround")
}

/// The short tag that tells two rows of the same card apart (`hw`, `plughw`).
pub(crate) fn device_pcm_tag(device_id: &str) -> Option<String> {
    let plugin = pcm_plugin(device_id);
    if plugin.is_empty() || plugin.eq_ignore_ascii_case("alsa") {
        return None;
    }
    Some(plugin)
}

/// Menu text for one device. When several rows share a display name and a
/// backend, the PCM plugin is what makes them different.
pub(crate) fn device_choice_label(device: &DeviceItem, peers: &[DeviceItem]) -> String {
    let shared_name = peers.iter().any(|other| {
        other.id != device.id
            && other.name.eq_ignore_ascii_case(&device.name)
            && other.backend.eq_ignore_ascii_case(&device.backend)
    });
    if shared_name {
        if let Some(node) = device.id.strip_prefix("pipewire:") {
            if !node.is_empty() {
                return format!("{} · {node}", device.name);
            }
        }
        if let Some(tag) = device_pcm_tag(&device.id) {
            return format!("{} · {tag}", device.name);
        }
    }
    device.name.clone()
}

/// Puts registry nodes in front of the cpal list and drops the ALSA shim
/// for each direction that has a real node.
#[cfg(target_os = "linux")]
fn merge_native_nodes(
    outputs: &mut Vec<DeviceItem>,
    inputs: &mut Vec<DeviceItem>,
    notes: &mut Vec<String>,
) {
    let nodes = match super::linux::native::list_nodes() {
        Ok(nodes) => nodes,
        Err(error) => {
            notes.push(format!(
                "PipeWire is not available ({error}). Its ALSA device stays in the list."
            ));
            return;
        }
    };
    if nodes.sources.is_empty() && nodes.sinks.is_empty() {
        notes.push("PipeWire is running but published no audio sources or sinks.".to_owned());
        return;
    }
    if !nodes.sources.is_empty() {
        inputs.retain(|device| !super::linux::native::is_alsa_shim(&device.id));
        let mut sources = nodes.sources;
        sources.append(inputs);
        *inputs = sources;
    }
    if !nodes.sinks.is_empty() {
        outputs.retain(|device| !super::linux::native::is_alsa_shim(&device.id));
        let mut sinks = nodes.sinks;
        sinks.append(outputs);
        *outputs = sinks;
    }
}

/// Replaces native rows from a fresh registry pass without opening a PCM.
#[cfg(target_os = "linux")]
fn refresh_native_presence(inventory: &mut Inventory) {
    let Ok(nodes) = super::linux::native::list_nodes() else {
        return;
    };
    let preferred_inputs: std::collections::HashSet<String> = inventory
        .asio
        .iter()
        .chain(inventory.inputs.iter())
        .filter(|device| device.preferred && super::linux::native::is_native_id(&device.id))
        .map(|device| device.id.clone())
        .collect();
    let preferred_outputs: std::collections::HashSet<String> = inventory
        .outputs
        .iter()
        .filter(|device| device.preferred && super::linux::native::is_native_id(&device.id))
        .map(|device| device.id.clone())
        .collect();
    for list in [
        &mut inventory.asio,
        &mut inventory.inputs,
        &mut inventory.outputs,
    ] {
        list.retain(|device| !super::linux::native::is_native_id(&device.id));
    }
    if !nodes.sources.is_empty() {
        for list in [&mut inventory.asio, &mut inventory.inputs] {
            list.retain(|device| !super::linux::native::is_alsa_shim(&device.id) && !device.demo);
        }
        let mut sources = nodes.sources;
        for device in &mut sources {
            device.preferred = preferred_inputs.contains(&device.id);
        }
        if !sources.iter().any(|device| device.preferred)
            && !inventory.asio.iter().any(|device| device.preferred)
        {
            sources[0].preferred = true;
        }
        sources.append(&mut inventory.asio);
        inventory.asio = sources;
    }
    if !nodes.sinks.is_empty() {
        inventory
            .outputs
            .retain(|device| !super::linux::native::is_alsa_shim(&device.id) && !device.demo);
        let mut sinks = nodes.sinks;
        for device in &mut sinks {
            device.preferred = preferred_outputs.contains(&device.id);
        }
        if !sinks.iter().any(|device| device.preferred)
            && !inventory.outputs.iter().any(|device| device.preferred)
        {
            sinks[0].preferred = true;
        }
        sinks.append(&mut inventory.outputs);
        inventory.outputs = sinks;
    }
}

fn remembered(devices: &[DeviceItem], id: &str, backend: &str) -> Option<DeviceItem> {
    devices
        .iter()
        .find(|device| {
            !device.demo && device.id == id && device.backend.eq_ignore_ascii_case(backend)
        })
        .cloned()
}

/// Drops a repeated endpoint. The key is the stable id plus the backend, so
/// two hosts that share a display name (WASAPI and ASIO) and two ALSA aliases
/// (`hw:` and `plughw:`) both stay selectable. The first row wins, which is
/// the preferred one after the inventory has been sorted.
fn dedup_devices(devices: &mut Vec<DeviceItem>) {
    let mut seen = std::collections::HashSet::new();
    devices.retain(|device| seen.insert((device.id.clone(), device.backend.to_ascii_lowercase())));
}

/// PipeWire-backed endpoints are preferred on Linux.
#[cfg(any(target_os = "linux", test))]
fn prefer_pipewire(devices: &mut [DeviceItem]) -> bool {
    let Some(index) = devices.iter().position(pipewire::is_pipewire_device) else {
        return false;
    };
    for device in devices.iter_mut() {
        device.preferred = false;
    }
    devices[index].preferred = true;
    true
}

/// Low-latency groups list PipeWire first, then JACK, then direct ALSA.
#[cfg(target_os = "linux")]
fn low_latency_rank(backend: &str) -> u8 {
    if backend.eq_ignore_ascii_case("PIPEWIRE") {
        0
    } else if backend.eq_ignore_ascii_case("JACK") {
        1
    } else {
        2
    }
}

/// The shared answer to "does this device belong on the low-latency tab?".
/// ASIO is low latency on Windows.  On Linux, PipeWire is the default backend,
/// JACK is the dedicated graph, and direct ALSA hardware PCMs are the
/// exclusive fallback.  PulseAudio and other virtual plugins stay on the
/// system side.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn is_low_latency_device(device: &DeviceItem, low_latency_label: &str) -> bool {
    if device.asio_clsid.is_some() {
        return true;
    }
    if pipewire::is_pipewire_device(device) {
        return true;
    }
    if device.backend.eq_ignore_ascii_case("JACK") {
        return true;
    }
    if device.backend.eq_ignore_ascii_case("ALSA") && alsa_pcm_is_direct(&device.id) {
        return true;
    }
    // The label names the low-latency group ("ALSA" when no PipeWire or JACK
    // endpoint was found). It must not pull pulse, default, or sysdefault
    // onto that side.
    let _ = low_latency_label;
    false
}

#[cfg(not(any(target_os = "linux", test)))]
pub(crate) fn is_low_latency_device(device: &DeviceItem, low_latency_label: &str) -> bool {
    device.asio_clsid.is_some() || device.backend.eq_ignore_ascii_case(low_latency_label)
}

pub(crate) fn pick_output_config(
    device: &cpal::Device,
    sample_rate: u32,
) -> Result<cpal::SupportedStreamConfig, String> {
    if let Ok(default) = device.default_output_config() {
        if default.sample_rate() == sample_rate && default.channels() >= 2 {
            return Ok(default);
        }
    }
    if let Ok(configs) = device.supported_output_configs() {
        for range in configs {
            if range.channels() >= 1
                && range.min_sample_rate() <= sample_rate
                && range.max_sample_rate() >= sample_rate
            {
                return Ok(range.with_sample_rate(sample_rate));
            }
        }
    }
    device
        .default_output_config()
        .map_err(|error| format!("No usable output configuration: {error}"))
}

pub(crate) fn pick_input_config(
    device: &cpal::Device,
    sample_rate: u32,
) -> Result<cpal::SupportedStreamConfig, String> {
    if let Ok(default) = device.default_input_config() {
        if default.sample_rate() == sample_rate {
            return Ok(default);
        }
    }
    if let Ok(configs) = device.supported_input_configs() {
        for range in configs {
            if range.min_sample_rate() <= sample_rate && range.max_sample_rate() >= sample_rate {
                return Ok(range.with_sample_rate(sample_rate));
            }
        }
    }
    device
        .default_input_config()
        .map_err(|error| format!("No usable input configuration: {error}"))
}

#[cfg(test)]
mod tests {
    use super::{dedup_devices, is_low_latency_device};
    use crate::audio::DeviceItem;

    fn device(id: &str, backend: &str) -> DeviceItem {
        named(id, "test", backend)
    }

    fn named(id: &str, name: &str, backend: &str) -> DeviceItem {
        DeviceItem {
            id: id.to_owned(),
            name: name.to_owned(),
            backend: backend.to_owned(),
            channels: 2,
            demo: false,
            preferred: false,
            asio_clsid: None,
        }
    }

    #[test]
    fn same_name_on_different_backends_both_remain() {
        let mut devices = vec![
            named("wasapi:{spk}", "Speakers", "WASAPI"),
            // Same endpoint listed twice; backend case must not make a second row.
            named("wasapi:{spk}", "Speakers", "wasapi"),
            named("asio-out:abc", "Speakers", "ASIO"),
            named("ALSA:hw:CARD=USB,DEV=0", "USB Audio", "ALSA"),
            named("ALSA:plughw:CARD=USB,DEV=0", "USB Audio", "ALSA"),
        ];
        dedup_devices(&mut devices);
        let keys: Vec<_> = devices
            .iter()
            .map(|device| (device.id.as_str(), device.backend.as_str()))
            .collect();
        assert_eq!(
            keys,
            [
                ("wasapi:{spk}", "WASAPI"),
                ("asio-out:abc", "ASIO"),
                ("ALSA:hw:CARD=USB,DEV=0", "ALSA"),
                ("ALSA:plughw:CARD=USB,DEV=0", "ALSA"),
            ]
        );
        // Each surviving id is its own row, so both "Speakers" can be chosen.
        assert_eq!(
            devices
                .iter()
                .position(|device| device.id == "wasapi:{spk}"),
            Some(0)
        );
        assert_eq!(
            devices
                .iter()
                .position(|device| device.id == "asio-out:abc"),
            Some(1)
        );
    }

    #[test]
    fn pipewire_is_the_low_latency_backend() {
        let pipewire = device("ALSA:pipewire", "ALSA");
        assert!(is_low_latency_device(&pipewire, "PIPEWIRE"));
        assert!(is_low_latency_device(&pipewire, "ALSA"));
    }

    #[test]
    fn virtual_alsa_pcms_other_than_pipewire_stay_on_the_system_side() {
        for id in ["ALSA:pulse", "ALSA:sysdefault:CARD=0"] {
            assert!(
                !is_low_latency_device(&device(id, "ALSA"), "PIPEWIRE"),
                "{id}"
            );
            assert!(
                !is_low_latency_device(&device(id, "ALSA"), "ALSA"),
                "{id} with an ALSA label"
            );
        }
    }

    #[test]
    fn one_usb_card_does_not_repeat_under_every_alsa_plugin() {
        let card = "GP-5, USB Audio";
        let mut devices = vec![
            named("alsa:hw:CARD=GP5,DEV=0", card, "ALSA"),
            named("alsa:plughw:CARD=GP5,DEV=0", card, "ALSA"),
            named("alsa:sysdefault:CARD=GP5", card, "ALSA"),
            named("alsa:front:CARD=GP5", card, "ALSA"),
            named("alsa:usbstream:CARD=GP5", card, "ALSA"),
        ];
        super::hide_redundant_alsa_aliases(&mut devices);
        let ids: Vec<_> = devices.iter().map(|device| device.id.as_str()).collect();
        assert_eq!(
            ids,
            ["alsa:hw:CARD=GP5,DEV=0", "alsa:plughw:CARD=GP5,DEV=0",]
        );
        assert_eq!(
            super::device_choice_label(&devices[0], &devices),
            "GP-5, USB Audio · hw"
        );
        assert_eq!(
            super::device_choice_label(&devices[1], &devices),
            "GP-5, USB Audio · plughw"
        );
    }

    #[test]
    fn two_pipewire_nodes_with_one_description_show_the_node_name() {
        let devices = vec![
            named("pipewire:alsa_input.usb-a", "USB Audio", "PIPEWIRE"),
            named("pipewire:alsa_input.usb-b", "USB Audio", "PIPEWIRE"),
        ];
        assert_eq!(
            super::device_choice_label(&devices[0], &devices),
            "USB Audio · alsa_input.usb-a"
        );
        assert!(is_low_latency_device(&devices[0], "ALSA"));
    }

    #[test]
    fn a_card_that_only_has_sysdefault_is_kept() {
        let mut devices = vec![named("alsa:sysdefault:CARD=GP5", "GP-5, USB Audio", "ALSA")];
        super::hide_redundant_alsa_aliases(&mut devices);
        assert_eq!(devices.len(), 1);
        assert_eq!(
            super::device_choice_label(&devices[0], &devices),
            "GP-5, USB Audio"
        );
    }

    #[test]
    fn direct_alsa_and_jack_are_low_latency() {
        let direct = device("ALSA:hw:CARD=0,DEV=0", "ALSA");
        assert!(is_low_latency_device(&direct, "PIPEWIRE"));
        assert!(is_low_latency_device(&direct, "JACK"));
        assert!(is_low_latency_device(
            &device("JACK:system", "JACK"),
            "PIPEWIRE"
        ));
    }
}
