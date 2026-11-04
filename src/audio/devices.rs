//! Device discovery and choosing a stream configuration for a device.

use cpal::traits::{DeviceTrait, HostTrait};

use super::types::{DeviceItem, Inventory, DEMO_ASIO_ID, DEMO_INPUT_ID, DEMO_OUTPUT_ID};

pub fn scan_devices() -> Inventory {
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

            // ALSA publishes sink/source plugins that cannot carry a guitar:
            // "null" discards everything and "oss" is the legacy emulation.
            // Leaving them out keeps the first real device as the default.
            #[cfg(target_os = "linux")]
            if host_name.eq_ignore_ascii_case("ALSA")
                && id.as_deref().is_some_and(alsa_pcm_is_placeholder)
            {
                continue;
            }

            if let Ok(config) = device.default_output_config() {
                if let Some(id) = &id {
                    output_candidates.push(DeviceItem {
                        id: id.clone(),
                        name: name.clone(),
                        backend: host_name.to_owned(),
                        channels: config.channels() as usize,
                        demo: false,
                        preferred: default_output_id.as_deref() == Some(id.as_str()),
                        asio_clsid: None,
                    });
                }
            } else if let Ok(mut configs) = device.supported_output_configs() {
                if let Some(range) = configs.next() {
                    if let Some(id) = &id {
                        output_candidates.push(DeviceItem {
                            id: id.clone(),
                            name: name.clone(),
                            backend: host_name.to_owned(),
                            channels: range.channels() as usize,
                            demo: false,
                            preferred: default_output_id.as_deref() == Some(id.as_str()),
                            asio_clsid: None,
                        });
                    }
                }
            }

            if let Ok(config) = device.default_input_config() {
                if let Some(id) = &id {
                    input_candidates.push(DeviceItem {
                        id: id.clone(),
                        name: name.clone(),
                        backend: host_name.to_owned(),
                        channels: config.channels() as usize,
                        demo: false,
                        preferred: default_input_id.as_deref() == Some(id.as_str()),
                        asio_clsid: None,
                    });
                }
            } else if let Ok(mut configs) = device.supported_input_configs() {
                if let Some(range) = configs.next() {
                    if let Some(id) = &id {
                        input_candidates.push(DeviceItem {
                            id: id.clone(),
                            name,
                            backend: host_name.to_owned(),
                            channels: range.channels() as usize,
                            demo: false,
                            preferred: default_input_id.as_deref() == Some(id.as_str()),
                            asio_clsid: None,
                        });
                    }
                }
            }
        }
    }

    // WASAPI is the host that keeps other apps audible in shared mode, which is
    // exactly what an ASIO input needs on the other side of the bridge.  Rank
    // it above DirectSound/MME, then collapse the same speaker showing up once
    // per host into a single WASAPI row.
    let host_rank = |backend: &str| {
        if backend.eq_ignore_ascii_case("WASAPI") {
            0
        } else {
            1
        }
    };
    // ASIO outputs are their own endpoints: the same speaker reached through
    // ASIO has a different clock, so it must not be collapsed into the
    // system-host row with the same name.
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
    output_candidates.dedup_by(|a, b| a.name.eq_ignore_ascii_case(&b.name));
    asio_output_candidates.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    // Keep the everyday system outputs first: the common case is an ASIO
    // input playing through the speakers Windows already uses.  The ASIO
    // outputs stay available at the end for people who want that path.
    output_candidates.extend(asio_output_candidates);

    input_candidates.sort_by(|a, b| {
        b.preferred
            .cmp(&a.preferred)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    input_candidates.dedup_by(|a, b| a.name == b.name && a.backend == b.backend);

    // On Linux, JACK inputs are the dedicated low-latency path (the platform
    // counterpart to ASIO).  Without JACK, direct ALSA hardware PCMs (`hw:` and
    // `plughw:`) take that role while the virtual `default`, PipeWire and
    // PulseAudio PCMs stay on the system side.  On a machine that exposes only
    // virtual ALSA inputs, fall back to those so the ALSA tab has something to
    // open instead of showing only a preview placeholder.
    #[cfg(target_os = "linux")]
    {
        let mut jack_inputs = Vec::new();
        input_candidates.retain(|device| {
            if device.backend.eq_ignore_ascii_case("JACK") {
                jack_inputs.push(device.clone());
                false
            } else {
                true
            }
        });
        if !jack_inputs.is_empty() {
            inventory.low_latency_label = "JACK".to_owned();
            inventory.asio.extend(jack_inputs);
        } else {
            inventory.low_latency_label = "ALSA".to_owned();
            let mut direct_inputs = Vec::new();
            input_candidates.retain(|device| {
                if device.backend.eq_ignore_ascii_case("ALSA") && alsa_pcm_is_direct(&device.id) {
                    direct_inputs.push(device.clone());
                    false
                } else {
                    true
                }
            });
            if direct_inputs.is_empty() {
                // Containers and some desktops only publish the "default"
                // plugin; keep it usable as the ALSA-side input too.  The
                // system list still has the same device.
                let alsa_inputs: Vec<DeviceItem> = input_candidates
                    .iter()
                    .filter(|device| device.backend.eq_ignore_ascii_case("ALSA"))
                    .cloned()
                    .collect();
                inventory.asio.extend(alsa_inputs);
            } else {
                inventory.asio.extend(direct_inputs);
            }
        }
    }

    inventory.outputs = output_candidates;
    inventory.inputs = input_candidates;

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
            } else {
                "ALSA".to_owned()
            },
            channels: 2,
            demo: true,
            preferred: true,
            asio_clsid: None,
        });
    }

    inventory
}

