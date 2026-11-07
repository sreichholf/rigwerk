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

/// How many scans must miss a selected device before it counts as unplugged.
const UNPLUG_SCANS: u8 = 2;

/// Whether a sound-card uevent arrived since the last look.
///
/// The watch is process-wide and blocks in its own thread. A machine without
/// a netlink socket simply never wakes this, and the timed poll still runs.
fn hotplug_pending() -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::sync::{Mutex, OnceLock};
        static WATCH: OnceLock<Mutex<mpsc::Receiver<()>>> = OnceLock::new();
        let watch = WATCH.get_or_init(|| Mutex::new(audio::hotplug::spawn()));
        let Ok(receiver) = watch.lock() else {
            return false;
        };
        let mut pending = false;
        while receiver.try_recv().is_ok() {
            pending = true;
        }
        pending
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

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

pub(crate) fn device_present(devices: &[DeviceItem], id: &str, backend: &str, name: &str) -> bool {
    restore_device(devices, id, backend, name).index().is_some()
}

pub(crate) fn device_index(
    devices: &[DeviceItem],
    id: &str,
    backend: &str,
    name: &str,
) -> Option<usize> {
    restore_device(devices, id, backend, name).index()
}

/// Which input list a preset row was found in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InputGroup {
    /// `Inventory::asio`: ASIO, PipeWire, JACK, or direct ALSA.
    LowLatency,
    /// `Inventory::inputs`.
    System,
}

/// A preset input located in one of the two input lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct InputRestore {
    pub(crate) group: InputGroup,
    pub(crate) restore: DeviceRestore,
}

/// How a saved device lined up with a scanned list.
///
/// An id match wins even when another row still carries the old display name.
/// [`Self::saved_identity_unconfirmed`] is what [`held_device`] reads so a
/// later save does not replace a stored identity this result did not find.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeviceRestore {
    /// `DeviceItem::id` matched. A renamed display is ignored.
    ById(usize),
    /// No id match. `saved_id_missing` is set when a non-empty id was stored
    /// and no row had it, so the name may point at a different device.
    ByName {
        index: usize,
        saved_id_missing: bool,
    },
    /// Nothing matched. The caller keeps its own default or current row.
    Missing,
}

impl DeviceRestore {
    pub(crate) fn index(self) -> Option<usize> {
        match self {
            Self::ById(index) | Self::ByName { index, .. } => Some(index),
            Self::Missing => None,
        }
    }

    /// The display name chose the row. Startup and preset apply log this.
    pub(crate) fn matched_by_name(self) -> bool {
        matches!(self, Self::ByName { .. })
    }

    /// True when a stored identity must not be replaced by this result.
    /// A legacy name match (no id in the file, and the name was found) is
    /// false, so the next save can record that id. A stored id that is not
    /// connected, and a complete miss, are true. A miss with nothing stored
    /// is also true; [`held_device`] ignores that when the saved id and name
    /// are both empty. An id match is false.
    pub(crate) fn saved_identity_unconfirmed(self) -> bool {
        match self {
            Self::ById(_) => false,
            Self::ByName {
                saved_id_missing, ..
            } => saved_id_missing,
            Self::Missing => true,
        }
    }
}

/// Finds a saved device. Id and backend come from [`DeviceItem`] (the cpal
/// `DeviceId` string, or `asio:{clsid}` / `asio-out:{clsid}`). The display
/// name is used only when no row has that id, and the result says so.
pub(crate) fn restore_device(
    devices: &[DeviceItem],
    id: &str,
    backend: &str,
    name: &str,
) -> DeviceRestore {
    if let Some(index) = find_id(devices, id, backend) {
        return DeviceRestore::ById(index);
    }
    if let Some(index) = find_name(devices, name, backend) {
        return DeviceRestore::ByName {
            index,
            saved_id_missing: !id.is_empty(),
        };
    }
    DeviceRestore::Missing
}

