//! The LV2 port table, the control ranges and the port buffers.
//!
//! The table is read once from the bundle's parsed Turtle
//! (`scan::read_bundle` is shared with the scan, so manifest and
//! `rdfs:seeAlso` handling exists once) and never changes afterwards: both
//! halves of an instance read the same immutable ranges, exactly as the CLAP
//! host shares its range table.
//!
//! A port is identified by its `lv2:index`.  Indices must be unique but need
//! not be contiguous: a bundle with a gap is accepted, the buffer slot is the
//! port's position in the table and `PortSlot::port` keeps the declared
//! index for `connect_port`.  Audio ports are mono, so the main pair is the
//! first up to two audio inputs and the first up to two outputs in index
//! order that are not `lv2:isSideChain`; side chains, further audio inputs
//! and outputs, CV ports and atom ports get buffers of their own and are
//! otherwise ignored, so an auxiliary input only ever hears silence.  Control inputs become the
//! writable parameters, control outputs the read-only views, and a control
//! output with `lv2:latency`/`lv2:reportsLatency` is the latency report.  A
//! `pprops:notOnGUI` control is not a parameter, but its buffer still starts
//! at its `lv2:default`, like every control input's: a hidden "enable" that
//! defaults to 1 must not run at 0.
//!
//! An atom port is connected to an `atom:Sequence`, the only buffer type
//! this host supplies: a port that lists only other `atom:bufferType`s is a
//! load error, or connected to NULL when it is `lv2:connectionOptional`.
//! Its buffer holds at least [`ATOM_CAPACITY`] bytes of events, more when
//! the port's `rsz:minimumSize` asks for it.  Before every `run` an input is
//! an empty sequence and an output an `atom:Chunk` whose size is the space
//! the plugin may write, as the atom spec requires.
//!
//! [`PortBuffers`] owns one buffer per declared port, sized at load.  Pointers
//! handed to `connect_port` are the addresses of those buffers, which never
//! move, so connecting once after `instantiate` and before the first `run` is
//! enough; nothing here allocates after construction.

use std::ffi::c_void;
use std::path::Path;
use std::sync::Arc;

use lv2_sys::{LV2_Atom_Sequence, LV2_Atom_Sequence_Body, LV2_Descriptor, LV2_Handle};

use crate::audio::plugins::{ParamIds, PluginParamView, MAX_AUDIO_PORTS};

use super::host::SUPPORTED;
use super::scan::{
    blocked_reason, has_property, missing_options, read_bundle, unsupported_atom_type,
};
use super::ttl::TtlStore;
use super::urid;

const LV2_INPUT_PORT: &str = "http://lv2plug.in/ns/lv2core#InputPort";
const LV2_OUTPUT_PORT: &str = "http://lv2plug.in/ns/lv2core#OutputPort";
const LV2_AUDIO_PORT: &str = "http://lv2plug.in/ns/lv2core#AudioPort";
const LV2_CONTROL_PORT: &str = "http://lv2plug.in/ns/lv2core#ControlPort";
const LV2_CV_PORT: &str = "http://lv2plug.in/ns/lv2core#CVPort";
const ATOM_ATOM_PORT: &str = "http://lv2plug.in/ns/ext/atom#AtomPort";

const LV2_PORT: &str = "http://lv2plug.in/ns/lv2core#port";
const LV2_INDEX: &str = "http://lv2plug.in/ns/lv2core#index";
const LV2_SYMBOL: &str = "http://lv2plug.in/ns/lv2core#symbol";
const LV2_NAME: &str = "http://lv2plug.in/ns/lv2core#name";
const LV2_DEFAULT: &str = "http://lv2plug.in/ns/lv2core#default";
const LV2_MINIMUM: &str = "http://lv2plug.in/ns/lv2core#minimum";
const LV2_MAXIMUM: &str = "http://lv2plug.in/ns/lv2core#maximum";
const LV2_SAMPLE_RATE: &str = "http://lv2plug.in/ns/lv2core#sampleRate";
const LV2_INTEGER: &str = "http://lv2plug.in/ns/lv2core#integer";
const LV2_ENUMERATION: &str = "http://lv2plug.in/ns/lv2core#enumeration";
const LV2_TOGGLED: &str = "http://lv2plug.in/ns/lv2core#toggled";
const LV2_SCALE_POINT: &str = "http://lv2plug.in/ns/lv2core#scalePoint";
const LV2_DESIGNATION: &str = "http://lv2plug.in/ns/lv2core#designation";
const LV2_CONNECTION_OPTIONAL: &str = "http://lv2plug.in/ns/lv2core#connectionOptional";
const LV2_IS_SIDE_CHAIN: &str = "http://lv2plug.in/ns/lv2core#isSideChain";
const LV2_LATENCY: &str = "http://lv2plug.in/ns/lv2core#latency";
const LV2_REPORTS_LATENCY: &str = "http://lv2plug.in/ns/lv2core#reportsLatency";
const LV2_REQUIRED_FEATURE: &str = "http://lv2plug.in/ns/lv2core#requiredFeature";

