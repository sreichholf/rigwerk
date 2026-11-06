//! One PipeWire node for a PipeWire → PipeWire bridge.
//!
//! When both the input and the output are PipeWire nodes they already share
//! one graph clock: PipeWire itself keeps any second device in step with the
//! graph driver.  Two separate streams joined by RigWerk's ring would add a
//! cushion and drift steering that have nothing to correct, and because the
//! two streams are not linked to each other the graph may run playback before
//! capture in a cycle, which costs one more quantum.
//!
//! This path is a single `pw_filter` node, "RigWerk", with two DSP input
//! ports and two DSP output ports.  One realtime callback reads the input
//! ports, runs the mixer chain on that block and writes the output ports in
//! the same graph cycle.  There is no ring, no cushion, no priming, no drift
//! steering and no rate conversion: DSP ports always run at the graph rate.
//!
//! A filter is not placed by the session manager the way a stream with
//! `target.object` is, so RigWerk links its own ports.  It watches the
//! registry for the chosen source and sink by `node.name` and creates links
//! through `link-factory` whenever the ports it wants are present and not yet
//! linked.  A device that disappears and comes back (a profile switch, a USB
//! re-plug) is linked again as soon as its new ports show up.
//!
//! The delay past RigWerk's output (filters, Bluetooth, the device) is left
//! unknown on this path.  A stream reports it through `pw_stream_get_time`;
//! a filter's `pw_filter_get_time` reported 0 here, and PipeWire 1.0 did not
//! pass the ports' Latency params to the filter, so there is nothing honest
//! to show.
//!
//! The safe `pipewire` crate has no filter wrapper, so the filter itself goes
//! through `pipewire-sys`.  Everything else (core, registry, links) uses the
//! safe API.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ffi::{c_void, CString};
use std::ptr::{self, NonNull};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ::pipewire as pw;
use pw::spa;
use pw::sys as pw_sys;
use spa::sys as spa_sys;

use crate::audio::denormal::DenormalGuard;
use crate::audio::mixer::OutputMixer;
use crate::audio::output_map::MAX_FOLLOWERS;
use crate::audio::shared::Shared;
use crate::audio::types::StartConfig;

use super::native::{node_latency_frames, node_name};

/// The node RigWerk publishes for the duplex path.
const DUPLEX_NODE: &str = "RigWerk";
/// The largest graph quantum the scratch is sized for.  PipeWire's own
/// `clock.max-quantum` default is 8192.
const MAX_FRAMES: usize = 8192;
/// How often the main loop checks the stop flag, links and the graph rate.
const TICK: Duration = Duration::from_millis(50);
/// How long a requested link may take to appear before it is asked again.
const LINK_GRACE: Duration = Duration::from_secs(1);
/// Input FL/FR, clock output FL/FR, then one stereo pair per follower slot.
const INPUT_PORTS: usize = 2;
const CLOCK_OUT: usize = 2;
const TOTAL_PORTS: usize = INPUT_PORTS + CLOCK_OUT + MAX_FOLLOWERS * 2;

/// Why the duplex path did not run.  `Setup` errors happen before any audio
/// flowed, so the caller can still open the two-stream path instead.
pub(crate) enum DuplexError {
    Setup(String),
    Runtime(String),
}

/// One audio port of another node, as the registry describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PortRef {
    pub(crate) id: u32,
    /// `audio.channel`, e.g. `FL`, `FR`, `MONO`, `AUX0`; empty when unset.
    pub(crate) channel: String,
}

/// Chooses which of a node's ports feed (or take) RigWerk's left and right
/// port.  `FL`/`FR` win; a `MONO` port, or a node with a single port, feeds
/// both sides when `mono_to_both` is set (a mono source) and only the left
/// side otherwise (a mono sink, so the two sides are not summed into it at
/// double level).  Without channel names the first two ports in id order are
/// used.
pub(crate) fn map_ports(ports: &[PortRef], mono_to_both: bool) -> [Option<u32>; 2] {
    let find = |name: &str| {
        ports
            .iter()
            .filter(|port| port.channel.eq_ignore_ascii_case(name))
            .map(|port| port.id)
            .min()
    };
    let left = find("FL");
    let right = find("FR");
    if left.is_some() || right.is_some() {
        return match (left, right) {
            (Some(left), Some(right)) => [Some(left), Some(right)],
            // Only one side is labelled: treat it like a mono port.
            (Some(only), None) | (None, Some(only)) => mono(only, mono_to_both),
            (None, None) => [None, None],
        };
    }
    if let Some(port) = find("MONO") {
        return mono(port, mono_to_both);
    }
    let mut ids: Vec<u32> = ports.iter().map(|port| port.id).collect();
    ids.sort_unstable();
    match ids.as_slice() {
        [] => [None, None],
        [only] => mono(*only, mono_to_both),
        [first, second, ..] => [Some(*first), Some(*second)],
    }
}

fn mono(port: u32, mono_to_both: bool) -> [Option<u32>; 2] {
    if mono_to_both {
        [Some(port), Some(port)]
    } else {
        [Some(port), None]
    }
}

/// What the registry has told us about the graph, kept on the main loop.
#[derive(Default)]
pub(crate) struct Graph {
    /// Node id → (`node.name`, `media.class`).
    nodes: HashMap<u32, (String, String)>,
    /// Port id → (owning node id, direction is output, monitor, channel).
    ports: HashMap<u32, PortInfo>,
    /// Existing links as (output port, input port), by link id.
    links: HashMap<u32, (u32, u32)>,
    /// Set whenever a node, port or link came or went.
    dirty: bool,
}

#[derive(Clone, Debug)]
struct PortInfo {
    node: u32,
    output: bool,
    monitor: bool,
    channel: String,
    name: String,
}

