//! Which devices are selected, what they are, and keeping the lists current
//! when devices are plugged in or out.

use std::sync::mpsc;
use std::sync::mpsc::TryRecvError;
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use crate::app::RigWerkApp;
use crate::audio::{self, DeviceItem, Inventory};
use crate::ui::theme::{AMBER, BLUE, GREEN, MUTED, RED};

/// The device to fall back on when nothing has been saved (first launch, or
/// the remembered device is gone): the one the system marks as its default,
/// otherwise the first real device in the list.  No device is favoured by
/// name, because that choice belongs to the person using the app.
pub(crate) fn default_index(devices: &[DeviceItem]) -> usize {
    if let Some(index) = devices.iter().position(|device| device.preferred) {
        return index;
    }
    devices.iter().position(|device| !device.demo).unwrap_or(0)
}

/// Drivers that wrap a sound card Windows already owns instead of talking to
/// their own hardware.  ASIO4ALL is the common one; FlexASIO and ASIO2KS
/// behave the same way.  Their device name says nothing about the physical
/// hardware, so they only get a tailored hint when an open actually fails.
pub(crate) const WRAPPER_DRIVERS: [&str; 6] = [
    "asio4all",
    "asio2ks",
    "flexasio",
    "asio link",
    "voicemeeter",
    "jackrouter",
];

pub(crate) fn is_wrapper_driver(device: &DeviceItem) -> bool {
    let name = device.name.to_lowercase();
    WRAPPER_DRIVERS.iter().any(|needle| name.contains(needle))
}

/// Stable summary of the device lists so the watcher can tell a real
/// plug/unplug event apart from an unchanged scan.
pub(crate) fn inventory_fingerprint(inventory: &Inventory) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    for devices in [&inventory.asio, &inventory.inputs, &inventory.outputs] {
        for device in devices {
            device.id.hash(&mut hasher);
            device.name.hash(&mut hasher);
            device.backend.hash(&mut hasher);
            device.channels.hash(&mut hasher);
            device.demo.hash(&mut hasher);
            device.preferred.hash(&mut hasher);
        }
        u8::MAX.hash(&mut hasher);
    }
    hasher.finish()
}

pub(crate) fn device_present(devices: &[DeviceItem], id: &str, name: &str) -> bool {
    devices
        .iter()
        .any(|device| device.id == id || device.name.eq_ignore_ascii_case(name))
}

pub(crate) fn device_index(devices: &[DeviceItem], id: &str, name: &str) -> Option<usize> {
    devices
        .iter()
        .position(|device| device.id == id)
        .or_else(|| {
            devices
                .iter()
                .position(|device| device.name.eq_ignore_ascii_case(name))
        })
}

/// Last-resort entries so a momentarily empty device list can never make the
/// window panic while it is being redrawn.
pub(crate) fn missing_input_device() -> &'static DeviceItem {
    static MISSING: OnceLock<DeviceItem> = OnceLock::new();
    MISSING.get_or_init(|| DeviceItem {
        id: "missing:input".to_owned(),
        name: "No input device".to_owned(),
        backend: "SYSTEM".to_owned(),
        channels: 2,
        demo: true,
        preferred: false,
        asio_clsid: None,
    })
}

pub(crate) fn missing_output_device() -> &'static DeviceItem {
    static MISSING: OnceLock<DeviceItem> = OnceLock::new();
    MISSING.get_or_init(|| DeviceItem {
        id: "missing:output".to_owned(),
        name: "No output device".to_owned(),
        backend: "SYSTEM".to_owned(),
        channels: 2,
        demo: true,
        preferred: false,
        asio_clsid: None,
    })
}

impl RigWerkApp {
    pub(crate) fn selected_asio(&self) -> &DeviceItem {
        self.inventory
            .asio
            .get(self.asio_idx)
            .or_else(|| self.inventory.asio.first())
            .unwrap_or_else(|| missing_input_device())
    }

    pub(crate) fn using_other_input(&self) -> bool {
        !self.use_asio_input && self.inventory.inputs.get(self.input_idx).is_some()
    }

    pub(crate) fn active_input(&self) -> &DeviceItem {
        if self.using_other_input() {
            self.inventory
                .inputs
                .get(self.input_idx)
                .or_else(|| self.inventory.inputs.first())
                .unwrap_or_else(|| missing_input_device())
        } else {
            self.selected_asio()
        }
    }

    pub(crate) fn selected_output(&self) -> &DeviceItem {
        self.inventory
            .outputs
            .get(self.output_idx)
            .or_else(|| self.inventory.outputs.first())
            .unwrap_or_else(|| missing_output_device())
    }

    pub(crate) fn is_preview(&self) -> bool {
        self.active_input().demo || self.selected_output().demo
    }

    /// Watches for hot-plug events.  Changes are applied to the device lists
    /// immediately; if a device the bridge is using disappears, the bridge is
    /// stopped and the person gets a clear message.
    pub(crate) fn watch_devices(&mut self) {
        // Audio hosting APIs can block for seconds while another app (or an
        // ASIO driver) holds a device.  Enumerating on the UI thread made the
        // window stop responding, so the scan runs on its own thread and the
        // result is picked up here.
        let received = self.device_scan.as_ref().map(|scan| scan.try_recv());
        let mut finished = None;
        match received {
            Some(Ok(fresh)) => {
                self.device_scan = None;
                finished = Some(fresh);
            }
            Some(Err(TryRecvError::Disconnected)) => {
                self.device_scan = None;
            }
            _ => {}
        }
        if let Some(fresh) = finished {
            let changed = self.apply_inventory(fresh);
            if changed {
                // The driver under a device can change while its name or id
                // stays the same.  Throw away any old answer (and any check
                // still in flight) and ask again.
                self.driver_probe = None;
                self.driver_probe_job = None;
                self.last_output_driver = None;
                self.start_output_driver_check(false);
                #[cfg(target_os = "linux")]
                self.refresh_quantum_check(true);
            }
            if !changed && self.manual_scan {
                self.push_log("Scan complete — no device changes.", GREEN);
                self.start_output_driver_check(true);
            }
            self.manual_scan = false;
        }
        if self.device_scan.is_none()
            && self.last_device_scan.elapsed() >= Duration::from_millis(2500)
        {
            self.start_device_scan();
        }
    }

