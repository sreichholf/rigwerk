//! RigWerk as a PipeWire client.
//!
//! The device menu lists real sources and sinks from the PipeWire registry.
//! Starting the bridge creates our own nodes, "RigWerk Capture" and "RigWerk
//! Playback", and links them to the chosen node by name (`target.object`).
//! Names stay valid when ids change, so a device that comes back is linked
//! again. When both sides are PipeWire nodes, `duplex` replaces the two
//! streams with one node and no ring; the streams remain for mixed bridges
//! and as the fallback. `node.latency` is set on our streams only. The desktop-wide
//! `clock.force-quantum` switch stays in the metadata path.
//!
//! `hw:` and `plughw:` are untouched: they are the exclusive ALSA path. When
//! the server is not running, listing fails and the ALSA `pipewire` device
//! stays in the menu so something can still be opened.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use cpal::traits::StreamTrait;
// The sibling module is also named pipewire, so the crate needs a leading `::`.
use ::pipewire as pw;
use pw::spa;
use pw::spa::pod::Pod;

use crate::audio::denormal::DenormalGuard;
use crate::audio::input::{make_input_stream, open_input_device};
use crate::audio::mixer::OutputMixer;
use crate::audio::output::open_output;
use crate::audio::shared::Shared;
use crate::audio::types::{DeviceItem, StartConfig};

use super::alsa::{alsa_pcm_is_default, alsa_pcm_name};
use super::pipewire::{self, LOW_QUANTUM};

const PREFIX: &str = "pipewire:";
const CAPTURE_NODE: &str = "RigWerk Capture";
const PLAYBACK_NODE: &str = "RigWerk Playback";
/// Front left / front right in `spa_audio_channel`.
const CHANNEL_FL: u32 = 3;
const CHANNEL_FR: u32 = 4;
const PREPARED_BLOCK: usize = 8192;

/// One registry pass. Sources are capture endpoints, sinks are playback.
pub(crate) struct NativeNodes {
    pub(crate) sources: Vec<DeviceItem>,
    pub(crate) sinks: Vec<DeviceItem>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EndpointKind {
    Source,
    Sink,
}

/// `pipewire:{node.name}`. The bare ALSA plugin id `alsa:pipewire` does not match.
pub(crate) fn node_name(device_id: &str) -> Option<&str> {
    let name = device_id.strip_prefix(PREFIX)?;
    if name.is_empty() || name.eq_ignore_ascii_case("pipewire") {
        None
    } else {
        Some(name)
    }
}

pub(crate) fn is_native_id(device_id: &str) -> bool {
    node_name(device_id).is_some()
}

/// The ALSA `pipewire` plugin and a PipeWire-backed `default` PCM.
///
/// These are the single shim row. Native node ids are a different path.
pub(crate) fn is_alsa_shim(device_id: &str) -> bool {
    if is_native_id(device_id) {
        return false;
    }
    let pcm = alsa_pcm_name(device_id).to_ascii_lowercase();
    pcm == "pipewire"
        || pcm.starts_with("pipewire:")
        || (alsa_pcm_is_default(device_id) && pipewire::is_pipewire_endpoint(device_id, ""))
}

/// Frames written into `node.latency`. Low latency asks for the same 64-frame
/// quantum as the metadata switch. The ordinary path uses the output buffer.
pub(crate) fn node_latency_frames(low_latency: bool, buffer_frames: usize) -> u32 {
    if low_latency {
        LOW_QUANTUM
    } else {
        buffer_frames.clamp(32, PREPARED_BLOCK) as u32
    }
}

/// Keep hardware and virtual sources and sinks. Skip monitors, our own nodes,
/// streams that are not endpoints, and nodes with no name.
pub(crate) fn endpoint_kind(name: &str, class: &str, monitor: bool) -> Option<EndpointKind> {
    if name.is_empty() || name.to_ascii_lowercase().starts_with("rigwerk") || monitor {
        return None;
    }
    if name.ends_with(".monitor") || class.to_ascii_lowercase().contains("monitor") {
        return None;
    }
    if class == "Audio/Source" || class.starts_with("Audio/Source/") {
        Some(EndpointKind::Source)
    } else if class == "Audio/Sink" || class.starts_with("Audio/Sink/") {
        Some(EndpointKind::Sink)
    } else {
        None
    }
}

/// Sources and sinks currently published by the server.
///
/// Fails when no server is running. Callers keep the ALSA shim in that case.
pub(crate) fn list_nodes() -> Result<NativeNodes, String> {
    ensure_init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|error| error.to_string())?;
    let context =
        pw::context::ContextRc::new(&mainloop, None).map_err(|error| error.to_string())?;
    let core = context
        .connect_rc(None)
        .map_err(|error| error.to_string())?;
    let registry = core.get_registry().map_err(|error| error.to_string())?;

