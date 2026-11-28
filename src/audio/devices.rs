//! Asking ALSA for a configuration opens a PCM.  A scan while the bridge
//! holds the device must not, or a busy device looks unplugged.
//!
//! The ALSA and native PipeWire id grammar lives here: this file is the
//! copy that compiles on every platform.

use cpal::traits::{DeviceTrait, HostTrait};

#[cfg(target_os = "linux")]
use super::linux::alsa::alsa_pcm_is_placeholder;
#[cfg(any(target_os = "linux", test))]
use super::linux::pipewire;
use super::types::{DeviceItem, Inventory, DEMO_ASIO_ID, DEMO_INPUT_ID, DEMO_OUTPUT_ID};

pub fn scan_devices() -> Inventory {
    scan_devices_from(&Inventory::default(), true)
}

/// Preview rows, enough to paint the shell before the first real scan.
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

/// A device whose configuration cannot be read is kept when `previous`
/// already listed it.
///
/// `probe_configs` is false while a bridge is opening or running.
/// Enumerating names does not open a PCM.  Asking ALSA for a configuration
/// does, and that open fails on a device the bridge already holds.
pub(crate) fn scan_devices_from(previous: &Inventory, probe_configs: bool) -> Inventory {
    if !probe_configs {
        return presence_from(previous);
    }
    let mut inventory = Inventory {
        // Playback is never the low-latency driver.
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
                    // The same driver is also a playback device, so the protocol
                    // does not decide which side it appears on.
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

    // Only the Windows default.  Marking every host's default would let an
    // arbitrary MME or DirectSound device win the preferred sort.
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

            // "null" discards audio and "oss" is the legacy emulation.
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

    // When the card already has `hw:` or `plughw:`, the plugin aliases add
    // nothing the person can choose.
    hide_redundant_alsa_aliases(&mut output_candidates);
    hide_redundant_alsa_aliases(&mut input_candidates);

    // Real nodes replace the `alsa:pipewire` shim.  If the server is down
    // the shim stays, so a device can still be opened.
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

    // WASAPI stays audible for other apps, which is what an ASIO input
    // needs on the other side.  Dedup is id plus backend, so two hosts and
    // `hw:`/`plughw:` both stay.
    let host_rank = |backend: &str| {
        if backend.eq_ignore_ascii_case("WASAPI") {
            0
        } else {
            1
        }
    };
    // So the everyday system output stays the first row.
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
    // The common case is an ASIO input through the speakers Windows already
    // uses.
    output_candidates.extend(asio_output_candidates);

    input_candidates.sort_by(|a, b| {
        b.preferred
            .cmp(&a.preferred)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    dedup_devices(&mut input_candidates);

    // PipeWire, then JACK, then direct ALSA.  Virtual pulse/default stay on
    // the system side.  A machine with only virtual inputs falls back to
    // those.
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
            // A machine that only publishes "default" still needs a
            // low-latency input.
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
        // Preferred PipeWire stays first even when JACK or direct ALSA also
        // landed here.
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

/// Ids, without opening a stream configuration.
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

/// Does not open a PCM.  New devices wait for a probing scan.
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
    // Native ids are not cpal ids.  A failed registry round-trip leaves
    // the previous nodes in place.
    #[cfg(target_os = "linux")]
    if is_native_id(&device.id) {
        return true;
    }
    false
}

fn ensure_placeholders(inventory: &mut Inventory) {
    if inventory.asio.is_empty() {
        inventory.asio.push(DeviceItem {
            id: DEMO_ASIO_ID.to_owned(),
            name: format!("No {} input", inventory.low_latency_label),
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
            name: "Studio microphone".to_owned(),
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
            name: "System speakers".to_owned(),
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

/// `default`, `pulse`, and `pipewire` stay: they are not one card.
fn hide_redundant_alsa_aliases(devices: &mut Vec<DeviceItem>) {
    let direct_cards: std::collections::HashSet<String> = devices
        .iter()
        .filter(|device| alsa_pcm_is_direct(&device.id))
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

/// The only copy of the id grammar.  `linux::alsa` re-exports it.
///
/// cpal prints the host in lower case; other backends use the display
/// spelling.
pub(crate) fn alsa_pcm_name(device_id: &str) -> &str {
    match device_id.split_once(':') {
        Some((host, pcm)) if host.eq_ignore_ascii_case("alsa") => pcm.trim(),
        _ => device_id.trim(),
    }
}

/// `hw:` and `plughw:` are both the low-latency side.
pub(crate) fn alsa_pcm_is_direct(device_id: &str) -> bool {
    let pcm = alsa_pcm_name(device_id);
    pcm.starts_with("hw:") || pcm.starts_with("plughw:")
}

/// `hw:CARD=GP5,DEV=0` and `front:CARD=GP5` share the key `GP5`.
fn alsa_card_key(device_id: &str) -> Option<String> {
    let pcm = alsa_pcm_name(device_id);
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
    let pcm = alsa_pcm_name(device_id);
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

/// The host prefix a native PipeWire id carries.
pub(crate) const NATIVE_PREFIX: &str = "pipewire:";

/// The only copy of the native-id grammar.  `linux::native` re-exports it.
/// The bare `alsa:pipewire` plugin id does not match.
pub(crate) fn node_name(device_id: &str) -> Option<&str> {
    let name = device_id.strip_prefix(NATIVE_PREFIX)?;
    if name.is_empty() || name.eq_ignore_ascii_case("pipewire") {
        None
    } else {
        Some(name)
    }
}

pub(crate) fn is_native_id(device_id: &str) -> bool {
    node_name(device_id).is_some()
}

/// When several rows share a name and a backend, the PCM plugin is what
/// makes them different.
pub(crate) fn device_choice_label(device: &DeviceItem, peers: &[DeviceItem]) -> String {
    let shared_name = peers.iter().any(|other| {
        other.id != device.id
            && other.name.eq_ignore_ascii_case(&device.name)
            && other.backend.eq_ignore_ascii_case(&device.backend)
    });
    if shared_name {
        if let Some(node) = node_name(&device.id) {
            return format!("{} · {node}", device.name);
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
        notes.push("PipeWire is running but offers no audio sources or sinks.".to_owned());
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
        .filter(|device| device.preferred && is_native_id(&device.id))
        .map(|device| device.id.clone())
        .collect();
    let preferred_outputs: std::collections::HashSet<String> = inventory
        .outputs
        .iter()
        .filter(|device| device.preferred && is_native_id(&device.id))
        .map(|device| device.id.clone())
        .collect();
    for list in [
        &mut inventory.asio,
        &mut inventory.inputs,
        &mut inventory.outputs,
    ] {
        list.retain(|device| !is_native_id(&device.id));
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

/// The key is the id plus the backend, so two hosts and `hw:`/`plughw:`
/// both stay.  The first row wins.
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

/// The decision both targets share: an ASIO driver, or a backend the target's
/// low-latency group claims.  `in_platform_group` is that second input; only
/// the way each target resolves it differs.
fn low_latency_decision(device: &DeviceItem, in_platform_group: bool) -> bool {
    device.asio_clsid.is_some() || in_platform_group
}

/// The label names the group after the fact, so it must not pull pulse,
/// default, or sysdefault onto that side.
#[cfg(target_os = "linux")]
pub(crate) fn is_low_latency_device(device: &DeviceItem, low_latency_label: &str) -> bool {
    let _ = low_latency_label;
    low_latency_decision(
        device,
        pipewire::is_pipewire_device(device)
            || device.backend.eq_ignore_ascii_case("JACK")
            || (device.backend.eq_ignore_ascii_case("ALSA") && alsa_pcm_is_direct(&device.id)),
    )
}

/// The backend is matched against the label.  The other system hosts stay
/// on the system side.
#[cfg(not(target_os = "linux"))]
pub(crate) fn is_low_latency_device(device: &DeviceItem, low_latency_label: &str) -> bool {
    low_latency_decision(
        device,
        device.backend.eq_ignore_ascii_case(low_latency_label),
    )
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
mod tests;