impl Graph {
    fn node_by_name(&self, name: &str) -> Option<u32> {
        self.nodes
            .iter()
            .filter(|(_, node)| node.0 == name)
            .map(|(id, _)| *id)
            .min()
    }

    fn ports_of(&self, node: u32, output: bool, monitor: bool) -> Vec<PortRef> {
        let mut ports: Vec<PortRef> = self
            .ports
            .iter()
            .filter(|(_, port)| {
                port.node == node && port.output == output && port.monitor == monitor
            })
            .map(|(id, port)| PortRef {
                id: *id,
                channel: port.channel.clone(),
            })
            .collect();
        ports.sort_by_key(|port| port.id);
        ports
    }

    /// The output ports a source feeds RigWerk from.  Plain outputs come
    /// first.  A node with only monitor outputs (a virtual source such as
    /// `Audio/Source/Virtual` publishes its capture ports that way) uses
    /// those, unless it is a sink: a sink's monitor is its own playback, and
    /// taking it would feed RigWerk's output back into its input.
    fn source_ports(&self, node: u32) -> Vec<PortRef> {
        let plain = self.ports_of(node, true, false);
        if !plain.is_empty() {
            return plain;
        }
        let is_sink = self
            .nodes
            .get(&node)
            .is_some_and(|(_, class)| class.starts_with("Audio/Sink"));
        if is_sink {
            Vec::new()
        } else {
            self.ports_of(node, true, true)
        }
    }

    /// RigWerk's own port by name (`input_FL`, `output_FR`, …).
    fn own_port(&self, node: u32, name: &str) -> Option<u32> {
        self.ports
            .iter()
            .filter(|(_, port)| port.node == node && port.name == name)
            .map(|(id, _)| *id)
            .min()
    }

    /// The links the bridge needs, as (output node, output port, input node,
    /// input port).  Empty until RigWerk's own ports and the device ports
    /// are all known.  `followers` is `(slot, sink node.name)` for each
    /// hardware follower that should play; virtual slots are omitted.
    pub(crate) fn wanted_links(
        &self,
        own: u32,
        source: &str,
        sink: &str,
        followers: &[(usize, &str)],
    ) -> Vec<(u32, u32, u32, u32)> {
        let mut wanted = Vec::new();
        if let Some(source_node) = self.node_by_name(source) {
            let from = map_ports(&self.source_ports(source_node), true);
            for (side, name) in ["input_FL", "input_FR"].iter().enumerate() {
                if let (Some(out_port), Some(in_port)) = (from[side], self.own_port(own, name)) {
                    wanted.push((source_node, out_port, own, in_port));
                }
            }
        }
        self.link_sink(&mut wanted, own, sink, ["output_FL", "output_FR"]);
        for &(slot, name) in followers {
            if slot >= MAX_FOLLOWERS || name.is_empty() {
                continue;
            }
            let ports = [format!("follow_{slot}_FL"), format!("follow_{slot}_FR")];
            self.link_sink(
                &mut wanted,
                own,
                name,
                [ports[0].as_str(), ports[1].as_str()],
            );
        }
        wanted
    }

    fn link_sink(
        &self,
        wanted: &mut Vec<(u32, u32, u32, u32)>,
        own: u32,
        sink: &str,
        port_names: [&str; 2],
    ) {
        let Some(sink_node) = self.node_by_name(sink) else {
            return;
        };
        let to = map_ports(&self.ports_of(sink_node, false, false), false);
        for (side, name) in port_names.iter().enumerate() {
            if let (Some(out_port), Some(in_port)) = (self.own_port(own, name), to[side]) {
                wanted.push((own, out_port, sink_node, in_port));
            }
        }
    }

    fn has_link(&self, output_port: u32, input_port: u32) -> bool {
        self.links
            .values()
            .any(|link| *link == (output_port, input_port))
    }

    fn add_global(
        &mut self,
        id: u32,
        kind: &pw::types::ObjectType,
        props: &spa::utils::dict::DictRef,
    ) {
        match kind {
            pw::types::ObjectType::Node => {
                if let Some(name) = props.get("node.name") {
                    let class = props.get("media.class").unwrap_or("").to_owned();
                    self.nodes.insert(id, (name.to_owned(), class));
                    self.dirty = true;
                }
            }
            pw::types::ObjectType::Port => {
                let node = props.get("node.id").and_then(|text| text.parse().ok());
                let Some(node) = node else {
                    return;
                };
                self.ports.insert(
                    id,
                    PortInfo {
                        node,
                        output: props.get("port.direction") == Some("out"),
                        monitor: props.get("port.monitor") == Some("true"),
                        channel: props.get("audio.channel").unwrap_or("").to_owned(),
                        name: props.get("port.name").unwrap_or("").to_owned(),
                    },
                );
                self.dirty = true;
            }
            pw::types::ObjectType::Link => {
                let output = props
                    .get("link.output.port")
                    .and_then(|text| text.parse().ok());
                let input = props
                    .get("link.input.port")
                    .and_then(|text| text.parse().ok());
                if let (Some(output), Some(input)) = (output, input) {
                    self.links.insert(id, (output, input));
                    self.dirty = true;
                }
            }
            _ => {}
        }
    }

    fn remove_global(&mut self, id: u32) {
        let removed = self.nodes.remove(&id).is_some()
            | self.ports.remove(&id).is_some()
            | self.links.remove(&id).is_some();
        if removed {
            self.dirty = true;
        }
    }
}