    let found = Rc::new(RefCell::new(Vec::<Listed>::new()));
    let found_global = Rc::clone(&found);
    let pending = core.sync(0).map_err(|error| error.to_string())?;
    let loop_done = mainloop.clone();
    let _core_listener = core
        .add_listener_local()
        .done(move |id, seq| {
            if id == pw::core::PW_ID_CORE && seq == pending {
                loop_done.quit();
            }
        })
        .register();
    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            if global.type_ != pw::types::ObjectType::Node {
                return;
            }
            let Some(props) = global.props.as_ref() else {
                return;
            };
            let Some(name) = props.get("node.name") else {
                return;
            };
            if name.is_empty() {
                return;
            }
            let class = props.get("media.class").unwrap_or("");
            let monitor = props
                .get("stream.monitor")
                .is_some_and(|value| value == "true");
            if endpoint_kind(name, class, monitor).is_none() {
                return;
            }
            let description = props
                .get("node.description")
                .filter(|text| !text.is_empty())
                .unwrap_or(name);
            let priority = props
                .get("priority.session")
                .and_then(|text| text.parse::<f32>().ok())
                .map(|value| value as i32)
                .unwrap_or(0);
            let channels = props
                .get("audio.channels")
                .and_then(|text| text.parse::<usize>().ok())
                .filter(|channels| (1..=32).contains(channels))
                .unwrap_or(2);
            found_global.borrow_mut().push(Listed {
                name: name.to_owned(),
                description: description.to_owned(),
                class: class.to_owned(),
                monitor,
                priority,
                channels,
            });
        })
        .register();

    let loop_timer = mainloop.clone();
    let timer = mainloop.loop_().add_timer(move |_| loop_timer.quit());
    timer
        .update_timer(Some(Duration::from_millis(800)), None)
        .into_result()
        .map_err(|error| format!("PipeWire registry timer: {error}"))?;
    mainloop.run();
    drop(timer);

    let mut sources = Vec::new();
    let mut sinks = Vec::new();
    let mut listed = std::mem::take(&mut *found.borrow_mut());
    listed.sort_by(|a, b| {
        b.priority.cmp(&a.priority).then_with(|| {
            a.description
                .to_lowercase()
                .cmp(&b.description.to_lowercase())
        })
    });
    let mut seen = std::collections::HashSet::new();
    for node in listed {
        if !seen.insert(node.name.clone()) {
            continue;
        }
        let item = DeviceItem {
            id: format!("{PREFIX}{}", node.name),
            name: node.description,
            backend: "PIPEWIRE".to_owned(),
            channels: node.channels,
            demo: false,
            preferred: false,
            asio_clsid: None,
        };
        match endpoint_kind(&node.name, &node.class, node.monitor) {
            Some(EndpointKind::Source) => sources.push(item),
            Some(EndpointKind::Sink) => sinks.push(item),
            None => {}
        }
    }
    Ok(NativeNodes { sources, sinks })
}