const RDF_VALUE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#value";
const RDFS_LABEL: &str = "http://www.w3.org/2000/01/rdf-schema#label";

const PPROPS_LOGARITHMIC: &str = "http://lv2plug.in/ns/ext/port-props#logarithmic";
const PPROPS_NOT_ON_GUI: &str = "http://lv2plug.in/ns/ext/port-props#notOnGUI";

const RSZ_MINIMUM_SIZE: &str = "http://lv2plug.in/ns/ext/resize-port#minimumSize";

const UNITS_UNIT: &str = "http://lv2plug.in/ns/extensions/units#unit";
const UNITS_SYMBOL: &str = "http://lv2plug.in/ns/extensions/units#symbol";

/// The type of an atom input, which is cleared to an empty sequence.
const ATOM_SEQUENCE: &std::ffi::CStr = c"http://lv2plug.in/ns/ext/atom#Sequence";

/// The type of an atom output before `run`: space the plugin may fill.
const ATOM_CHUNK: &std::ffi::CStr = c"http://lv2plug.in/ns/ext/atom#Chunk";

/// The event bytes every atom port buffer can hold at least, sequence header
/// excluded.  A port's `rsz:minimumSize` can only raise it.
pub(super) const ATOM_CAPACITY: u32 = 8192;

/// The largest `rsz:minimumSize` a port may ask for.  Far above any effect's
/// event buffer; the cap only keeps a broken description from sizing a
/// buffer without bound.
const MAX_ATOM_BYTES: u64 = 16 * 1024 * 1024;

/// The most ports one plugin may declare, whatever their kind.  Far above
/// any effect; the cap only keeps a broken description from sizing buffers
/// without bound.
const MAX_PORTS: usize = 512;

/// The most audio ports that form the main pair, one mono port per channel.
const MAIN_CHANNELS: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PortKind {
    Audio,
    Control,
    Atom,
    Cv,
    /// An optional port of a kind this host has no buffer for.  It gets no
    /// connection: `lv2:connectionOptional` says the plugin handles that.
    Other,
}

/// One declared port, in index order.
#[derive(Clone, Debug)]
pub(super) struct PortInfo {
    pub(super) index: u32,
    pub(super) kind: PortKind,
    pub(super) input: bool,
    /// `lv2:isSideChain`: never part of the main pair.
    pub(super) side_chain: bool,
    /// An atom port's event capacity in bytes, sequence header excluded:
    /// [`ATOM_CAPACITY`] or its `rsz:minimumSize`, whichever is larger.  0
    /// for any other port.
    pub(super) capacity: u32,
    /// A control input's `lv2:default`, clamped and scaled like its range;
    /// what its buffer holds until a value is set.  0 for any other port.
    pub(super) default: f32,
}

/// One `lv2:scalePoint`: the value the port reports and the label to show.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct ScalePoint {
    pub(super) value: f64,
    pub(super) label: String,
}

/// One control port's range, normalisation and display text.  Immutable after
/// load, so the processor and the UI both read it without a lock.  Bounds
/// already carry any `lv2:sampleRate` scaling.
#[derive(Clone, Debug)]
pub(super) struct Lv2Range {
    /// [`Self::view`]'s id: the 32-bit FNV-1a of the port symbol.
    pub(super) id: u32,
    /// The port's buffer slot in [`PortBuffers`]; the control buffer to read
    /// or write.
    pub(super) slot: usize,
    pub(super) name: String,
    pub(super) min: f64,
    pub(super) max: f64,
    pub(super) default: f64,
    /// Integer, enumeration and toggled controls snap to whole values.
    pub(super) stepped: bool,
    pub(super) logarithmic: bool,
    pub(super) read_only: bool,
    /// 0 for a continuous control, otherwise the number of steps.
    pub(super) step_count: i32,
    pub(super) unit: String,
    pub(super) points: Vec<ScalePoint>,
}

impl Lv2Range {
    pub(super) fn normalise(&self, native: f64) -> f64 {
        if !native.is_finite() {
            return 0.0;
        }
        let span = self.max - self.min;
        if !span.is_finite() || span <= 0.0 {
            return 0.0;
        }
        let value = if self.logarithmic && self.min > 0.0 && native > 0.0 {
            (native / self.min).ln() / (self.max / self.min).ln()
        } else {
            (native - self.min) / span
        };
        value.clamp(0.0, 1.0)
    }