/// Whether a cpal ALSA id names a physical PCM rather than a virtual plugin.
/// `hw:` bypasses every conversion; `plughw:` still goes straight to the card
/// but lets ALSA convert formats.  Both are the low-latency ALSA side.
#[cfg(any(target_os = "linux", test))]
fn alsa_pcm_name(device_id: &str) -> &str {
    // cpal prints the host in lower case (`alsa:hw:CARD=0,DEV=0`), while other
    // backends and older versions use the display spelling (`ALSA:...`).
    match device_id.split_once(':') {
        Some((host, pcm)) if host.eq_ignore_ascii_case("alsa") => pcm.trim(),
        _ => device_id.trim(),
    }
}

#[cfg(any(target_os = "linux", test))]
pub(crate) fn alsa_pcm_is_direct(device_id: &str) -> bool {
    let pcm = alsa_pcm_name(device_id);
    pcm.starts_with("hw:") || pcm.starts_with("plughw:")
}

#[cfg(any(target_os = "linux", test))]
pub(crate) fn alsa_pcm_is_placeholder(device_id: &str) -> bool {
    let pcm = alsa_pcm_name(device_id);
    pcm == "null" || pcm == "oss" || pcm.starts_with("oss:")
}

/// The shared answer to "does this device belong on the low-latency tab?".
/// ASIO is low latency on Windows; JACK is on Linux; direct ALSA hardware PCMs
/// are Linux's fallback when JACK is absent.  Virtual ALSA devices (`default`,
/// `pipewire`, `pulse`, `sysdefault`) stay on the system side.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn is_low_latency_device(device: &DeviceItem, low_latency_label: &str) -> bool {
    if device.asio_clsid.is_some() {
        return true;
    }
    if device.backend.eq_ignore_ascii_case("ALSA") {
        return low_latency_label.eq_ignore_ascii_case("ALSA") && alsa_pcm_is_direct(&device.id);
    }
    device.backend.eq_ignore_ascii_case(low_latency_label)
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
    use super::{alsa_pcm_is_direct, alsa_pcm_is_placeholder, is_low_latency_device};
    use crate::audio::DeviceItem;

    fn device(id: &str, backend: &str) -> DeviceItem {
        DeviceItem {
            id: id.to_owned(),
            name: "test".to_owned(),
            backend: backend.to_owned(),
            channels: 2,
            demo: false,
            preferred: false,
            asio_clsid: None,
        }
    }

    #[test]
    fn direct_alsa_pcms_are_recognised() {
        assert!(alsa_pcm_is_direct("ALSA:hw:CARD=0,DEV=0"));
        assert!(alsa_pcm_is_direct("ALSA:plughw:CARD=1,DEV=0"));
        assert!(alsa_pcm_is_direct("alsa:hw:CARD=0,DEV=0"));
        assert!(alsa_pcm_is_direct("alsa:plughw:CARD=1,DEV=0"));
        assert!(alsa_pcm_is_direct("hw:CARD=0,DEV=0"));
    }

    #[test]
    fn virtual_alsa_pcms_stay_on_the_system_side() {
        for id in [
            "ALSA:default",
            "ALSA:pipewire",
            "ALSA:pulse",
            "ALSA:sysdefault:CARD=0",
        ] {
            assert!(!alsa_pcm_is_direct(id), "{id}");
            assert!(!is_low_latency_device(&device(id, "ALSA"), "ALSA"), "{id}");
        }
    }

    #[test]
    fn sink_plugins_that_cannot_carry_audio_are_skipped() {
        assert!(alsa_pcm_is_placeholder("ALSA:null"));
        assert!(alsa_pcm_is_placeholder("alsa:null"));
        assert!(alsa_pcm_is_placeholder("ALSA:oss"));
        assert!(alsa_pcm_is_placeholder("ALSA:oss:default"));
        assert!(!alsa_pcm_is_placeholder("ALSA:pulse"));
        assert!(!alsa_pcm_is_placeholder("ALSA:hw:CARD=0,DEV=0"));
    }

    #[test]
    fn a_direct_alsa_pcm_is_low_latency_only_when_alsa_owns_that_side() {
        let direct = device("ALSA:hw:CARD=0,DEV=0", "ALSA");
        assert!(is_low_latency_device(&direct, "ALSA"));
        assert!(!is_low_latency_device(&direct, "JACK"));
        assert!(is_low_latency_device(
            &device("JACK:system", "JACK"),
            "JACK"
        ));
    }
}