/// Opens whichever sides are native PipeWire nodes and runs them until `stop`.
///
/// A side whose id is not native is opened through cpal, so PipeWire input
/// with an ALSA output (and the other way around) stays one bridge.
pub(crate) fn run(
    config: &StartConfig,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
    ensure_init();
    let input_native = config.input_id.as_deref().is_some_and(is_native_id);
    let output_native = is_native_id(&config.output_id);

    // Both sides in the PipeWire graph: one duplex node, no ring.  If that
    // node cannot be set up, fall back to the two-stream bridge below.
    let mut fallback_note = None;
    if input_native && output_native {
        match super::duplex::run(config, Arc::clone(&shared), Arc::clone(&stop)) {
            Ok(()) => return Ok(()),
            Err(super::duplex::DuplexError::Runtime(reason)) => return Err(reason),
            Err(super::duplex::DuplexError::Setup(reason)) => {
                fallback_note = Some(format!(" · duplex node unavailable: {reason}"));
            }
        }
    }

    let mut cpal_input = None;
    let input_channels;
    let input_rate;
    if input_native {
        input_rate = config.sample_rate;
        input_channels = 2;
        shared.set_source_rate(input_rate);
    } else {
        shared.set_status("Opening input…");
        let device = open_input_device(config)?;
        let input_config = crate::audio::devices::pick_input_config(&device, config.sample_rate)?;
        input_rate = input_config.sample_rate();
        input_channels = input_config.channels() as usize;
        shared.set_source_rate(input_rate);
        let stream = make_input_stream(
            &device,
            input_config,
            input_channels,
            Arc::clone(&shared),
            config.buffer_size,
        )?;
        stream
            .play()
            .map_err(|error| format!("Starting input: {error}"))?;
        cpal_input = Some(stream);
    }

    let output_rate = config.output_rate.unwrap_or(input_rate);
    let output_channels;
    let mut cpal_output = None;
    if output_native {
        output_channels = 2;
        let frames = node_latency_frames(config.pipewire_low_latency, config.output_buffer);
        let note = fallback_note.as_deref().unwrap_or("");
        let mode = if config.pipewire_low_latency {
            format!("Low latency · PipeWire node · {frames} frames{note}")
        } else {
            format!("PipeWire node · {frames} frames{note}")
        };
        shared.set_output(output_rate, frames, mode);
    } else {
        shared.set_status("Opening independent output…");
        let output = open_output(
            &config.output_id,
            output_rate,
            &shared,
            config.output_buffer,
            config.real_style_output,
            false,
        )?;
        output_channels = output.channels;
        cpal_output = Some(output);
    }

    shared.running.store(true, Ordering::Relaxed);
    shared.set_status(format!(
        "Bridging {} ch · {} → independent output ({} ch)",
        input_channels,
        crate::audio::bridge::rate_path(input_rate, output_rate),
        output_channels
    ));

    let result = run_graph(
        config,
        Arc::clone(&shared),
        Arc::clone(&stop),
        input_native,
        output_native,
        input_rate,
        output_rate,
    );
    drop(cpal_input);
    drop(cpal_output);
    result
}