    pub(super) fn denormalise(&self, value: f64) -> f64 {
        let value = if value.is_finite() {
            value.clamp(0.0, 1.0)
        } else {
            0.0
        };
        // The loader orders the bounds, but a hand-built range must not make
        // `clamp` panic (`min > max`) or see a NaN.
        if !self.min.is_finite() || !self.max.is_finite() || self.max <= self.min {
            return if self.min.is_finite() { self.min } else { 0.0 };
        }
        let native = if self.logarithmic && self.min > 0.0 {
            self.min * (self.max / self.min).powf(value)
        } else {
            self.min + value * (self.max - self.min)
        };
        if self.stepped {
            native.round().clamp(self.min, self.max)
        } else {
            native.clamp(self.min, self.max)
        }
    }

    /// The nearest scale-point label for a stepped control, else the value
    /// with its unit.  An integer or toggle without a unit shows no
    /// decimals; a continuous control with points still shows its value.
    pub(super) fn display(&self, native: f64) -> String {
        let native = if native.is_finite() {
            native
        } else {
            self.default
        };
        if self.stepped {
            if let Some(point) = self.nearest_point(native) {
                return point.label.clone();
            }
        }
        let decimals = if self.stepped && self.unit.is_empty() {
            0
        } else {
            2
        };
        if self.unit.is_empty() {
            format!("{native:.decimals$}")
        } else {
            format!("{native:.decimals$} {}", self.unit)
        }
    }

    fn nearest_point(&self, native: f64) -> Option<&ScalePoint> {
        self.points.iter().min_by(|a, b| {
            (a.value - native)
                .abs()
                .total_cmp(&(b.value - native).abs())
        })
    }

    pub(super) fn view(&self, native: f64) -> PluginParamView {
        PluginParamView {
            id: self.id,
            name: self.name.clone(),
            value: self.normalise(native),
            default: self.normalise(self.default),
            step_count: self.step_count,
            unit: self.unit.clone(),
            display: self.display(native),
            read_only: self.read_only,
        }
    }
}

/// Everything an instance needs from a bundle's description.
pub(super) struct PortTable {
    /// Every declared port, in index order.  Its position is also the port's
    /// buffer slot.
    pub(super) ports: Vec<PortInfo>,
    /// The writable control inputs, then the read-only control outputs.
    pub(super) ranges: Vec<Lv2Range>,
    pub(super) ids: Arc<ParamIds>,
    /// The buffer slot of the latency report, if there is one.
    pub(super) latency: Option<usize>,
}

