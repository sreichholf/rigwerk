//! A capture stream on the clock sink's monitor ports, opened only while
//! Low latency runs, so the Live Issues strip can tell a sink that went
//! silent from a guitar rest (`app::issues::silence`).
//!
//! It is its own PipeWire client on its own thread and main loop, opened
//! and closed by the engine (the UI thread), never by a bridge callback.
//! The process callback only folds the block's peak into an atomic and
//! counts the block: no allocation, no lock, no logging.  The stream is
//! passive and asks for the low quantum, so it neither keeps the sink
//! awake nor raises the graph block.  A native sink is targeted by name;
//! the ALSA `pipewire` shim plays to the default sink, so that is the one
//! monitored.  When nothing links, no block arrives and the reading stays
//! `None`: the detection is inert rather than guessing.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ::pipewire as pw;
use pw::spa;
use pw::spa::pod::Pod;

use super::native::{ensure_init, is_alsa_shim, node_name};
use super::pipewire::LOW_QUANTUM;

const MONITOR_NODE: &str = "RigWerk Sink Monitor";
/// A reading older than this is no reading: the monitor stopped flowing.
const STALE_AFTER: Duration = Duration::from_millis(250);

/// What the process callback publishes.
#[derive(Debug, Default)]
pub(crate) struct MonitorMeter {
    /// The largest |sample| since the last read, as f32 bits.  Non-negative
    /// floats order like their bits, so `fetch_max` keeps the peak.
    peak: AtomicU32,
    /// Blocks seen since the stream opened.
    blocks: AtomicU64,
}

impl MonitorMeter {
    /// Folds one block's peak in (the audio thread's side).
    pub(crate) fn note_block(&self, peak: f32) {
        let peak = if peak.is_finite() { peak.abs() } else { 0.0 };
        self.peak.fetch_max(peak.to_bits(), Ordering::Relaxed);
        self.blocks.fetch_add(1, Ordering::Release);
    }
}

/// The reading side, kept by the engine.
#[derive(Debug, Default)]
pub(crate) struct MonitorReader {
    seen_blocks: u64,
    flowing_at: Option<Instant>,
    last_peak: f32,
}

impl MonitorReader {
    /// The monitor's peak since the last read, or `None` while no block has
    /// arrived within [`STALE_AFTER`].
    pub(crate) fn read(&mut self, meter: &MonitorMeter, now: Instant) -> Option<f32> {
        let blocks = meter.blocks.load(Ordering::Acquire);
        if blocks != self.seen_blocks {
            self.seen_blocks = blocks;
            self.flowing_at = Some(now);
            self.last_peak = f32::from_bits(meter.peak.swap(0, Ordering::Relaxed));
        }
        self.flowing_at
            .filter(|&at| now.saturating_duration_since(at) < STALE_AFTER)
            .map(|_| self.last_peak)
    }
}

/// The largest |sample| among the f32 samples of a `spa_chunk`, bounded by
/// the mapped bytes so a bogus chunk cannot read past them.
pub(crate) fn chunk_peak(bytes: &[u8], offset: u32, size: u32) -> f32 {
    if bytes.is_empty() {
        return 0.0;
    }
    let start = offset as usize % bytes.len();
    let end = start + (size as usize).min(bytes.len() - start);
    bytes[start..end]
        .chunks_exact(4)
        .map(|sample| f32::from_le_bytes([sample[0], sample[1], sample[2], sample[3]]).abs())
        .filter(|sample| sample.is_finite())
        .fold(0.0, f32::max)
}

/// Which sink a clock output id plays to: `Some(Some(name))` for a native
/// node, `Some(None)` for the default sink behind the ALSA shim, `None`
/// for an output that is not a PipeWire sink.
pub(crate) fn monitor_target(output_id: &str) -> Option<Option<String>> {
    if let Some(name) = node_name(output_id) {
        Some(Some(name.to_owned()))
    } else if is_alsa_shim(output_id) {
        Some(None)
    } else {
        None
    }
}

/// A running sink monitor.  Dropping it asks its thread to stop; the
/// thread ends on its own within one stop-timer tick.
pub(crate) struct SinkMonitor {
    meter: Arc<MonitorMeter>,
    reader: MonitorReader,
    stop: Arc<AtomicBool>,
    _thread: JoinHandle<()>,
}