fn run_graph(
    config: &StartConfig,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    input_native: bool,
    output_native: bool,
    input_rate: u32,
    output_rate: u32,
) -> Result<(), String> {
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|error| error.to_string())?;
    let context =
        pw::context::ContextRc::new(&mainloop, None).map_err(|error| error.to_string())?;
    let core = context
        .connect_rc(None)
        .map_err(|error| error.to_string())?;

    let capture_format;
    let playback_format;
    let capture = if input_native {
        let target = node_name(config.input_id.as_deref().unwrap_or(""))
            .ok_or_else(|| "Choose a PipeWire input".to_owned())?;
        let latency = latency_text(
            node_latency_frames(config.pipewire_low_latency, config.buffer_size),
            input_rate,
        );
        let stream = pw::stream::StreamBox::new(
            &core,
            CAPTURE_NODE,
            stream_properties("Capture", CAPTURE_NODE, Some(target), &latency),
        )
        .map_err(|error| format!("PipeWire capture: {error}"))?;
        capture_format = enum_format(input_rate)?;
        Some(stream)
    } else {
        capture_format = Vec::new();
        None
    };
    let playback = if output_native {
        let target =
            node_name(&config.output_id).ok_or_else(|| "Choose a PipeWire output".to_owned())?;
        let frames = node_latency_frames(config.pipewire_low_latency, config.output_buffer);
        let latency = latency_text(frames, output_rate);
        let stream = pw::stream::StreamBox::new(
            &core,
            PLAYBACK_NODE,
            stream_properties("Playback", PLAYBACK_NODE, Some(target), &latency),
        )
        .map_err(|error| format!("PipeWire playback: {error}"))?;
        playback_format = enum_format(output_rate)?;
        Some(stream)
    } else {
        playback_format = Vec::new();
        None
    };

    let _capture_listener = if let Some(stream) = capture.as_ref() {
        let capture_shared = Arc::clone(&shared);
        let state_shared = Arc::clone(&shared);
        let data = CaptureData {
            shared: capture_shared,
            channels: AtomicU32::new(2),
        };
        Some(
            stream
                .add_local_listener_with_user_data(data)
                .state_changed(move |_, _, _, new| {
                    if let pw::stream::StreamState::Error(message) = new {
                        state_shared.set_device_lost(format!("PipeWire input: {message}"));
                    }
                })
                .param_changed(|_, data, id, param| {
                    let Some(param) = param else {
                        return;
                    };
                    if id != spa::param::ParamType::Format.as_raw() {
                        return;
                    }
                    let mut info = spa::param::audio::AudioInfoRaw::new();
                    if info.parse(param).is_err() {
                        return;
                    }
                    let channels = info.channels().clamp(1, 32);
                    data.channels.store(channels, Ordering::Relaxed);
                    let rate = info.rate();
                    if rate > 0 {
                        data.shared.set_source_rate(rate);
                    }
                })
                .process(|stream, data| {
                    let Some(mut buffer) = stream.dequeue_buffer() else {
                        return;
                    };
                    let datas = buffer.datas_mut();
                    if datas.is_empty() {
                        return;
                    }
                    let data_buf = &mut datas[0];
                    let channels = data.channels.load(Ordering::Relaxed).max(1) as usize;
                    let (offset, size, stride) = {
                        let chunk = data_buf.chunk();
                        (chunk.offset(), chunk.size(), chunk.stride())
                    };
                    let Some(bytes) = data_buf.data() else {
                        return;
                    };
                    let window = capture_window(bytes.len(), offset, size, stride, channels);
                    push_capture(
                        &data.shared,
                        &bytes[window.start..],
                        channels,
                        window.stride,
                        window.frames,
                    );
                })
                .register()
                .map_err(|error| format!("PipeWire capture listener: {error}"))?,
        )
    } else {
        None
    };

    let _playback_listener = if let Some(stream) = playback.as_ref() {
        let state_shared = Arc::clone(&shared);
        // The playback stream is always negotiated as F32LE stereo (FL/FR,
        // see `enum_format`), so the device map applies to two channels.
        let mut mixer = OutputMixer::new(output_rate, 2);
        mixer.follow_source_rate(&shared);
        mixer.publish_device_channels(&shared);
        mixer.prepare(PREPARED_BLOCK);
        let data = PlaybackData {
            shared: Arc::clone(&shared),
            mixer,
        };
        Some(
            stream
                .add_local_listener_with_user_data(data)
                .state_changed(move |_, _, _, new| {
                    if let pw::stream::StreamState::Error(message) = new {
                        state_shared.set_device_lost(format!("PipeWire output: {message}"));
                    }
                })
                .process(|stream, data| {
                    let Some(mut buffer) = stream.dequeue_buffer() else {
                        return;
                    };
                    // PipeWire maps buffers to the session max quantum. Fill only
                    // `requested` frames for this cycle; maxsize alone is what made
                    // playback drain the ring in one shot and then stutter.
                    let requested = buffer.requested();
                    let datas = buffer.datas_mut();
                    if datas.is_empty() {
                        return;
                    }
                    let data_buf = &mut datas[0];
                    let frames = {
                        let Some(bytes) = data_buf.data() else {
                            return;
                        };
                        let frames = playback_frames(bytes.len(), 8, requested);
                        if frames == 0 {
                            return;
                        }
                        data.shared
                            .output_step_frames
                            .store(frames as u32, Ordering::Relaxed);
                        data.mixer.render(&data.shared, frames, |index, mapped| {
                            let start = index * 8;
                            if start + 8 <= bytes.len() {
                                bytes[start..start + 4]
                                    .copy_from_slice(&mapped.sample(0).to_le_bytes());
                                bytes[start + 4..start + 8]
                                    .copy_from_slice(&mapped.sample(1).to_le_bytes());
                            }
                        });
                        frames
                    };
                    let chunk = data_buf.chunk_mut();
                    *chunk.offset_mut() = 0;
                    *chunk.stride_mut() = 8;
                    *chunk.size_mut() = (frames * 8) as u32;
                    drop(buffer);
                    // Graph + device delay past this node (BT A2DP, filters, …).
                    if let Ok(time) = stream.time() {
                        let rate = time.rate();
                        if let Some(ns) = path_delay_ns(time.delay(), rate.num, rate.denom) {
                            data.shared.set_path_delay_ns(ns);
                        }
                    }
                })
                .register()
                .map_err(|error| format!("PipeWire playback listener: {error}"))?,
        )
    } else {
        None
    };

    if let Some(stream) = capture.as_ref() {
        let mut params = [pod(&capture_format)?];
        stream
            .connect(
                spa::utils::Direction::Input,
                None,
                pw::stream::StreamFlags::AUTOCONNECT
                    | pw::stream::StreamFlags::MAP_BUFFERS
                    | pw::stream::StreamFlags::RT_PROCESS,
                &mut params,
            )
            .map_err(|error| format!("Connecting PipeWire input: {error}"))?;
    }
    if let Some(stream) = playback.as_ref() {
        let mut params = [pod(&playback_format)?];
        stream
            .connect(
                spa::utils::Direction::Output,
                None,
                pw::stream::StreamFlags::AUTOCONNECT
                    | pw::stream::StreamFlags::MAP_BUFFERS
                    | pw::stream::StreamFlags::RT_PROCESS,
                &mut params,
            )
            .map_err(|error| format!("Connecting PipeWire output: {error}"))?;
    }

    let stop_flag = Arc::clone(&stop);
    let loop_timer = mainloop.clone();
    let timer = mainloop.loop_().add_timer(move |_| {
        if stop_flag.load(Ordering::Relaxed) {
            loop_timer.quit();
        }
    });
    timer
        .update_timer(
            Some(Duration::from_millis(50)),
            Some(Duration::from_millis(50)),
        )
        .into_result()
        .map_err(|error| format!("PipeWire stop timer: {error}"))?;
    mainloop.run();
    drop(timer);
    Ok(())
}