impl PortTable {
    /// Parses the bundle and builds the table for `plugin_uri`.  The loader
    /// parses once and calls [`Self::read_store`] for both the table and the
    /// binary; this is the form the tests and any caller with only a bundle
    /// use.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn read(bundle: &Path, plugin_uri: &str, sample_rate: u32) -> Result<Self, String> {
        let (store, plugins) = read_bundle(bundle)?;
        Self::read_store(&store, &plugins, plugin_uri, sample_rate)
    }

    /// Builds the table from an already parsed bundle, so the loader parses
    /// the Turtle once for both the table and the binary.
    pub(super) fn read_store(
        store: &TtlStore,
        plugins: &[String],
        plugin_uri: &str,
        sample_rate: u32,
    ) -> Result<Self, String> {
        if !plugins.iter().any(|plugin| plugin == plugin_uri) {
            return Err(format!("The LV2 bundle does not declare {plugin_uri}."));
        }
        // Defense in depth: the scan already lists a plugin with an
        // unsupported required feature with an error, but the loader must not
        // instantiate one it cannot serve.
        for feature in store.iris(plugin_uri, LV2_REQUIRED_FEATURE) {
            if !SUPPORTED.contains(&feature.as_str()) {
                return Err(blocked_reason(&feature));
            }
        }
        if let Some(missing) = missing_options(store, plugin_uri).into_iter().next() {
            return Err(missing);
        }

        let (mut ports, nodes) = read_ports(store, plugin_uri)?;
        if ports.len() > MAX_PORTS {
            return Err(format!(
                "This LV2 plugin declares more than {MAX_PORTS} ports."
            ));
        }
        // The CLAP host's limit is per direction: the main pair only ever
        // consumes two, but a bundle with dozens of inputs or outputs is not
        // an effect this chain can wire.
        let audio_inputs = ports
            .iter()
            .filter(|port| port.kind == PortKind::Audio && port.input)
            .count();
        let audio_outputs = ports
            .iter()
            .filter(|port| port.kind == PortKind::Audio && !port.input)
            .count();
        if audio_inputs > MAX_AUDIO_PORTS || audio_outputs > MAX_AUDIO_PORTS {
            return Err(format!(
                "This LV2 plugin declares more than {MAX_AUDIO_PORTS} audio inputs or outputs."
            ));
        }

        let mut latency = None;
        for (slot, (port, node)) in ports.iter().zip(&nodes).enumerate() {
            if port.kind != PortKind::Control || port.input {
                continue;
            }
            if is_latency_port(store, node) {
                latency = Some(slot);
            }
        }

        // The writable inputs first, then the read-only outputs, each in
        // index order.  A latency or `notOnGUI` control is not a parameter,
        // but a hidden input still gets its default.
        let mut ranges = Vec::new();
        let mut symbols = Vec::new();
        let mut indexes = Vec::new();
        let mut defaults = Vec::new();
        for input in [true, false] {
            for (slot, (port, node)) in ports.iter().zip(&nodes).enumerate() {
                if port.kind != PortKind::Control || port.input != input {
                    continue;
                }
                if Some(slot) == latency {
                    continue;
                }
                let range = read_range(store, node, slot, !input, sample_rate)?;
                if input {
                    defaults.push((slot, range.default as f32));
                }
                if has_property(store, node, PPROPS_NOT_ON_GUI) {
                    continue;
                }
                ranges.push(range);
                symbols.push(store.literal(node, LV2_SYMBOL).unwrap_or_default());
                indexes.push(port.index);
            }
        }
        for (slot, default) in defaults {
            ports[slot].default = default;
        }

        assign_ids(plugin_uri, &mut ranges, &symbols, &indexes);
        let ids = Arc::new(ParamIds::new(ranges.iter().map(|range| range.id).collect()));
        Ok(Self {
            ports,
            ranges,
            ids,
            latency,
        })
    }

    /// The largest atom port buffer in bytes, header included: what the
    /// `buf-size:sequenceSize` option advertises.  Without an atom port it is
    /// the size a default buffer would have.
    pub(super) fn sequence_size(&self) -> u32 {
        let capacity = self
            .ports
            .iter()
            .filter(|port| port.kind == PortKind::Atom)
            .map(|port| port.capacity)
            .max()
            .unwrap_or(ATOM_CAPACITY);
        std::mem::size_of::<LV2_Atom_Sequence>() as u32 + capacity
    }
}

/// Gives every range its stable id: the 32-bit FNV-1a of the port symbol.  A
/// symbol that hashes to 0 becomes 1.  Two symbols that hash alike (the same
/// symbol on two ports) fall back to the port's declared `lv2:index` for the
/// later one, with one log line; if even that is taken, the next free id is
/// used.
fn assign_ids(plugin_uri: &str, ranges: &mut [Lv2Range], symbols: &[String], indexes: &[u32]) {
    let mut used: Vec<u32> = Vec::with_capacity(ranges.len());
    for ((range, symbol), index) in ranges.iter_mut().zip(symbols).zip(indexes) {
        let mut id = symbol_id(symbol);
        if id == 0 {
            id = 1;
        }
        if used.contains(&id) {
            let fallback = (*index).max(1);
            eprintln!(
                "[plugins] two LV2 ports of {plugin_uri} hash to {id}; using the port index {fallback} for {symbol:?}"
            );
            id = fallback;
            while used.contains(&id) {
                id = id.wrapping_add(1).max(1);
            }
        }
        used.push(id);
        range.id = id;
    }
}