/// A device identity from the preferences file that startup did not confirm.
///
/// `save` writes this back, and leaves the on-screen row on the fallback,
/// until the person picks that side or a patch finds a device there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HeldDevice {
    pub(crate) name: Option<String>,
    pub(crate) id: String,
    pub(crate) backend: String,
}

/// The identity `save` must keep when `restore` did not confirm a stored device.
///
/// `None` means the live device may be written: an id match, a legacy name
/// match (no id was stored and the name was found), or a miss where the name
/// and the id were both empty. `Some` means a name or an id was stored and
/// this result did not confirm it, including a same-name device standing in
/// for a missing id.
pub(crate) fn held_device(
    restore: DeviceRestore,
    name: Option<String>,
    id: String,
    backend: String,
) -> Option<HeldDevice> {
    let name_stored = name.as_deref().is_some_and(|name| !name.is_empty());
    if restore.saved_identity_unconfirmed() && (name_stored || !id.is_empty()) {
        Some(HeldDevice { name, id, backend })
    } else {
        None
    }
}

/// Finds the active preset input. Id-plus-backend is checked in both lists
/// before a looser id match, and both of those before any name, so an ASIO
/// row that still shows the old name cannot hide a system input whose id
/// matched.
pub(crate) fn restore_input(
    low_latency: &[DeviceItem],
    system: &[DeviceItem],
    id: &str,
    backend: &str,
    name: &str,
) -> Option<InputRestore> {
    let lists = [
        (InputGroup::LowLatency, low_latency),
        (InputGroup::System, system),
    ];
    if let Some((group, index)) = first_id(&lists, id, backend) {
        return Some(InputRestore {
            group,
            restore: DeviceRestore::ById(index),
        });
    }
    if let Some((group, index)) = first_name(&lists, name, backend) {
        return Some(InputRestore {
            group,
            restore: DeviceRestore::ByName {
                index,
                saved_id_missing: !id.is_empty(),
            },
        });
    }
    None
}

/// One line for the activity log when the id was not what selected the row.
/// `None` for an id match, and for a miss when nothing was saved.
pub(crate) fn device_restore_note(
    role: &str,
    label: &str,
    restore: DeviceRestore,
) -> Option<String> {
    let shown = if label.is_empty() { "device" } else { label };
    if restore.matched_by_name() {
        let why = if restore.saved_identity_unconfirmed() {
            "the saved device id was not found"
        } else {
            "no device id was saved"
        };
        return Some(format!("{role} “{shown}” matched by name ({why})."));
    }
    if matches!(restore, DeviceRestore::Missing) && !label.is_empty() {
        return Some(format!("{role} “{shown}” was not found."));
    }
    None
}

/// Id first, and only then an id whose backend string differs. A same-name
/// row is never a substitute while this id is still in the list.
fn find_id(devices: &[DeviceItem], id: &str, backend: &str) -> Option<usize> {
    if id.is_empty() {
        return None;
    }
    strict_id(devices, id, backend).or_else(|| loose_id(devices, id, backend))
}

fn first_id(
    lists: &[(InputGroup, &[DeviceItem])],
    id: &str,
    backend: &str,
) -> Option<(InputGroup, usize)> {
    if id.is_empty() {
        return None;
    }
    first_where(lists, |device| strict_id_row(device, id, backend))
        .or_else(|| first_where(lists, |device| loose_id_row(device, id, backend)))
}

/// Display name, preferring the saved backend when there is one. A saved
/// name that is actually a device id still matches, as older startup did.
fn find_name(devices: &[DeviceItem], name: &str, backend: &str) -> Option<usize> {
    if name.is_empty() {
        return None;
    }
    strict_name(devices, name, backend).or_else(|| loose_name(devices, name))
}