struct Listed {
    name: String,
    description: String,
    class: String,
    monitor: bool,
    priority: i32,
    channels: usize,
}

struct CaptureData {
    shared: Arc<Shared>,
    channels: AtomicU32,
}

struct PlaybackData {
    shared: Arc<Shared>,
    mixer: OutputMixer,
}

/// Where the valid capture frames sit in a mapped buffer.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CaptureWindow {
    /// Byte offset of the first frame.
    pub(crate) start: usize,
    /// Bytes from one frame to the next.
    pub(crate) stride: usize,
    pub(crate) frames: usize,
}

/// Reads a `spa_chunk` the way PipeWire defines it: the valid data starts at
/// `offset` (taken modulo the mapped size), is `size` bytes long, and frames
/// are `stride` bytes apart.  A stride of 0, or one too small for the
/// negotiated channels, falls back to packed f32 frames.  The frame count is
/// bounded by what is mapped after the offset, so a bogus chunk can never make
/// the reader run past the buffer.
pub(crate) fn capture_window(
    mapped: usize,
    offset: u32,
    size: u32,
    stride: i32,
    channels: usize,
) -> CaptureWindow {
    let packed = channels.max(1) * 4;
    let stride = match usize::try_from(stride) {
        Ok(stride) if stride >= packed => stride,
        _ => packed,
    };
    if mapped == 0 {
        return CaptureWindow {
            start: 0,
            stride,
            frames: 0,
        };
    }
    let start = offset as usize % mapped;
    let available = (size as usize).min(mapped - start);
    // The last frame only needs its own samples, not a whole stride after it.
    let frames = if available < packed {
        0
    } else {
        (available - packed) / stride + 1
    };
    CaptureWindow {
        start,
        stride,
        frames,
    }
}