/// 32-bit FNV-1a.  Stable across builds, unlike `DefaultHasher`.
pub(super) fn symbol_id(symbol: &str) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in symbol.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Reads and validates every `lv2:port` the plugin declares, in index order,
/// with each port's subject node in the same order.
fn read_ports(store: &TtlStore, plugin_uri: &str) -> Result<(Vec<PortInfo>, Vec<String>), String> {
    let mut ports = Vec::new();
    for node in store.iris(plugin_uri, LV2_PORT) {
        let types = store.types(&node);
        let has = |kind: &str| types.iter().any(|declared| declared == kind);
        let symbol = store.literal(&node, LV2_SYMBOL).unwrap_or_default();
        let name = store
            .literal(&node, LV2_NAME)
            .unwrap_or_else(|| symbol.clone());
        let label = if name.trim().is_empty() {
            symbol.clone()
        } else {
            name
        };
        let Some(index) = store
            .literal(&node, LV2_INDEX)
            .and_then(|index| index.trim().parse::<u32>().ok())
        else {
            return Err(format!("The LV2 port {label:?} has no valid lv2:index."));
        };
        let input = has(LV2_INPUT_PORT);
        let output = has(LV2_OUTPUT_PORT);
        if input && output {
            return Err(format!(
                "The LV2 port {label:?} is declared as both an input and an output."
            ));
        }
        if !input && !output {
            return Err(format!(
                "The LV2 port {label:?} is neither an input nor an output."
            ));
        }
        let optional = has_property(store, &node, LV2_CONNECTION_OPTIONAL);
        let kind = if has(LV2_AUDIO_PORT) {
            PortKind::Audio
        } else if has(LV2_CONTROL_PORT) {
            PortKind::Control
        } else if has(ATOM_ATOM_PORT) {
            match unsupported_atom_type(store, &node) {
                None => PortKind::Atom,
                Some(_) if optional => PortKind::Other,
                Some(why) => {
                    return Err(format!(
                        "The LV2 atom port {label:?} {why}; RigWerk only connects atom:Sequence."
                    ));
                }
            }
        } else if has(LV2_CV_PORT) {
            if !optional {
                return Err(format!("This LV2 plugin needs the CV port {label:?}."));
            }
            PortKind::Cv
        } else if optional {
            PortKind::Other
        } else {
            return Err(format!(
                "The LV2 port {label:?} has no port type RigWerk can connect."
            ));
        };
        let capacity = if kind == PortKind::Atom {
            atom_capacity(store, &node, &label)?
        } else {
            0
        };
        ports.push((
            PortInfo {
                index,
                kind,
                input,
                side_chain: has_property(store, &node, LV2_IS_SIDE_CHAIN),
                capacity,
                default: 0.0,
            },
            node,
        ));
    }

    ports.sort_by_key(|(port, _)| port.index);
    if let Some(pair) = ports
        .windows(2)
        .find(|pair| pair[0].0.index == pair[1].0.index)
    {
        return Err(format!(
            "The LV2 plugin declares the port index {} twice.",
            pair[0].0.index
        ));
    }
    // A gap is harmless: the buffer slot is the position in this list and
    // `PortSlot::port` keeps the declared index for `connect_port`.
    let (ports, nodes) = ports.into_iter().unzip();
    Ok((ports, nodes))
}

/// An atom port's event capacity: [`ATOM_CAPACITY`], or the port's
/// `rsz:minimumSize` when that is larger.  The minimum counts the whole
/// buffer, header included, so a capacity of that many bytes after the
/// header always satisfies it.
fn atom_capacity(store: &TtlStore, node: &str, label: &str) -> Result<u32, String> {
    let Some(text) = store.literal(node, RSZ_MINIMUM_SIZE) else {
        return Ok(ATOM_CAPACITY);
    };
    let minimum: u64 = text
        .trim()
        .parse()
        .map_err(|_| format!("The LV2 port {label:?} has an unreadable rsz:minimumSize."))?;
    if minimum > MAX_ATOM_BYTES {
        return Err(format!(
            "The LV2 port {label:?} asks for a buffer of more than {MAX_ATOM_BYTES} bytes."
        ));
    }
    Ok((minimum as u32).max(ATOM_CAPACITY))
}

/// Reads one control port's range.  A stated bound that cannot be read or is
/// not finite is a load error; swapped bounds are put in order and the
/// default is clamped into the range, so the audio thread only ever sees a
/// finite, ordered range.
fn read_range(
    store: &TtlStore,
    node: &str,
    slot: usize,
    read_only: bool,
    sample_rate: u32,
) -> Result<Lv2Range, String> {
    let name = store
        .literal(node, LV2_NAME)
        .or_else(|| store.literal(node, LV2_SYMBOL))
        .unwrap_or_default();
    let scale = if has_property(store, node, LV2_SAMPLE_RATE) {
        f64::from(sample_rate.max(1))
    } else {
        1.0
    };
    let mut min = parsed_bound(store, node, LV2_MINIMUM, scale, &name)?.unwrap_or(0.0);
    let mut max = parsed_bound(store, node, LV2_MAXIMUM, scale, &name)?.unwrap_or(1.0);
    if max < min {
        std::mem::swap(&mut min, &mut max);
    }
    let default = parsed_bound(store, node, LV2_DEFAULT, scale, &name)?
        .map_or(min, |default| default.clamp(min, max));
    let integer = has_property(store, node, LV2_INTEGER);
    let enumeration = has_property(store, node, LV2_ENUMERATION);
    let toggled = has_property(store, node, LV2_TOGGLED);
    let stepped = integer || enumeration || toggled;
    let step_count = if toggled {
        1
    } else if stepped {
        (max - min).round().max(0.0) as i32
    } else {
        0
    };
    Ok(Lv2Range {
        id: 0,
        slot,
        name,
        min,
        max,
        default,
        stepped,
        logarithmic: has_property(store, node, PPROPS_LOGARITHMIC),
        read_only,
        step_count,
        unit: unit_symbol(store, node),
        points: read_points(store, node),
    })
}