/// State the realtime callback owns.  The main loop only reads the atomics
/// it shares through `Arc`s; the struct itself lives in a `Box` that the
/// filter's listener points at and is freed only after the filter is gone.
struct ProcessData {
    shared: Arc<Shared>,
    mixer: OutputMixer,
    /// Input FL/FR, clock output FL/FR, then `follow_{i}_FL/FR`.
    ports: [*mut c_void; TOTAL_PORTS],
    scratch_l: Vec<f32>,
    scratch_r: Vec<f32>,
    rate: u32,
    /// The graph rate and quantum the callback last ran with, for the main
    /// loop's status text.
    seen_rate: Arc<AtomicU32>,
    seen_quantum: Arc<AtomicU64>,
}

/// What `state_changed` needs.  It arrives on the main loop while the
/// realtime callback may hold `ProcessData`, so it gets its own box and its
/// own listener instead of sharing that one.
struct StateData {
    shared: Arc<Shared>,
    error: Arc<AtomicBool>,
}

unsafe extern "C" fn on_process(data: *mut c_void, position: *mut spa_sys::spa_io_position) {
    // SAFETY: `data` is the `ProcessData` box registered with the listener,
    // alive until after `pw_filter_destroy`; only this callback touches it
    // while the filter runs.
    let data = unsafe { &mut *(data as *mut ProcessData) };
    if position.is_null() {
        return;
    }
    // Flush-to-zero for the whole cycle, as in the other audio callbacks;
    // `render_direct` re-asserts it around the chain.
    let _denormals = DenormalGuard::new();
    // SAFETY: PipeWire passes a valid position for this cycle.
    let clock = unsafe { &(*position).clock };
    let frames = clock.duration as usize;
    if frames == 0 {
        return;
    }
    let rate = clock.rate.denom;
    if rate > 0 && rate != data.rate {
        data.rate = rate;
        data.mixer.set_direct_rate(rate);
        data.shared.note_direct_rate(rate);
        data.seen_rate.store(rate, Ordering::Relaxed);
    }
    data.seen_quantum.store(frames as u64, Ordering::Relaxed);

    let buffer = |port: *mut c_void| unsafe {
        pw_sys::pw_filter_get_dsp_buffer(port, frames as u32) as *mut f32
    };
    let ProcessData {
        shared,
        mixer,
        ports,
        scratch_l,
        scratch_r,
        ..
    } = data;
    let shared: &Shared = shared;
    let in_l = buffer(ports[0]);
    let in_r = buffer(ports[1]);
    // Clock + every preallocated follower pair; unlinked ports stay null.
    let mut out_bufs = [ptr::null_mut::<f32>(); CLOCK_OUT + MAX_FOLLOWERS * 2];
    for (slot, out) in out_bufs.iter_mut().enumerate() {
        *out = buffer(ports[INPUT_PORTS + slot]);
    }

    let has_input = !in_l.is_null() || !in_r.is_null();
    if has_input {
        // An unlinked input is not counted as delivered, so a source that
        // has gone away trips the engine's input heartbeat as before.
        shared.note_input_block(frames);
        shared.note_input_frames(frames);
    }
    shared
        .output_step_frames
        .store(frames.min(u32::MAX as usize) as u32, Ordering::Relaxed);
    let gain = shared.input_gain();
    let enabled_l = shared.input_enabled[0].load(Ordering::Relaxed);
    let enabled_r = shared.input_enabled[1].load(Ordering::Relaxed);
    let mut peak_l = 0.0f32;
    let mut peak_r = 0.0f32;
    // A cycle longer than the scratch runs in scratch-sized pieces, so every
    // output sample of the cycle is written.
    let mut offset = 0;
    while offset < frames {
        let count = (frames - offset).min(MAX_FRAMES);
        for index in 0..count {
            let at = offset + index;
            // SAFETY: a non-null DSP buffer holds `frames` samples.  A
            // missing side copies the other one, as a mono source would.
            let raw_l = (!in_l.is_null()).then(|| unsafe { *in_l.add(at) });
            let raw_r = (!in_r.is_null()).then(|| unsafe { *in_r.add(at) });
            let (raw_l, raw_r) = match (raw_l, raw_r) {
                (Some(left), Some(right)) => (left, right),
                (Some(only), None) | (None, Some(only)) => (only, only),
                (None, None) => (0.0, 0.0),
            };
            let left = if enabled_l { raw_l * gain } else { 0.0 };
            let right = if enabled_r { raw_r * gain } else { 0.0 };
            peak_l = peak_l.max(left.abs());
            peak_r = peak_r.max(right.abs());
            shared.push_scope_in(left, right);
            scratch_l[index] = left;
            scratch_r[index] = right;
        }
        mixer.render_direct_devices(
            shared,
            &scratch_l[..count],
            &scratch_r[..count],
            |device, index, mapped| {
                let at = offset + index;
                let base = device * 2;
                if base + 1 >= out_bufs.len() {
                    return;
                }
                let out_l = out_bufs[base];
                let out_r = out_bufs[base + 1];
                // SAFETY: a non-null DSP output buffer holds `frames` samples.
                if !out_l.is_null() {
                    unsafe { *out_l.add(at) = mapped.sample(0) };
                }
                if !out_r.is_null() {
                    unsafe { *out_r.add(at) = mapped.sample(1) };
                }
            },
        );
        offset += count;
    }
    shared.in_l.store(peak_l.to_bits(), Ordering::Relaxed);
    shared.in_r.store(peak_r.to_bits(), Ordering::Relaxed);
}