fn push_capture(shared: &Shared, bytes: &[u8], channels: usize, stride: usize, frames: usize) {
    let _denormals = DenormalGuard::new();
    let channels = channels.max(1);
    shared.note_input_block(frames);
    shared.note_input_frames(frames);
    let gain = shared.input_gain();
    let enabled_l = shared.input_enabled[0].load(Ordering::Relaxed);
    let enabled_r = shared.input_enabled[1].load(Ordering::Relaxed);
    let mut peak_l = 0.0f32;
    let mut peak_r = 0.0f32;
    for frame in 0..frames {
        let base = frame * stride;
        let raw_left = sample_at(bytes, base);
        let raw_right = if channels > 1 {
            sample_at(bytes, base + 4)
        } else {
            raw_left
        };
        // Raw, before gain: a calibration must see the converter's level.
        shared.capture_calibration(raw_left, raw_right);
        let mut left = raw_left * gain;
        let mut right = raw_right * gain;
        shared.tap_frame(left, right);
        if !enabled_l {
            left = 0.0;
        }
        if !enabled_r {
            right = 0.0;
        }
        peak_l = peak_l.max(left.abs());
        peak_r = peak_r.max(right.abs());
        shared.push_frame(left, right);
    }
    shared.in_l.store(peak_l.to_bits(), Ordering::Relaxed);
    shared.in_r.store(peak_r.to_bits(), Ordering::Relaxed);
}

/// The f32 sample starting at byte `start`, or silence past the end.
fn sample_at(bytes: &[u8], start: usize) -> f32 {
    let Some(chunk) = bytes.get(start..start + 4) else {
        return 0.0;
    };
    f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])
}

fn stream_properties(
    category: &str,
    node_name: &str,
    target: Option<&str>,
    latency: &str,
) -> pw::properties::PropertiesBox {
    let mut props = pw::properties::PropertiesBox::new();
    props.insert(*pw::keys::MEDIA_TYPE, "Audio");
    props.insert(*pw::keys::MEDIA_CATEGORY, category);
    props.insert(*pw::keys::MEDIA_ROLE, "Production");
    props.insert(*pw::keys::NODE_NAME, node_name);
    props.insert(*pw::keys::NODE_DESCRIPTION, node_name);
    props.insert(*pw::keys::NODE_LATENCY, latency);
    if let Some(target) = target {
        props.insert("target.object", target);
    }
    props
}

fn latency_text(frames: u32, rate: u32) -> String {
    format!("{frames}/{}", rate.max(1))
}

/// How many stereo frames to write into one playback buffer.
///
/// `max_bytes / stride` is the mapped capacity (often the session max quantum).
/// `requested` is what this graph cycle actually wants; 0 means no hint, so the
/// capacity is used. Matching the PipeWire audio-src tutorial keeps the ring
/// and the graph on the same step size.
pub(crate) fn playback_frames(max_bytes: usize, stride: usize, requested: u64) -> usize {
    if stride == 0 {
        return 0;
    }
    let capacity = max_bytes / stride;
    if requested == 0 {
        capacity
    } else {
        capacity.min(requested as usize)
    }
}

/// Converts `pw_time.delay` into nanoseconds. Negative delay (sync offset) is
/// clamped to zero; a zero-denominator rate is rejected.
pub(crate) fn path_delay_ns(delay_ticks: i64, rate_num: u32, rate_denom: u32) -> Option<u64> {
    if rate_denom == 0 {
        return None;
    }
    let delay = delay_ticks.max(0) as i128;
    let ns = delay
        .saturating_mul(i128::from(rate_num))
        .saturating_mul(1_000_000_000)
        / i128::from(rate_denom);
    u64::try_from(ns).ok()
}

fn enum_format(rate: u32) -> Result<Vec<u8>, String> {
    let mut audio = spa::param::audio::AudioInfoRaw::new();
    audio.set_format(spa::param::audio::AudioFormat::F32LE);
    audio.set_rate(rate);
    audio.set_channels(2);
    let mut position = [0; spa::param::audio::MAX_CHANNELS];
    position[0] = CHANNEL_FL;
    position[1] = CHANNEL_FR;
    audio.set_position(position);
    let object = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: audio.into(),
    };
    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(object),
    )
    .map(|(cursor, _)| cursor.into_inner())
    .map_err(|error| format!("PipeWire format: {error}"))
}

fn pod(bytes: &[u8]) -> Result<&Pod, String> {
    Pod::from_bytes(bytes).ok_or_else(|| "PipeWire format was empty".to_owned())
}

pub(crate) fn ensure_init() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(pw::init);
}

#[cfg(test)]
mod tests;