/// A `units:unit`'s display symbol: the unit node's own `units:symbol` when
/// the bundle states it, else the conventional symbol for the common units.
/// The unit definitions are not part of a plugin bundle, so a bundle almost
/// never carries `units:symbol` itself.
fn unit_symbol(store: &TtlStore, node: &str) -> String {
    let Some(unit) = store.iri(node, UNITS_UNIT) else {
        return String::new();
    };
    if let Some(symbol) = store
        .literal(&unit, UNITS_SYMBOL)
        .filter(|symbol| !symbol.trim().is_empty())
    {
        return symbol;
    }
    match local_name(&unit) {
        "db" => "dB".to_owned(),
        "hz" => "Hz".to_owned(),
        "pc" => "%".to_owned(),
        other => other.to_owned(),
    }
}

fn read_points(store: &TtlStore, node: &str) -> Vec<ScalePoint> {
    let mut points: Vec<ScalePoint> = store
        .iris(node, LV2_SCALE_POINT)
        .into_iter()
        .filter_map(|point| {
            let value = store.literal(&point, RDF_VALUE)?;
            let value = value
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|value| value.is_finite())?;
            let label = store.literal(&point, RDFS_LABEL).unwrap_or_default();
            Some(ScalePoint { value, label })
        })
        .collect();
    points.sort_by(|a, b| a.value.total_cmp(&b.value));
    points
}

/// One of a control range's bounds, scaled to the sample rate.  A value the
/// bundle states must parse and stay finite; both are load errors, so no NaN
/// or infinity ever reaches the audio math.
fn parsed_bound(
    store: &TtlStore,
    node: &str,
    predicate: &str,
    scale: f64,
    name: &str,
) -> Result<Option<f64>, String> {
    let Some(text) = store.literal(node, predicate) else {
        return Ok(None);
    };
    let value: f64 = text
        .trim()
        .parse()
        .map_err(|_| format!("The LV2 port {name:?} has an unreadable {predicate} value."))?;
    let value = value * scale;
    if !value.is_finite() {
        return Err(format!(
            "The LV2 port {name:?} has a non-finite {predicate} value."
        ));
    }
    Ok(Some(value))
}

/// Whether a control output reports latency, by designation or port property.
fn is_latency_port(store: &TtlStore, node: &str) -> bool {
    store
        .iris(node, LV2_DESIGNATION)
        .iter()
        .any(|designation| designation == LV2_LATENCY)
        || has_property(store, node, LV2_REPORTS_LATENCY)
}

fn local_name(uri: &str) -> &str {
    uri.rsplit(['#', '/']).next().unwrap_or(uri)
}

/// One port's storage.  Pointers into it never move: the buffers are sized
/// at load and the `Vec` is never resized.
#[derive(Debug)]
pub(super) enum Buffer {
    Audio(Vec<f32>),
    Control(Vec<f32>),
    Atom(AtomBuffer),
    Cv(Vec<f32>),
}

impl Buffer {
    fn data(&mut self) -> *mut c_void {
        match self {
            Buffer::Audio(samples) | Buffer::Control(samples) | Buffer::Cv(samples) => {
                samples.as_mut_ptr().cast()
            }
            Buffer::Atom(atom) => atom.sequence().cast(),
        }
    }
}

/// One atom port's storage: the `LV2_Atom_Sequence` header followed by the
/// port's capacity, in one aligned allocation that never moves.
#[derive(Debug)]
pub(super) struct AtomBuffer {
    /// 8-byte aligned, so the header and every aligned atom event inside are
    /// naturally aligned.  Never resized.
    storage: Vec<u64>,
    capacity: u32,
}

impl AtomBuffer {
    fn new(capacity: u32) -> Self {
        let words = (std::mem::size_of::<LV2_Atom_Sequence>() + capacity as usize).div_ceil(8);
        Self {
            storage: vec![0; words],
            capacity,
        }
    }

    /// For the tests: the whole buffer's size in bytes, header included.
    #[cfg(test)]
    pub(super) fn bytes(&self) -> usize {
        self.storage.len() * 8
    }

    /// The sequence header, at the start of the storage.
    pub(super) fn sequence(&mut self) -> *mut LV2_Atom_Sequence {
        self.storage.as_mut_ptr().cast()
    }

    /// The bytes after the `LV2_Atom` header: the sequence body header and
    /// the events.  What an output's `atom:Chunk` offers the plugin.
    fn body_space(&self) -> u32 {
        std::mem::size_of::<LV2_Atom_Sequence_Body>() as u32 + self.capacity
    }