fn first_name(
    lists: &[(InputGroup, &[DeviceItem])],
    name: &str,
    backend: &str,
) -> Option<(InputGroup, usize)> {
    if name.is_empty() {
        return None;
    }
    first_where(lists, |device| strict_name_row(device, name, backend))
        .or_else(|| first_where(lists, |device| loose_name_row(device, name)))
}

fn first_where(
    lists: &[(InputGroup, &[DeviceItem])],
    mut pred: impl FnMut(&DeviceItem) -> bool,
) -> Option<(InputGroup, usize)> {
    for (group, devices) in lists {
        if let Some(index) = devices.iter().position(&mut pred) {
            return Some((*group, index));
        }
    }
    None
}

fn strict_id(devices: &[DeviceItem], id: &str, backend: &str) -> Option<usize> {
    devices
        .iter()
        .position(|device| strict_id_row(device, id, backend))
}

fn loose_id(devices: &[DeviceItem], id: &str, backend: &str) -> Option<usize> {
    if backend.is_empty() {
        return None;
    }
    devices
        .iter()
        .position(|device| loose_id_row(device, id, backend))
}

fn strict_id_row(device: &DeviceItem, id: &str, backend: &str) -> bool {
    device.id == id && (backend.is_empty() || device.backend.eq_ignore_ascii_case(backend))
}

fn loose_id_row(device: &DeviceItem, id: &str, backend: &str) -> bool {
    device.id == id && !device.backend.eq_ignore_ascii_case(backend)
}

fn strict_name(devices: &[DeviceItem], name: &str, backend: &str) -> Option<usize> {
    if backend.is_empty() {
        return None;
    }
    devices
        .iter()
        .position(|device| strict_name_row(device, name, backend))
}

fn loose_name(devices: &[DeviceItem], name: &str) -> Option<usize> {
    devices
        .iter()
        .position(|device| loose_name_row(device, name))
}

fn strict_name_row(device: &DeviceItem, name: &str, backend: &str) -> bool {
    device.name.eq_ignore_ascii_case(name) && device.backend.eq_ignore_ascii_case(backend)
}