unsafe extern "C" fn on_state_changed(
    data: *mut c_void,
    _old: pw_sys::pw_filter_state,
    state: pw_sys::pw_filter_state,
    error: *const std::os::raw::c_char,
) {
    if state != pw_sys::pw_filter_state_PW_FILTER_STATE_ERROR {
        return;
    }
    // SAFETY: `data` is the `StateData` box of the main-loop listener, alive
    // until after `pw_filter_destroy`; nothing else holds it mutably.
    let data = unsafe { &*(data as *const StateData) };
    let message = if error.is_null() {
        "unknown error".to_owned()
    } else {
        // SAFETY: PipeWire passes a NUL-terminated message.
        unsafe { std::ffi::CStr::from_ptr(error) }
            .to_string_lossy()
            .into_owned()
    };
    data.error.store(true, Ordering::Relaxed);
    data.shared
        .set_device_lost(format!("PipeWire duplex node: {message}"));
}

/// The `pw_filter` with its ports, listener and callback state.  Dropping it
/// destroys the filter first (which stops the realtime callback), then frees
/// what the callback used.
struct Filter {
    raw: NonNull<pw_sys::pw_filter>,
    _hooks: [Box<spa_sys::spa_hook>; 2],
    _events: [Box<pw_sys::pw_filter_events>; 2],
    _data: Box<ProcessData>,
    state: Box<StateData>,
}

impl Drop for Filter {
    fn drop(&mut self) {
        // SAFETY: `raw` came from `pw_filter_new` and is destroyed once.
        // Destroying it removes the node from the data loop before returning.
        unsafe { pw_sys::pw_filter_destroy(self.raw.as_ptr()) };
    }
}

fn properties(pairs: &[(&str, &str)]) -> pw::properties::PropertiesBox {
    let mut props = pw::properties::PropertiesBox::new();
    for (key, value) in pairs {
        props.insert(*key, *value);
    }
    props
}

impl Filter {
    fn new(
        core: &pw::core::CoreRc,
        latency: &str,
        rate: u32,
        shared: Arc<Shared>,
        observed: &Observed,
    ) -> Result<Self, String> {
        let rate_hint = format!("1/{}", rate.max(1));
        let props = properties(&[
            ("media.type", "Audio"),
            ("media.category", "Filter"),
            ("media.role", "DSP"),
            ("node.name", DUPLEX_NODE),
            ("node.description", "RigWerk"),
            ("node.latency", latency),
            // A hint, not a force: the graph switches only when its allowed
            // rates and the other clients permit it.
            ("node.rate", &rate_hint),
            // RigWerk links this node itself.
            ("node.autoconnect", "false"),
        ]);
        let name = CString::new(DUPLEX_NODE).expect("node name has no NUL");
        // SAFETY: the core outlives the filter (`run` drops the filter
        // first); `pw_filter_new` takes ownership of `props`.
        let raw =
            unsafe { pw_sys::pw_filter_new(core.as_raw_ptr(), name.as_ptr(), props.into_raw()) };
        let raw = NonNull::new(raw).ok_or_else(|| "pw_filter_new failed".to_owned())?;

        // Clock device is stereo; followers get their own stereo port pairs.
        let mut mixer = OutputMixer::new(rate, 2);
        mixer.prepare(MAX_FRAMES);
        mixer.publish_device_channels(&shared);
        mixer.publish_follower_channels(&shared);
        let mut state = Box::new(StateData {
            shared: Arc::clone(&shared),
            error: Arc::clone(&observed.error),
        });
        let mut data = Box::new(ProcessData {
            shared,
            mixer,
            ports: [ptr::null_mut(); TOTAL_PORTS],
            scratch_l: vec![0.0; MAX_FRAMES],
            scratch_r: vec![0.0; MAX_FRAMES],
            // Unknown until the first cycle, which then publishes the real
            // graph rate.
            rate: 0,
            seen_rate: Arc::clone(&observed.rate),
            seen_quantum: Arc::clone(&observed.quantum),
        });
        let events = |state_changed, process| {
            Box::new(pw_sys::pw_filter_events {
                version: pw_sys::PW_VERSION_FILTER_EVENTS,
                destroy: None,
                state_changed,
                io_changed: None,
                param_changed: None,
                add_buffer: None,
                remove_buffer: None,
                process,
                drained: None,
                command: None,
            })
        };
        let mut process_events = events(None, Some(on_process as _));
        let mut state_events = events(Some(on_state_changed as _), None);
        // SAFETY: an all-zero `spa_hook` is the unlinked state PipeWire
        // expects before `add_listener`.
        let mut hooks: [Box<spa_sys::spa_hook>; 2] = [
            Box::new(unsafe { std::mem::zeroed() }),
            Box::new(unsafe { std::mem::zeroed() }),
        ];

        let mut port_names: Vec<(String, &'static str, u32)> = vec![
            ("input_FL".to_owned(), "FL", spa_sys::SPA_DIRECTION_INPUT),
            ("input_FR".to_owned(), "FR", spa_sys::SPA_DIRECTION_INPUT),
            ("output_FL".to_owned(), "FL", spa_sys::SPA_DIRECTION_OUTPUT),
            ("output_FR".to_owned(), "FR", spa_sys::SPA_DIRECTION_OUTPUT),
        ];
        for slot in 0..MAX_FOLLOWERS {
            port_names.push((
                format!("follow_{slot}_FL"),
                "FL",
                spa_sys::SPA_DIRECTION_OUTPUT,
            ));
            port_names.push((
                format!("follow_{slot}_FR"),
                "FR",
                spa_sys::SPA_DIRECTION_OUTPUT,
            ));
        }
        for (index, (port_name, channel, direction)) in port_names.iter().enumerate() {
            let props = properties(&[
                ("format.dsp", "32 bit float mono audio"),
                ("port.name", port_name),
                ("audio.channel", channel),
            ]);
            // SAFETY: `raw` is a live filter; the call takes `props`.
            let port = unsafe {
                pw_sys::pw_filter_add_port(
                    raw.as_ptr(),
                    *direction,
                    pw_sys::pw_filter_port_flags_PW_FILTER_PORT_FLAG_MAP_BUFFERS,
                    std::mem::size_of::<u64>(),
                    props.into_raw(),
                    ptr::null_mut(),
                    0,
                )
            };
            if port.is_null() {
                // SAFETY: nothing else refers to the filter yet.
                unsafe { pw_sys::pw_filter_destroy(raw.as_ptr()) };
                return Err(format!("could not add port {port_name}"));
            }
            data.ports[index] = port;
        }

        // SAFETY: the hook, event and data boxes stay at fixed addresses and
        // live in `Filter` until after `pw_filter_destroy`.  `process` runs on
        // the data thread and only sees `ProcessData`; `state_changed` runs
        // on the main loop and only sees `StateData`.
        unsafe {
            pw_sys::pw_filter_add_listener(
                raw.as_ptr(),
                hooks[0].as_mut(),
                process_events.as_mut(),
                data.as_mut() as *mut ProcessData as *mut c_void,
            );
            pw_sys::pw_filter_add_listener(
                raw.as_ptr(),
                hooks[1].as_mut(),
                state_events.as_mut(),
                state.as_mut() as *mut StateData as *mut c_void,
            );
        }
        // From here on `Filter`'s Drop destroys `raw` on every error path.
        let filter = Self {
            raw,
            _hooks: hooks,
            _events: [process_events, state_events],
            _data: data,
            state,
        };
        // SAFETY: ports and listener are in place.
        let result = unsafe {
            pw_sys::pw_filter_connect(
                filter.raw.as_ptr(),
                pw_sys::pw_filter_flags_PW_FILTER_FLAG_RT_PROCESS,
                ptr::null_mut(),
                0,
            )
        };
        if result < 0 {
            return Err(format!(
                "pw_filter_connect failed: {}",
                std::io::Error::from_raw_os_error(-result)
            ));
        }
        Ok(filter)
    }