    /// An input is an empty `atom:Sequence` in frames, as
    /// `lv2_atom_sequence_clear` leaves it: its size is the body header's.
    /// An output is an `atom:Chunk` sized to the space after its header, so
    /// the plugin knows how much it may write.
    fn reset(&mut self, types: AtomTypes, input: bool) {
        let space = self.body_space();
        // SAFETY: the storage starts with a live sequence header followed by
        // `capacity` bytes, and `&mut self` means no block is using it.
        unsafe {
            let sequence = &mut *self.sequence();
            if input {
                sequence.atom.size = std::mem::size_of::<LV2_Atom_Sequence_Body>() as u32;
                sequence.atom.type_ = types.sequence;
                sequence.body.unit = 0;
                sequence.body.pad = 0;
            } else {
                sequence.atom.size = space;
                sequence.atom.type_ = types.chunk;
            }
        }
    }
}

/// One declared port's slot.
pub(super) struct PortSlot {
    /// The port index the plugin knows it by, for `connect_port`.
    pub(super) port: u32,
    pub(super) input: bool,
    /// Whether `connect_port` is called for it.  False for an optional port
    /// of a kind this host has no buffer for.
    pub(super) connect: bool,
    pub(super) buffer: Buffer,
}

/// Every buffer an instance owns, in port index order.  The default is the
/// empty set a processor leaves behind when it hands its buffers to the
/// core.  The buffer slot of a
/// port is its position here, which is also its `lv2:index`.
#[derive(Default)]
pub(super) struct PortBuffers {
    pub(super) slots: Vec<PortSlot>,
    /// The buffer slots of the first up to two audio inputs and outputs that
    /// are not side chains, in index order.
    pub(super) audio_inputs: Vec<usize>,
    pub(super) audio_outputs: Vec<usize>,
    /// The buffer slot of the latency report.
    pub(super) latency: Option<usize>,
    /// The atom types the headers are re-armed to, mapped once at load so
    /// the audio thread only stores them.
    types: AtomTypes,
}

/// `atom:Sequence` and `atom:Chunk`, as URIDs.
#[derive(Clone, Copy, Default)]
struct AtomTypes {
    sequence: u32,
    chunk: u32,
}

impl PortBuffers {
    pub(super) fn new(table: &PortTable, block: usize) -> Self {
        let block = block.max(1);
        let mut slots = Vec::with_capacity(table.ports.len());
        let mut audio_inputs = Vec::new();
        let mut audio_outputs = Vec::new();
        for (slot, port) in table.ports.iter().enumerate() {
            let buffer = match port.kind {
                PortKind::Audio => {
                    // A side chain is auxiliary: silent in, ignored out.
                    if !port.side_chain {
                        if port.input {
                            audio_inputs.push(slot);
                        } else {
                            audio_outputs.push(slot);
                        }
                    }
                    Buffer::Audio(vec![0.0; block])
                }
                // Every control input starts at its default, a hidden one
                // included; the load then writes the restored values.
                PortKind::Control => Buffer::Control(vec![port.default; 1]),
                PortKind::Cv => Buffer::Cv(vec![0.0; block]),
                PortKind::Atom => Buffer::Atom(AtomBuffer::new(port.capacity)),
                // Not connected: `lv2:connectionOptional` says the plugin
                // handles a missing pointer.  The one-float slot only keeps
                // positions equal to the port order; it is never read.
                PortKind::Other => Buffer::Cv(vec![0.0; 1]),
            };
            slots.push(PortSlot {
                port: port.index,
                input: port.input,
                connect: port.kind != PortKind::Other,
                buffer,
            });
        }
        audio_inputs.truncate(MAIN_CHANNELS);
        audio_outputs.truncate(MAIN_CHANNELS);
        Self {
            slots,
            audio_inputs,
            audio_outputs,
            latency: table.latency,
            types: AtomTypes {
                sequence: urid::global().map(ATOM_SEQUENCE),
                chunk: urid::global().map(ATOM_CHUNK),
            },
        }
    }

    /// Hands every declared port's buffer to `connect_port`, except an
    /// optional port of a kind this host has no buffer for.  UI thread, at
    /// load, when no block is inside the plugin.
    pub(super) fn connect(&mut self, handle: LV2_Handle, descriptor: *const LV2_Descriptor) {
        // SAFETY: the loader passes a live descriptor from the module, which
        // the core keeps loaded for as long as the instance lives.
        let Some(connect) =
            (unsafe { descriptor.as_ref() }).and_then(|descriptor| descriptor.connect_port)
        else {
            return;
        };
        for slot in &mut self.slots {
            if !slot.connect {
                continue;
            }
            let data = slot.buffer.data();
            // SAFETY: `slot.port` is a port the bundle declares, and `data`
            // points at storage sized for its type.
            unsafe { connect(handle, slot.port, data) };
        }
    }