fn loose_name_row(device: &DeviceItem, name: &str) -> bool {
    device.name.eq_ignore_ascii_case(name) || device.id == name
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

    /// The person chose this low-latency input, or a patch found it.
    /// Later saves may write that identity. A silent fallback does not call this.
    pub(crate) fn accept_asio_device(&mut self) {
        self.asio_held = None;
    }

    /// The person chose this system input, or a patch found it.
    pub(crate) fn accept_input_device(&mut self) {
        self.input_held = None;
    }

    /// The person chose this output, or a patch found it.
    pub(crate) fn accept_output_device(&mut self) {
        self.output_held = None;
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
            if self.startup.is_some() {
                self.finish_startup_scan(fresh);
            } else {
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
        }
        let hotplug = hotplug_pending();
        if self.device_scan.is_none()
            && (hotplug || self.last_device_scan.elapsed() >= Duration::from_millis(2500))
        {
            self.start_device_scan();
        }
    }

    pub(crate) fn start_device_scan(&mut self) {
        self.last_device_scan = Instant::now();
        let (sender, receiver) = mpsc::channel();
        let previous = self.inventory.clone();
        // Opening a PCM to read its configuration contends with a bridge that
        // already holds that device. While the bridge is up, list what the
        // host still enumerates and leave configuration to the next idle scan.
        let probe = !matches!(
            self.engine.state,
            crate::audio::EngineState::Starting
                | crate::audio::EngineState::Running
                | crate::audio::EngineState::Stopping
        );
        let cold_start = self.startup.is_some();
        let scan = thread::Builder::new()
            .name("rigwerk-scan".to_owned())
            .spawn(move || {
                let fresh = if cold_start {
                    audio::scan_devices()
                } else {
                    audio::scan_devices_from(&previous, probe)
                };
                let _ = sender.send(fresh);
            });
        match scan {
            Ok(_) => self.device_scan = Some(receiver),
            Err(_) => {
                // No spare thread: fall back to scanning here, once.
                let fresh = if cold_start {
                    audio::scan_devices()
                } else {
                    audio::scan_devices_from(&self.inventory, probe)
                };
                if self.startup.is_some() {
                    self.finish_startup_scan(fresh);
                } else {
                    self.apply_inventory(fresh);
                }
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

    /// Keeps a selected device in `fresh` until a second scan also misses it.
    ///
    /// One absence is not enough. A configuration probe fails while the bridge
    /// holds an exclusive device, and a single enumeration can flake. The
    /// counters live on the app so the next scan sees the first miss.
    fn hold_unconfirmed_devices(&mut self, fresh: &mut Inventory) {
        let asio_id = self.selected_asio().id.clone();
        let asio_backend = self.selected_asio().backend.clone();
        let asio_name = self.selected_asio().name.clone();
        let asio_demo = self.selected_asio().demo;
        let input = self.inventory.inputs.get(self.input_idx).cloned();
        let output = self.selected_output().clone();

        let input_demo = if self.using_other_input() {
            input.as_ref().map(|device| device.demo).unwrap_or(true)
        } else {
            asio_demo
        };
        let input_absent = if input_demo {
            false
        } else if self.using_other_input() {
            match &input {
                Some(device) => {
                    !device_present(&fresh.inputs, &device.id, &device.backend, &device.name)
                }
                None => false,
            }
        } else {
            !device_present(&fresh.asio, &asio_id, &asio_backend, &asio_name)
        };
        if input_demo || !input_absent {
            self.input_misses = 0;
        } else {
            self.input_misses = self.input_misses.saturating_add(1);
            if self.input_misses < UNPLUG_SCANS {
                let (id, name) = if self.using_other_input() {
                    match input.as_ref() {
                        Some(device) => (device.id.clone(), device.name.clone()),
                        None => (String::new(), String::new()),
                    }
                } else {
                    (asio_id.clone(), asio_name.clone())
                };
                if let Some(prior) = self
                    .inventory
                    .asio
                    .iter()
                    .chain(self.inventory.inputs.iter())
                    .find(|device| device.id == id)
                    .cloned()
                {
                    if self.using_other_input() {
                        fresh.inputs.push(prior);
                    } else {
                        fresh.asio.push(prior);
                    }
                }
                if self.input_misses == 1 && !name.is_empty() {
                    self.push_log(
                        format!(
                            "{name} missed one device scan. It stays selected until a second scan misses it too."
                        ),
                        AMBER,
                    );
                }
            }
        }

        if output.demo || device_present(&fresh.outputs, &output.id, &output.backend, &output.name)
        {
            self.output_misses = 0;
        } else {
            self.output_misses = self.output_misses.saturating_add(1);
            if self.output_misses < UNPLUG_SCANS {
                if let Some(prior) = self
                    .inventory
                    .outputs
                    .iter()
                    .find(|device| device.id == output.id)
                {
                    fresh.outputs.push(prior.clone());
                }
                if self.output_misses == 1 {
                    self.push_log(
                        format!(
                            "{} missed one device scan. It stays selected until a second scan misses it too.",
                            output.name
                        ),
                        AMBER,
                    );
                }
            }
        }
    }

    /// Writes scan notes that are not already in the activity log.
    ///
    /// The note text is the log line, so a later scan that clears
    /// [`Inventory::notes`] cannot drop the failure from the diagnostics log.
    pub(crate) fn log_scan_notes(&mut self, notes: &[String]) {
        for note in notes {
            if note.is_empty() {
                continue;
            }
            if !self.log.iter().any(|entry| entry.text == *note) {
                self.push_log(note.clone(), AMBER);
            }
        }
    }

    pub(crate) fn apply_inventory(&mut self, mut fresh: Inventory) -> bool {
        let scan_notes = fresh.notes.clone();
        self.hold_unconfirmed_devices(&mut fresh);
        if inventory_fingerprint(&fresh) == self.inventory_fingerprint {
            // Same devices. A scan can still fail, and that note has to be kept.
            if self.inventory.notes != scan_notes {
                self.inventory.notes = scan_notes.clone();
            }
            self.log_scan_notes(&scan_notes);
            return false;
        }

        let bridge_was_requested = self.bridge_signature != 0;
        let asio_id = self.selected_asio().id.clone();
        let asio_backend = self.selected_asio().backend.clone();
        let asio_name = self.selected_asio().name.clone();
        let asio_demo = self.selected_asio().demo;
        let input_id = self
            .inventory
            .inputs
            .get(self.input_idx)
            .map(|device| device.id.clone());
        let input_backend = self
            .inventory
            .inputs
            .get(self.input_idx)
            .map(|device| device.backend.clone());
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
        let output_backend = self.selected_output().backend.clone();
        let output_name = self.selected_output().name.clone();
        let output_demo = self.selected_output().demo;
        let output_asio = self.selected_output().asio_clsid.is_some();

        let input_present = if self.using_other_input() {
            match (&input_id, &input_name) {
                (Some(id), Some(name)) => device_present(
                    &fresh.inputs,
                    id,
                    input_backend.as_deref().unwrap_or(""),
                    name,
                ),
                _ => false,
            }
        } else {
            device_present(&fresh.asio, &asio_id, &asio_backend, &asio_name)
        };
        // An ASIO output is present only when its id is. A same-name system
        // speaker must not keep the bridge running on a different backend.
        let output_present = if output_asio {
            fresh.outputs.iter().any(|device| device.id == output_id)
        } else {
            device_present(&fresh.outputs, &output_id, &output_backend, &output_name)
        };
        let active_input_demo = if self.using_other_input() {
            input_demo
        } else {
            asio_demo
        };

        self.inventory = fresh;
        self.inventory_fingerprint = inventory_fingerprint(&self.inventory);
        crate::app::settings::rematch_windows_virtual_out(
            &mut self.followers,
            &self.inventory.outputs,
        );
        self.asio_idx = device_index(&self.inventory.asio, &asio_id, &asio_backend, &asio_name)
            .unwrap_or_else(|| default_index(&self.inventory.asio));
        self.input_idx = match (&input_id, &input_name) {
            (Some(id), Some(name)) => device_index(
                &self.inventory.inputs,
                id,
                input_backend.as_deref().unwrap_or(""),
                name,
            )
            .unwrap_or(0),
            _ => 0,
        };
        self.output_idx = device_index(
            &self.inventory.outputs,
            &output_id,
            &output_backend,
            &output_name,
        )
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
        if !input_present && !active_input_demo && self.input_misses >= UNPLUG_SCANS {
            missing.push(if self.using_other_input() {
                input_name.clone().unwrap_or_else(|| asio_name.clone())
            } else {
                asio_name.clone()
            });
        }
        if !output_present && !output_demo && self.output_misses >= UNPLUG_SCANS {
            missing.push(output_name.clone());
        }

        if missing.is_empty() {
            self.push_log(
                format!(
                    "Device change applied · {asio_count} low-latency / {output_count} output devices."
                ),
                BLUE,
            );
            self.log_scan_notes(&scan_notes);
            return true;
        }

        // A device disappeared, so an automatic reconnect requested before
        // this scan must not start a bridge on the stale selection.
        self.bridge_restart_pending = false;

        let (subject, verb) = if missing.len() > 1 {
            (missing.join(" and "), "were")
        } else {
            (missing[0].clone(), "was")
        };

        if bridge_was_requested {
            self.engine.stop();
            self.engine.clear_error();
            self.bridge_signature = 0;
            self.push_log(format!("{subject} {verb} unplugged — bridge stopped."), RED);
            self.alert_lost_device(format!(
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
        self.log_scan_notes(&scan_notes);
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

#[cfg(test)]
mod tests;