    /// The filter's node id once the server has bound it.
    fn node_id(&self) -> Option<u32> {
        // SAFETY: `raw` is live for `self`'s lifetime.
        let id = unsafe { pw_sys::pw_filter_get_node_id(self.raw.as_ptr()) };
        (id != spa_sys::SPA_ID_INVALID).then_some(id)
    }

    fn shared(&self) -> &Shared {
        &self.state.shared
    }
}

/// A link RigWerk asked `link-factory` for.  Dropping the proxy before the
/// link appears abandons it; once it exists it lives as long as this client.
struct MadeLink {
    link: pw::link::Link,
    asked: std::time::Instant,
    /// Whether the link has shown up in the registry.
    seen: bool,
}

/// Values the realtime and main-loop callbacks report to the run loop.
struct Observed {
    rate: Arc<AtomicU32>,
    quantum: Arc<AtomicU64>,
    error: Arc<AtomicBool>,
}

/// Runs the duplex node until `stop`.  A `Setup` error means nothing was
/// opened and the two-stream path can still be tried.
pub(crate) fn run(
    config: &StartConfig,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
) -> Result<(), DuplexError> {
    let setup = DuplexError::Setup;
    let source = node_name(config.input_id.as_deref().unwrap_or(""))
        .ok_or_else(|| setup("Choose a PipeWire input".to_owned()))?
        .to_owned();
    let sink = node_name(&config.output_id)
        .ok_or_else(|| setup("Choose a PipeWire output".to_owned()))?
        .to_owned();
    let rate = config.output_rate.unwrap_or(config.sample_rate).max(1);
    // One node, one quantum: the smaller of the two buffer requests.
    let frames = node_latency_frames(
        config.pipewire_low_latency,
        config.buffer_size.min(config.output_buffer),
    );
    let latency = format!("{frames}/{rate}");

    let mainloop =
        pw::main_loop::MainLoopRc::new(None).map_err(|error| setup(error.to_string()))?;
    let context =
        pw::context::ContextRc::new(&mainloop, None).map_err(|error| setup(error.to_string()))?;
    let core = context
        .connect_rc(None)
        .map_err(|error| setup(error.to_string()))?;
    let registry = core
        .get_registry_rc()
        .map_err(|error| setup(error.to_string()))?;

    let graph = Rc::new(RefCell::new(Graph::default()));
    let added = Rc::clone(&graph);
    let removed = Rc::clone(&graph);
    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            if let Some(props) = global.props.as_ref() {
                added
                    .borrow_mut()
                    .add_global(global.id, &global.type_, props);
            }
        })
        .global_remove(move |id| removed.borrow_mut().remove_global(id))
        .register();

    let observed = Observed {
        rate: Arc::new(AtomicU32::new(0)),
        quantum: Arc::new(AtomicU64::new(frames as u64)),
        error: Arc::new(AtomicBool::new(false)),
    };
    let filter =
        Rc::new(Filter::new(&core, &latency, rate, Arc::clone(&shared), &observed).map_err(setup)?);
    // Only now: a fallback must not start from this path's rates.  The
    // callback publishes the real graph rate on its first cycle, and the
    // timer below re-publishes it, so this guess cannot stick.
    shared.note_direct_rate(rate);
    shared.direct.store(true, Ordering::Relaxed);
    let low_latency = config.pipewire_low_latency;
    let mode = move |rate: u32, quantum: u64| {
        let low = if low_latency { "Low latency · " } else { "" };
        format!("{low}PipeWire duplex node · {quantum} frames at {rate} Hz · no bridge buffer")
    };
    shared.set_output(rate, frames, mode(rate, frames as u64));
    shared.running.store(true, Ordering::Relaxed);
    shared.set_status(format!(
        "Bridging 2 ch · {rate} Hz · one PipeWire node (no bridge buffer)"
    ));

    // Errors from the server (a link the factory refused, for one) show up
    // in the status line while the bridge runs.
    let error_shared = Arc::clone(&shared);
    let _core_listener = core
        .add_listener_local()
        .error(move |_id, _seq, _res, message| {
            error_shared.set_status(format!("PipeWire: {message}"));
        })
        .register();

    // Links RigWerk asked for, by (output port, input port).
    let made: Rc<RefCell<HashMap<(u32, u32), MadeLink>>> = Rc::default();
    let shown = std::cell::Cell::new((rate, frames as u64));
    let filter_ref = Rc::clone(&filter);

    let stop_flag = Arc::clone(&stop);
    let loop_quit = mainloop.clone();
    let tick_graph = Rc::clone(&graph);
    let tick_made = Rc::clone(&made);
    let tick_core = core.clone();
    let seen_rate = Arc::clone(&observed.rate);
    let seen_quantum = Arc::clone(&observed.quantum);
    let error = Arc::clone(&observed.error);
    let timer = mainloop.loop_().add_timer(move |_| {
        if stop_flag.load(Ordering::Relaxed) || error.load(Ordering::Relaxed) {
            loop_quit.quit();
            return;
        }
        let shared = filter_ref.shared();
        // The mode text follows the graph rate and quantum; the callback
        // only stores them.
        let rate = seen_rate.load(Ordering::Relaxed);
        let quantum = seen_quantum.load(Ordering::Relaxed);
        if rate > 0 && (rate, quantum) != shown.get() {
            shown.set((rate, quantum));
            shared.note_direct_rate(rate);
            shared.set_output(rate, quantum as u32, mode(rate, quantum));
        }
        let Some(own) = filter_ref.node_id() else {
            return;
        };
        let mut graph = tick_graph.borrow_mut();
        let mut made = tick_made.borrow_mut();
        // Follower add/remove is live: rebuild links whenever the plan moves.
        let plan = shared.follower_plan();
        let follower_sinks: Vec<(usize, &str)> = plan
            .slots
            .iter()
            .take(plan.count)
            .enumerate()
            .filter(|(_, slot)| !slot.virtual_out && !slot.node_name.is_empty())
            .map(|(index, slot)| (index, slot.node_name.as_str()))
            .collect();
        // Nothing changed and no link is still waiting to appear.
        if !graph.dirty && made.values().all(|link| link.seen) {
            // Still re-check when the follower list may have changed: compare
            // against what we would want now.
            let wanted = graph.wanted_links(own, &source, &sink, &follower_sinks);
            let wanted_ports: HashSet<(u32, u32)> =
                wanted.iter().map(|link| (link.1, link.3)).collect();
            if made.len() == wanted_ports.len()
                && made.keys().all(|ports| wanted_ports.contains(ports))
            {
                return;
            }
        }
        graph.dirty = false;
        let wanted = graph.wanted_links(own, &source, &sink, &follower_sinks);
        let wanted_ports: HashSet<(u32, u32)> =
            wanted.iter().map(|link| (link.1, link.3)).collect();
        // Links that are no longer wanted (a device came back with other
        // ports) are removed, on the server too if they still exist.
        let stale: Vec<(u32, u32)> = made
            .keys()
            .filter(|ports| !wanted_ports.contains(ports))
            .copied()
            .collect();
        for ports in stale {
            if let Some(old) = made.remove(&ports) {
                if graph.has_link(ports.0, ports.1) {
                    let _ = tick_core.destroy_object(old.link);
                }
            }
        }
        for (out_node, out_port, in_node, in_port) in wanted {
            let ports = (out_port, in_port);
            let present = graph.has_link(out_port, in_port);
            match made.get_mut(&ports) {
                Some(link) if present => {
                    link.seen = true;
                    continue;
                }
                // Still on its way to the registry.
                Some(link) if !link.seen && link.asked.elapsed() < LINK_GRACE => continue,
                // Removed (by hand, or by the server) or never appeared:
                // ask again.
                Some(_) => {
                    made.remove(&ports);
                }
                // Someone else already linked these ports.
                None if present => continue,
                None => {}
            }
            let props = properties(&[
                ("link.output.node", &out_node.to_string()),
                ("link.output.port", &out_port.to_string()),
                ("link.input.node", &in_node.to_string()),
                ("link.input.port", &in_port.to_string()),
                ("object.linger", "false"),
            ]);
            match tick_core.create_object::<pw::link::Link>("link-factory", &props) {
                Ok(link) => {
                    made.insert(
                        ports,
                        MadeLink {
                            link,
                            asked: std::time::Instant::now(),
                            seen: false,
                        },
                    );
                }
                Err(error) => shared.set_status(format!("Linking RigWerk: {error}")),
            }
        }
    });
    let started = timer.update_timer(Some(TICK), Some(TICK)).into_result();
    if let Err(error) = started {
        shared.running.store(false, Ordering::Relaxed);
        shared.direct.store(false, Ordering::Relaxed);
        return Err(DuplexError::Runtime(format!(
            "PipeWire duplex timer: {error}"
        )));
    }
    mainloop.run();
    drop(timer);
    // Links first, then the node, then the connection.
    made.borrow_mut().clear();
    drop(filter);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{map_ports, Graph, PortRef};

    fn port(id: u32, channel: &str) -> PortRef {
        PortRef {
            id,
            channel: channel.to_owned(),
        }
    }

    #[test]
    fn stereo_ports_map_by_channel_name() {
        let ports = [port(12, "FR"), port(11, "FL"), port(13, "RL")];
        assert_eq!(map_ports(&ports, true), [Some(11), Some(12)]);
        assert_eq!(map_ports(&ports, false), [Some(11), Some(12)]);
    }

    #[test]
    fn a_mono_source_feeds_both_sides_and_a_mono_sink_takes_the_left() {
        let ports = [port(40, "MONO")];
        assert_eq!(map_ports(&ports, true), [Some(40), Some(40)]);
        assert_eq!(map_ports(&ports, false), [Some(40), None]);
        let unnamed = [port(7, "")];
        assert_eq!(map_ports(&unnamed, true), [Some(7), Some(7)]);
    }

    #[test]
    fn unnamed_ports_use_the_first_two_by_id() {
        let ports = [port(9, "AUX1"), port(5, "AUX0"), port(30, "AUX2")];
        assert_eq!(map_ports(&ports, true), [Some(5), Some(9)]);
        assert_eq!(map_ports(&[], true), [None, None]);
    }

    #[test]
    fn a_single_labelled_side_counts_as_mono() {
        assert_eq!(map_ports(&[port(3, "FL")], true), [Some(3), Some(3)]);
        assert_eq!(map_ports(&[port(3, "FR")], false), [Some(3), None]);
    }

    fn dict(pairs: &[(&str, &str)]) -> pipewire::properties::PropertiesBox {
        super::properties(pairs)
    }

    fn add_node(graph: &mut Graph, id: u32, name: &str, class: &str) {
        graph.add_global(
            id,
            &pipewire::types::ObjectType::Node,
            dict(&[("node.name", name), ("media.class", class)]).dict(),
        );
    }

    fn add_port(graph: &mut Graph, id: u32, node: u32, output: bool, name: &str, monitor: bool) {
        let node = node.to_string();
        let channel = &name[name.len() - 2..];
        graph.add_global(
            id,
            &pipewire::types::ObjectType::Port,
            dict(&[
                ("node.id", &node),
                ("port.direction", if output { "out" } else { "in" }),
                ("audio.channel", channel),
                ("port.name", name),
                ("port.monitor", if monitor { "true" } else { "false" }),
            ])
            .dict(),
        );
    }

    /// RigWerk (node 3, ports 30-33) and a stereo sink (node 2, ports 20/21
    /// with monitors 22/23).
    fn rigwerk_and_sink() -> Graph {
        let mut graph = Graph::default();
        add_node(&mut graph, 2, "speakers", "Audio/Sink");
        add_port(&mut graph, 20, 2, false, "playback_FL", false);
        add_port(&mut graph, 21, 2, false, "playback_FR", false);
        add_port(&mut graph, 22, 2, true, "monitor_FL", true);
        add_port(&mut graph, 23, 2, true, "monitor_FR", true);
        add_node(&mut graph, 3, "RigWerk", "");
        add_port(&mut graph, 30, 3, false, "input_FL", false);
        add_port(&mut graph, 31, 3, false, "input_FR", false);
        add_port(&mut graph, 32, 3, true, "output_FL", false);
        add_port(&mut graph, 33, 3, true, "output_FR", false);
        graph
    }

    #[test]
    fn a_virtual_source_with_only_monitor_ports_is_linked() {
        let mut graph = rigwerk_and_sink();
        add_node(&mut graph, 1, "vsrc", "Audio/Source/Virtual");
        add_port(&mut graph, 10, 1, true, "capture_FL", true);
        add_port(&mut graph, 11, 1, true, "capture_FR", true);
        add_port(&mut graph, 12, 1, false, "input_FL", false);
        add_port(&mut graph, 13, 1, false, "input_FR", false);
        assert_eq!(
            graph.wanted_links(3, "vsrc", "speakers", &[]),
            vec![
                (1, 10, 3, 30),
                (1, 11, 3, 31),
                (3, 32, 2, 20),
                (3, 33, 2, 21)
            ]
        );
    }

    #[test]
    fn a_sink_is_never_used_as_a_source_through_its_monitor() {
        let mut graph = rigwerk_and_sink();
        add_node(&mut graph, 4, "other_sink", "Audio/Sink");
        add_port(&mut graph, 40, 4, false, "playback_FL", false);
        add_port(&mut graph, 41, 4, true, "monitor_FL", true);
        add_port(&mut graph, 42, 4, true, "monitor_FR", true);
        assert_eq!(
            graph.wanted_links(3, "other_sink", "speakers", &[]),
            vec![(3, 32, 2, 20), (3, 33, 2, 21)]
        );
        // Plain outputs win over monitor outputs on a source.
        add_node(&mut graph, 5, "mixed", "Audio/Source");
        add_port(&mut graph, 50, 5, true, "monitor_FL", true);
        add_port(&mut graph, 51, 5, true, "capture_FL", false);
        add_port(&mut graph, 52, 5, true, "capture_FR", false);
        assert_eq!(
            graph.wanted_links(3, "mixed", "speakers", &[])[..2],
            [(5, 51, 3, 30), (5, 52, 3, 31)]
        );
    }

    #[test]
    fn wanted_links_wire_source_to_rigwerk_to_sink_and_skip_monitors() {
        use pipewire::types::ObjectType;
        let mut graph = Graph::default();
        let node = |graph: &mut Graph, id, name| {
            graph.add_global(id, &ObjectType::Node, dict(&[("node.name", name)]).dict());
        };
        let port = |graph: &mut Graph,
                    id: u32,
                    node: u32,
                    dir: &str,
                    ch: &str,
                    name: &str,
                    monitor: bool| {
            let node = node.to_string();
            graph.add_global(
                id,
                &ObjectType::Port,
                dict(&[
                    ("node.id", &node),
                    ("port.direction", dir),
                    ("audio.channel", ch),
                    ("port.name", name),
                    ("port.monitor", if monitor { "true" } else { "false" }),
                ])
                .dict(),
            );
        };
        node(&mut graph, 1, "gp5");
        node(&mut graph, 2, "speakers");
        node(&mut graph, 3, "RigWerk");
        port(&mut graph, 10, 1, "out", "FL", "capture_FL", false);
        port(&mut graph, 11, 1, "out", "FR", "capture_FR", false);
        port(&mut graph, 20, 2, "in", "FL", "playback_FL", false);
        port(&mut graph, 21, 2, "in", "FR", "playback_FR", false);
        port(&mut graph, 22, 2, "out", "FL", "monitor_FL", true);
        port(&mut graph, 23, 2, "out", "FR", "monitor_FR", true);
        port(&mut graph, 30, 3, "in", "FL", "input_FL", false);
        port(&mut graph, 31, 3, "in", "FR", "input_FR", false);
        port(&mut graph, 32, 3, "out", "FL", "output_FL", false);
        port(&mut graph, 33, 3, "out", "FR", "output_FR", false);
        assert!(graph.dirty);
        assert_eq!(
            graph.wanted_links(3, "gp5", "speakers", &[]),
            vec![
                (1, 10, 3, 30),
                (1, 11, 3, 31),
                (3, 32, 2, 20),
                (3, 33, 2, 21)
            ]
        );

        // An existing link is seen; removing the source drops its links.
        graph.add_global(
            50,
            &ObjectType::Link,
            dict(&[("link.output.port", "10"), ("link.input.port", "30")]).dict(),
        );
        assert!(graph.has_link(10, 30));
        graph.dirty = false;
        graph.remove_global(1);
        graph.remove_global(10);
        graph.remove_global(11);
        assert!(graph.dirty);
        assert_eq!(
            graph.wanted_links(3, "gp5", "speakers", &[]),
            vec![(3, 32, 2, 20), (3, 33, 2, 21)]
        );
    }

    #[test]
    fn follower_sinks_link_from_preallocated_port_pairs() {
        let mut graph = rigwerk_and_sink();
        add_port(&mut graph, 34, 3, true, "follow_0_FL", false);
        add_port(&mut graph, 35, 3, true, "follow_0_FR", false);
        add_node(&mut graph, 6, "phones", "Audio/Sink");
        add_port(&mut graph, 60, 6, false, "playback_FL", false);
        add_port(&mut graph, 61, 6, false, "playback_FR", false);
        let links = graph.wanted_links(3, "missing", "speakers", &[(0, "phones")]);
        assert!(links.contains(&(3, 32, 2, 20)));
        assert!(links.contains(&(3, 33, 2, 21)));
        assert!(links.contains(&(3, 34, 6, 60)));
        assert!(links.contains(&(3, 35, 6, 61)));
        // A missing follower stays in the session: no link, bridge keeps going.
        let only_clock = graph.wanted_links(3, "missing", "speakers", &[(0, "gone")]);
        assert_eq!(only_clock, vec![(3, 32, 2, 20), (3, 33, 2, 21)]);
    }

    /// Runs the real bridge entry point against a live PipeWire daemon.
    /// Set `RIGWERK_PW_SOURCE` and `RIGWERK_PW_SINK` to node names (any node
    /// with audio output ports works as the source, e.g. a `pw-cat` tone)
    /// and run with `--ignored`.
    #[test]
    #[ignore = "needs a running PipeWire daemon and test nodes"]
    fn duplex_bridge_runs_against_a_live_daemon() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let (Ok(source), Ok(sink)) = (
            std::env::var("RIGWERK_PW_SOURCE"),
            std::env::var("RIGWERK_PW_SINK"),
        ) else {
            panic!("set RIGWERK_PW_SOURCE and RIGWERK_PW_SINK");
        };
        let shared = Arc::new(crate::audio::shared::Shared::new(48_000));
        shared.reset();
        let stop = Arc::new(AtomicBool::new(false));
        let config = crate::audio::types::StartConfig {
            asio_clsid: None,
            input_id: Some(format!("pipewire:{source}")),
            output_id: format!("pipewire:{sink}"),
            output_asio_clsid: None,
            sample_rate: 48_000,
            buffer_size: 128,
            output_rate: None,
            output_buffer: 128,
            asio_wrapper: false,
            real_style_output: false,
            pipewire_low_latency: false,
        };
        let thread = {
            let shared = Arc::clone(&shared);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || crate::audio::linux::native::run(&config, shared, stop))
        };
        let seconds: u64 = std::env::var("RIGWERK_PW_SECONDS")
            .ok()
            .and_then(|text| text.parse().ok())
            .unwrap_or(3);
        let mut peak_in = 0.0f32;
        let mut peak_out = 0.0f32;
        for _ in 0..seconds * 10 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            let snapshot = shared.snapshot();
            peak_in = peak_in.max(snapshot.in_l);
            peak_out = peak_out.max(snapshot.out_l);
        }
        let snapshot = shared.snapshot();
        stop.store(true, Ordering::Relaxed);
        let result = thread.join().expect("bridge thread");
        eprintln!(
            "status={:?} mode={:?} error={:?} frames={} input_frames={} rate={} step={} \
             peak_in={peak_in} peak_out={peak_out} direct={} underruns={} path_delay={:?}",
            snapshot.status,
            snapshot.output_mode,
            snapshot.error,
            snapshot.frames,
            snapshot.input_frames,
            snapshot.output_rate,
            snapshot.output_step_frames,
            snapshot.direct,
            snapshot.underruns,
            snapshot.path_delay_ms,
        );
        assert!(result.is_ok(), "{result:?}");
        assert!(snapshot.direct, "the duplex node should be used");
        assert!(snapshot.frames > 0, "the node should have processed blocks");
        assert!(snapshot.input_frames > 0, "the source should be linked");
        assert!(peak_in > 0.0 && peak_out > 0.0, "audio should flow through");
        assert_eq!(snapshot.underruns, 0);
    }
}