    pub(crate) fn start_device_scan(&mut self) {
        self.last_device_scan = Instant::now();
        let (sender, receiver) = mpsc::channel();
        let scan = thread::Builder::new()
            .name("rigwerk-scan".to_owned())
            .spawn(move || {
                let _ = sender.send(audio::scan_devices());
            });
        match scan {
            Ok(_) => self.device_scan = Some(receiver),
            Err(_) => {
                // No spare thread: fall back to scanning here, once.
                let fresh = audio::scan_devices();
                self.apply_inventory(fresh);
            }
        }
    }

    pub(crate) fn rescan(&mut self) {
        if self.device_scan.is_some() {
            self.push_log("Already scanning audio devices…", MUTED);
            return;
        }
        self.manual_scan = true;
        self.push_log("Scanning audio devices…", BLUE);
        self.start_device_scan();
    }

    pub(crate) fn apply_inventory(&mut self, fresh: Inventory) -> bool {
        if inventory_fingerprint(&fresh) == self.inventory_fingerprint {
            return false;
        }

        let bridge_was_requested = self.bridge_signature != 0;
        let asio_id = self.selected_asio().id.clone();
        let asio_name = self.selected_asio().name.clone();
        let asio_demo = self.selected_asio().demo;
        let input_id = self
            .inventory
            .inputs
            .get(self.input_idx)
            .map(|device| device.id.clone());
        let input_name = self
            .inventory
            .inputs
            .get(self.input_idx)
            .map(|device| device.name.clone());
        let input_demo = self
            .inventory
            .inputs
            .get(self.input_idx)
            .map(|device| device.demo)
            .unwrap_or(true);
        let output_id = self.selected_output().id.clone();
        let output_name = self.selected_output().name.clone();
        let output_demo = self.selected_output().demo;
        let output_asio = self.selected_output().asio_clsid.is_some();

        let input_present = if self.using_other_input() {
            match (&input_id, &input_name) {
                (Some(id), Some(name)) => device_present(&fresh.inputs, id, name),
                _ => false,
            }
        } else {
            device_present(&fresh.asio, &asio_id, &asio_name)
        };
        let output_present = if output_asio {
            fresh.outputs.iter().any(|device| device.id == output_id)
        } else {
            device_present(&fresh.outputs, &output_id, &output_name)
        };
        let active_input_demo = if self.using_other_input() {
            input_demo
        } else {
            asio_demo
        };

        self.inventory = fresh;
        self.inventory_fingerprint = inventory_fingerprint(&self.inventory);
        self.asio_idx = device_index(&self.inventory.asio, &asio_id, &asio_name)
            .unwrap_or_else(|| default_index(&self.inventory.asio));
        self.input_idx = match (&input_id, &input_name) {
            (Some(id), Some(name)) => device_index(&self.inventory.inputs, id, name).unwrap_or(0),
            _ => 0,
        };
        self.output_idx = device_index(&self.inventory.outputs, &output_id, &output_name)
            .unwrap_or_else(|| default_index(&self.inventory.outputs));

        let asio_count = self
            .inventory
            .asio
            .iter()
            .filter(|device| !device.demo)
            .count();
        let output_count = self
            .inventory
            .outputs
            .iter()
            .filter(|device| !device.demo)
            .count();

        let mut missing: Vec<String> = Vec::new();
        if !input_present && !active_input_demo {
            missing.push(if self.using_other_input() {
                input_name.clone().unwrap_or_else(|| asio_name.clone())
            } else {
                asio_name.clone()
            });
        }
        if !output_present && !output_demo {
            missing.push(output_name.clone());
        }

        if missing.is_empty() {
            self.push_log(
                format!(
                    "Device change applied · {asio_count} low-latency / {output_count} output devices."
                ),
                BLUE,
            );
            return true;
        }

        let (subject, verb) = if missing.len() > 1 {
            (missing.join(" and "), "were")
        } else {
            (missing[0].clone(), "was")
        };

        if bridge_was_requested {
            self.engine.stop();
            self.engine.clear_error();
            self.bridge_signature = 0;
            self.bridge_restart_pending = false;
            self.push_log(format!("{subject} {verb} unplugged — bridge stopped."), RED);
            self.device_alert = Some(format!(
                "{subject} {verb} unplugged, so the bridge was stopped. Reconnect the device, then start the bridge again."
            ));
        } else {
            self.push_log(
                format!("{subject} {verb} unplugged. Choose another device."),
                AMBER,
            );
            self.device_alert = Some(format!(
                "{subject} {verb} unplugged. Choose another device before starting the bridge."
            ));
        }
        true
    }

    /// ASIO outputs on Windows, JACK or direct ALSA hardware on Linux.
    pub(crate) fn is_low_latency_output(&self, device: &DeviceItem) -> bool {
        audio::is_low_latency_device(device, &self.inventory.low_latency_label)
    }

    /// Positions in the output list that belong to one tab.
    pub(crate) fn output_indices(&self, low_latency: bool) -> Vec<usize> {
        self.inventory
            .outputs
            .iter()
            .enumerate()
            .filter(|(_, device)| self.is_low_latency_output(device) == low_latency)
            .map(|(index, _)| index)
            .collect()
    }
}