impl SinkMonitor {
    /// Starts monitoring the sink `output_id` plays to, or `None` when it
    /// is not a PipeWire sink or the thread cannot start.  Connecting
    /// happens on the monitor's thread; a failure there leaves the reading
    /// `None`.
    pub(crate) fn open(output_id: &str, rate: u32) -> Option<Self> {
        let target = monitor_target(output_id)?;
        let meter = Arc::new(MonitorMeter::default());
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let meter = Arc::clone(&meter);
            let stop = Arc::clone(&stop);
            thread::Builder::new()
                .name("rigwerk-sink-monitor".to_owned())
                .spawn(move || {
                    let _ = run(target.as_deref(), rate, meter, stop);
                })
                .ok()?
        };
        Some(Self {
            meter,
            reader: MonitorReader::default(),
            stop,
            _thread: thread,
        })
    }

    pub(crate) fn read(&mut self, now: Instant) -> Option<f32> {
        self.reader.read(&self.meter, now)
    }
}

impl Drop for SinkMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// The monitor's main loop, until `stop`.
pub(crate) fn run(
    target: Option<&str>,
    rate: u32,
    meter: Arc<MonitorMeter>,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
    ensure_init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|error| error.to_string())?;
    let context =
        pw::context::ContextRc::new(&mainloop, None).map_err(|error| error.to_string())?;
    let core = context
        .connect_rc(None)
        .map_err(|error| error.to_string())?;

    let mut props = pw::properties::PropertiesBox::new();
    props.insert(*pw::keys::MEDIA_TYPE, "Audio");
    props.insert(*pw::keys::MEDIA_CATEGORY, "Capture");
    props.insert(*pw::keys::MEDIA_ROLE, "Production");
    props.insert(*pw::keys::NODE_NAME, MONITOR_NODE);
    props.insert(*pw::keys::NODE_DESCRIPTION, MONITOR_NODE);
    props.insert(
        *pw::keys::NODE_LATENCY,
        format!("{LOW_QUANTUM}/{}", rate.max(1)),
    );
    props.insert("node.passive", "true");
    props.insert("stream.capture.sink", "true");
    if let Some(target) = target {
        props.insert("target.object", target);
    }
    let stream = pw::stream::StreamBox::new(&core, MONITOR_NODE, props)
        .map_err(|error| format!("PipeWire sink monitor: {error}"))?;

    let _listener = stream
        .add_local_listener_with_user_data(meter)
        .process(|stream, meter| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else {
                return;
            };
            let (offset, size) = {
                let chunk = data.chunk();
                (chunk.offset(), chunk.size())
            };
            let Some(bytes) = data.data() else {
                return;
            };
            meter.note_block(chunk_peak(bytes, offset, size));
        })
        .register()
        .map_err(|error| format!("PipeWire sink monitor listener: {error}"))?;

    // F32 only: rate and channels follow the sink, the peak spans them all.
    let mut audio = spa::param::audio::AudioInfoRaw::new();
    audio.set_format(spa::param::audio::AudioFormat::F32LE);
    let object = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: audio.into(),
    };
    let format = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(object),
    )
    .map(|(cursor, _)| cursor.into_inner())
    .map_err(|error| format!("PipeWire sink monitor format: {error}"))?;
    let mut params =
        [Pod::from_bytes(&format).ok_or_else(|| "PipeWire format was empty".to_owned())?];
    stream
        .connect(
            spa::utils::Direction::Input,
            None,
            pw::stream::StreamFlags::AUTOCONNECT
                | pw::stream::StreamFlags::MAP_BUFFERS
                | pw::stream::StreamFlags::RT_PROCESS,
            &mut params,
        )
        .map_err(|error| format!("Connecting the PipeWire sink monitor: {error}"))?;

    let loop_timer = mainloop.clone();
    let timer = mainloop.loop_().add_timer(move |_| {
        if stop.load(Ordering::Relaxed) {
            loop_timer.quit();
        }
    });
    timer
        .update_timer(
            Some(Duration::from_millis(50)),
            Some(Duration::from_millis(50)),
        )
        .into_result()
        .map_err(|error| format!("PipeWire sink monitor timer: {error}"))?;
    mainloop.run();
    drop(timer);
    Ok(())
}

#[cfg(test)]
mod tests;