    /// Clears every audio and CV buffer for `frames`.  Auxiliary inputs and
    /// outputs past the main pair stay silent; the plugin may write into any
    /// output.
    pub(super) fn silence(&mut self, frames: usize) {
        for slot in &mut self.slots {
            match &mut slot.buffer {
                Buffer::Audio(samples) | Buffer::Cv(samples) => {
                    let end = frames.min(samples.len());
                    samples[..end].fill(0.0);
                }
                Buffer::Control(_) | Buffer::Atom(_) => {}
            }
        }
    }

    /// Copies the signal into the main audio inputs: one input gets the mono
    /// sum, two get left and right.  The other inputs were silenced.
    pub(super) fn copy_input(&mut self, left: &[f32], right: &[f32], frames: usize) {
        let Some(first) = self.audio_inputs.first().copied() else {
            return;
        };
        match self.audio_inputs.get(1).copied() {
            Some(second) => {
                if let Some(samples) = self.samples_mut(first) {
                    let frames = frames.min(samples.len()).min(left.len());
                    samples[..frames].copy_from_slice(&left[..frames]);
                }
                if let Some(samples) = self.samples_mut(second) {
                    let frames = frames.min(samples.len()).min(right.len());
                    samples[..frames].copy_from_slice(&right[..frames]);
                }
            }
            None => {
                let frames = frames.min(left.len()).min(right.len());
                if let Some(samples) = self.samples_mut(first) {
                    let frames = frames.min(samples.len());
                    for sample in 0..frames {
                        samples[sample] = 0.5 * (left[sample] + right[sample]);
                    }
                }
            }
        }
    }

    /// Copies the main audio outputs back: one output goes to both sides,
    /// two carry left and right.  The other outputs are ignored.
    pub(super) fn copy_output(&mut self, left: &mut [f32], right: &mut [f32], frames: usize) {
        let Some(first) = self.audio_outputs.first().copied() else {
            return;
        };
        match self.audio_outputs.get(1).copied() {
            Some(second) => {
                if let Some(samples) = self.samples_mut(first) {
                    let frames = frames.min(samples.len()).min(left.len());
                    left[..frames].copy_from_slice(&samples[..frames]);
                }
                if let Some(samples) = self.samples_mut(second) {
                    let frames = frames.min(samples.len()).min(right.len());
                    right[..frames].copy_from_slice(&samples[..frames]);
                }
            }
            None => {
                let frames = frames.min(left.len()).min(right.len());
                if let Some(samples) = self.samples_mut(first) {
                    let frames = frames.min(samples.len());
                    left[..frames].copy_from_slice(&samples[..frames]);
                    right[..frames].copy_from_slice(&samples[..frames]);
                }
            }
        }
    }

    /// Re-arms every atom port for a block: empty input sequences, output
    /// chunks offering their whole space.
    pub(super) fn reset_atoms(&mut self) {
        let types = self.types;
        for slot in &mut self.slots {
            if let Buffer::Atom(atom) = &mut slot.buffer {
                atom.reset(types, slot.input);
            }
        }
    }

    /// The value of a control buffer.
    pub(super) fn control(&self, slot: usize) -> f32 {
        match self.slots.get(slot).map(|slot| &slot.buffer) {
            Some(Buffer::Control(samples)) => samples.first().copied().unwrap_or(0.0),
            _ => 0.0,
        }
    }

    /// Writes a control buffer.
    pub(super) fn set_control(&mut self, slot: usize, value: f32) {
        if let Some(Buffer::Control(samples)) =
            self.slots.get_mut(slot).map(|slot| &mut slot.buffer)
        {
            if let Some(sample) = samples.first_mut() {
                *sample = value;
            }
        }
    }

    /// The latency report, in frames; 0 without a latency port.
    pub(super) fn latency(&self) -> f32 {
        self.latency.map_or(0.0, |slot| self.control(slot))
    }

    fn samples_mut(&mut self, slot: usize) -> Option<&mut [f32]> {
        match &mut self.slots.get_mut(slot)?.buffer {
            Buffer::Audio(samples) | Buffer::Cv(samples) => Some(samples),
            Buffer::Control(_) | Buffer::Atom(_) => None,
        }
    }

    /// For the tests: a port's atom sequence header, if it has one.
    #[cfg(test)]
    pub(super) fn atom(&mut self, slot: usize) -> Option<*mut LV2_Atom_Sequence> {
        match &mut self.slots.get_mut(slot)?.buffer {
            Buffer::Atom(atom) => Some(atom.sequence()),
            _ => None,
        }
    }
}
