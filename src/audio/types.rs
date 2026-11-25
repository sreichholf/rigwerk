//! The public vocabulary of the audio side: devices, inventory, engine
//! state, live snapshot, bridge buffer mode and the bridge's start settings.

use super::loudness::LoudnessReading;
use super::output_map::{MAX_EXTRA_OUTPUTS, MAX_OUTPUT_CHANNELS};
use super::path::MAX_PATHS;
use super::shared::CallbackFailure;

pub const DEMO_ASIO_ID: &str = "demo:asio";
pub const DEMO_INPUT_ID: &str = "demo:input";
pub const DEMO_OUTPUT_ID: &str = "demo:output";

/// The quantum RigWerk asks PipeWire for while the low-latency bridge runs.
pub const PIPEWIRE_LOW_QUANTUM: u32 = 64;
/// Above this quantum a PipeWire route is shown as the desktop's shared size
/// instead of being counted as low latency.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
pub const PIPEWIRE_LOW_QUANTUM_MAX: u32 = 256;

/// What the Linux low-latency path got: PipeWire's graph quantum compared with
/// the ALSA period the output will use.  Empty on other platforms.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Clone, Debug, Default)]
pub struct QuantumInfo {
    /// Whether PipeWire's settings store could be read.
    pub available: bool,
    /// PipeWire's current graph quantum in frames (0 when unknown).
    pub quantum: u32,
    /// PipeWire's graph rate in Hz (0 when unknown).
    pub rate: u32,
    /// The ALSA period the output asks for (0 until it is known).
    pub alsa_period: u32,
    /// Whether the quantum is small enough to count as low latency.
    pub low_latency: bool,
    /// Whether the ALSA period and quantum line up without extra buffering.
    pub fits: bool,
    /// One plain explanation, ready for the UI and the activity log.
    pub note: String,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl QuantumInfo {
    /// Builds the comparison.  A period at or below the quantum that divides
    /// it evenly is a fit: the ALSA side is at least as fine as the graph.
    pub fn assess(available: bool, quantum: u32, rate: u32, alsa_period: u32) -> Self {
        let rate = if rate == 0 { 48_000 } else { rate };
        let ms = |frames: u32| frames as f32 / rate as f32 * 1000.0;
        if !available || quantum == 0 {
            return Self {
                available,
                quantum,
                rate,
                alsa_period,
                low_latency: false,
                fits: false,
                note: "The PipeWire graph buffer could not be read (the desktop settings are missing or the server is not running). RigWerk still uses the ALSA buffer; start PipeWire for the low-latency path.".to_owned(),
            };
        }
        let low_latency = quantum <= PIPEWIRE_LOW_QUANTUM_MAX;
        let fits = alsa_period > 0 && alsa_period <= quantum && quantum.is_multiple_of(alsa_period);
        let note = if !low_latency {
            format!(
                "The PipeWire graph buffer is still {quantum} samples ({:.1} ms at {:.1} kHz). That is the shared desktop size, not the clock output. Low latency asks the graph for {} samples.",
                ms(quantum),
                rate as f32 / 1000.0,
                PIPEWIRE_LOW_QUANTUM
            )
        } else if alsa_period == 0 {
            format!(
                "PipeWire is running at a {quantum}-sample buffer ({:.1} ms). The ALSA buffer size is not known yet; the bridge compares them as soon as the output opens.",
                ms(quantum)
            )
        } else if fits {
            if alsa_period == quantum {
                format!(
                    "The ALSA buffer of {alsa_period} samples matches the PipeWire graph buffer ({quantum} samples, {:.1} ms).",
                    ms(quantum)
                )
            } else {
                format!(
                    "The ALSA buffer of {alsa_period} samples fits {}× into the PipeWire graph buffer ({quantum} samples, {:.1} ms).",
                    quantum / alsa_period,
                    ms(quantum)
                )
            }
        } else if alsa_period > quantum {
            format!(
                "The ALSA buffer ({alsa_period} samples) is larger than the PipeWire graph buffer ({quantum} samples), so the coarser ALSA buffer sets the latency. Set the output buffer to {quantum} samples or lower."
            )
        } else {
            format!(
                "The ALSA buffer ({alsa_period} samples) does not divide the PipeWire graph buffer ({quantum} samples), so the two clocks do not line up and can cause a dropout. Pick a buffer size that divides {quantum} (for example 32 or 64 samples)."
            )
        };
        Self {
            available,
            quantum,
            rate,
            alsa_period,
            low_latency,
            fits,
            note,
        }
    }

    /// Re-checks the fit once the output's real period is known.
    pub fn with_period(&self, alsa_period: u32) -> Self {
        if alsa_period == self.alsa_period {
            return self.clone();
        }
        Self::assess(self.available, self.quantum, self.rate, alsa_period)
    }
}

#[cfg(test)]
mod tests;

#[derive(Clone, Debug)]
pub struct DeviceItem {
    pub id: String,
    pub name: String,
    pub backend: String,
    pub channels: usize,
    pub demo: bool,
    pub preferred: bool,
    pub asio_clsid: Option<u128>,
}

#[derive(Clone, Debug, Default)]
pub struct Inventory {
    pub asio: Vec<DeviceItem>,
    pub inputs: Vec<DeviceItem>,
    pub outputs: Vec<DeviceItem>,
    /// Problems met while scanning (only the Windows ASIO scan reports any).
    /// The Diagnostics page lists them, and each new note is also written to
    /// the activity log so a later scan cannot erase it.
    pub notes: Vec<String>,
    pub os: String,
    /// What the dedicated low-latency input is called on this platform.
    /// "ASIO" on Windows. On Linux, "PIPEWIRE" when that plugin is present,
    /// otherwise "ALSA" for direct hardware. The label does not make every
    /// ALSA PCM low latency.
    pub low_latency_label: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineState {
    Idle,
    Preview,
    Starting,
    Running,
    /// Stop was requested but the bridge thread has not confirmed teardown.
    /// The device is only released once `poll` sees it finish.
    Stopping,
    Error,
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub in_l: f32,
    pub in_r: f32,
    pub out_l: f32,
    pub out_r: f32,
    /// Per path slot: the gate's deepest gain reduction in the last block,
    /// dB.  0 while the gate is open or off, and for a slot no mixer runs.
    #[cfg_attr(not(test), allow(dead_code))] // For the gate sheet's meter.
    pub gate_reduction_db: [f32; MAX_PATHS],
    /// Block peak per clock-device channel, after its channel map, level,
    /// safety ceiling, metronome and mute.  The tick is included.  Channels
    /// the device does not have read 0.
    pub out_channels: [f32; MAX_OUTPUT_CHANNELS],
    /// Safety-limiter gain reduction on the clock output, dB.  0 when it
    /// is not limiting.
    pub out_reduction_db: f32,
    /// Loudness of the clock output: momentary, short-term, integrated and
    /// the output trim it applied.  No reading is silence at
    /// [`super::SILENCE_LUFS`].
    pub out_loudness: LoudnessReading,
    /// The clock output's real channel count (0 until its first block).
    pub output_channels: u32,
    /// How many extra outputs the engine currently maps (≤ [`MAX_EXTRA_OUTPUTS`]).
    #[allow(dead_code)] // Published on the snapshot. The strips do not read the count.
    pub extra_output_count: usize,
    /// Block peak per extra output channel, after that extra output's map, level,
    /// safety ceiling, metronome and mute.  The tick is included.
    pub extra_output_channels: [[f32; MAX_OUTPUT_CHANNELS]; MAX_EXTRA_OUTPUTS],
    /// Safety-limiter gain reduction per extra output, dB.  0 when not limiting.
    /// The virtual output is one of these slots.
    pub extra_output_reduction_db: [f32; MAX_EXTRA_OUTPUTS],
    /// Loudness per extra output, as [`Self::out_loudness`].
    pub extra_output_loudness: [LoudnessReading; MAX_EXTRA_OUTPUTS],
    /// Each extra output's real channel count (0 until published).
    pub extra_output_channel_counts: [u32; MAX_EXTRA_OUTPUTS],
    /// Generic extra outputs that failed to open or were lost, and are being
    /// retried.  PipeWire in-graph extra outputs always read false.
    pub extra_output_missing: [bool; MAX_EXTRA_OUTPUTS],
    /// Dropouts of each generic extra output's own ring since its stream
    /// opened (a reopen starts again from 0).
    pub extra_output_dropouts: [u64; MAX_EXTRA_OUTPUTS],
    /// Generic extra outputs whose drift loop is pinned at its limit.
    pub extra_output_drift_at_limit: [bool; MAX_EXTRA_OUTPUTS],
    /// The rate each extra output's own stream opened at, Hz.  0 while it has
    /// no stream: not open yet, missing, or played in the PipeWire graph.
    #[cfg_attr(not(test), allow(dead_code))] // For the output strip and sheet.
    pub extra_output_rates: [u32; MAX_EXTRA_OUTPUTS],
    /// The frames each extra output's driver asks for per callback (its real
    /// buffer size).  0 while it has no stream, and until its first
    /// callback.
    #[cfg_attr(not(test), allow(dead_code))] // For the output strip and sheet.
    pub extra_output_buffer_frames: [u32; MAX_EXTRA_OUTPUTS],
    /// Each extra output's ring fill against its bridge buffer, 0 to 1 (as
    /// [`Self::ring_fill`]).  0 while it has no stream.
    #[cfg_attr(not(test), allow(dead_code))] // For the output strip and sheet.
    pub extra_output_ring_fill: [f32; MAX_EXTRA_OUTPUTS],
    /// Each extra output's bridge buffer (its ring's fill target), in frames at the
    /// clock output's rate.  0 while it has no stream.
    #[cfg_attr(not(test), allow(dead_code))] // For the output strip and sheet.
    pub extra_output_target_frames: [usize; MAX_EXTRA_OUTPUTS],
    pub dropouts: u64,
    /// Frames the input callback dropped because the ring was full.
    #[allow(dead_code)] // Published for diagnostics; no UI consumer yet.
    pub overruns: u64,
    /// Frames the input callback delivered (0 until the first block arrives).
    #[allow(dead_code)] // Published for diagnostics; the heartbeat reads Shared directly.
    pub input_frames: u64,
    pub frames: u64,
    pub ring_fill: f32,
    pub target_frames: usize,
    pub drift_ppm: f32,
    pub error: Option<String>,
    pub device_lost: bool,
    /// Why a real-time callback stopped the run, when one did.  `device_lost`
    /// is true then too, because the run cannot continue, but no device went
    /// away.
    pub callback_failure: Option<CallbackFailure>,
    /// The bridge's own status line. The header pill follows [`EngineState`]
    /// and does not read this, so an error pill stays an error pill.
    #[allow(dead_code)] // Published for diagnostics; no UI consumer yet.
    pub status: String,
    /// The rate the input clock really delivers (0 until it is known).
    /// [`Self::input_rate_reported`] says whether it is a reading or only
    /// the rate asked for before the stream or graph opened.
    pub input_rate: u32,
    /// Whether [`Self::input_rate`] came from the device or graph.  False
    /// for a native PipeWire capture between the start and its first
    /// negotiated format.
    pub input_rate_reported: bool,
    /// The block size the input driver really delivers, in samples (0 until
    /// the first block arrives).
    pub input_block_frames: u32,
    /// What the output side really got: its rate and the step it is fed in.
    /// [`Self::output_rate_reported`] says whether the rate is a reading.
    pub output_rate: u32,
    /// Whether [`Self::output_rate`] came from the device or graph.  False
    /// for a native PipeWire stream between the start and the format it
    /// negotiated, and on the duplex node until its first cycle.
    pub output_rate_reported: bool,
    /// The step the output side got: the block its callback really feeds.
    /// [`Self::output_step_reported`] says whether it is a reading or only
    /// the quantum asked for before a native PipeWire stream ran.
    pub output_step_frames: u32,
    /// Whether [`Self::output_step_frames`] came from the device or graph.
    /// False for a native PipeWire stream between the start and its first
    /// block, and on the duplex node until its first cycle.
    pub output_step_reported: bool,
    /// One line on how the output runs, e.g. REAL-style or why it fell back.
    /// Display only: compare [`Self::opened_output_mode`] instead.
    pub output_mode: String,
    /// The mode the clock output opened in, published with
    /// [`Self::output_mode`].  `None` until the output has opened.
    pub opened_output_mode: Option<OutputMode>,
    /// Milliseconds from our playback node to the graph edge (device/filters),
    /// when PipeWire reports it. `None` on non-PipeWire outputs or before the
    /// first timing sample. Bluetooth A2DP often lands around 100–200 ms here.
    pub path_delay_ms: Option<f32>,
    /// True when input and output run in one PipeWire graph callback (the
    /// duplex node): there is no ring, bridge buffer or drift correction.
    pub direct: bool,
    /// Metronome beat the mixer last published.  0 when it is stopped,
    /// otherwise `1..=clicks` of the rhythm the beat belongs to.  0 after the
    /// bridge stops or the device is lost.
    pub metronome_beat: u8,
}

/// How the bridge buffer between input and output (the ring buffer's fill
/// target) is sized.  It can change while the bridge runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BufferMode {
    /// Twice the input buffer, at least two output blocks; grows 256 samples
    /// per dropout and shrinks 64 after 4 calm seconds.
    Auto = 0,
    /// One input block plus one output block; grows and shrinks in smaller,
    /// quicker steps.
    Minimum = 1,
    /// A fixed size in milliseconds that never grows on its own.
    Fixed = 2,
}

impl BufferMode {
    pub(crate) fn from_u32(value: u32) -> Self {
        match value {
            1 => Self::Minimum,
            2 => Self::Fixed,
            _ => Self::Auto,
        }
    }
}

/// How the Windows WASAPI clock output opens.  Extra outputs stay shared.
///
/// Older preferences only stored a `low_latency_output` bool; `true` maps to
/// [`Self::RealStyle`], `false` to [`Self::Shared`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WasapiOutputMode {
    /// Normal shared cpal stream with the chosen `output_buffer`.
    Shared,
    /// Shared IAudioClient3 at the driver's smallest engine step (REAL-style).
    RealStyle,
    /// Exclusive mode on the clock output only; other apps cannot play there.
    Exclusive,
}

impl WasapiOutputMode {
    pub fn from_low_latency(low_latency: bool) -> Self {
        if low_latency {
            Self::RealStyle
        } else {
            Self::Shared
        }
    }

    /// Short UI label for the OUTPUT card control.
    pub fn label(self) -> &'static str {
        match self {
            Self::Shared => "Shared",
            Self::RealStyle => "Shared, smallest buffer",
            Self::Exclusive => "Exclusive",
        }
    }

    /// Whether this mode asks for a low-latency attempt (not plain Shared).
    pub fn wants_low_latency(self) -> bool {
        !matches!(self, Self::Shared)
    }
}

/// How the clock output really opened, published beside the mode line
/// ([`Snapshot::output_mode`]).  An open that refused what was asked for
/// (an exclusive or low-latency attempt, a fixed buffer) publishes the
/// mode that runs instead, never the request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputMode {
    /// Linux: a stream at the chosen output buffer, or a PipeWire node at
    /// the graph's own size.
    Standard,
    /// Linux Low latency: ALSA's smallest period, or a PipeWire node at the
    /// low quantum.
    LowLatency,
    /// Windows (and any other non-Linux host): a shared stream at the
    /// chosen output buffer.
    Shared,
    /// WASAPI shared at the driver's smallest engine step (REAL-style).
    RealStyle,
    /// WASAPI exclusive, this device only.
    Exclusive,
    /// An ASIO driver, alone or as the input's driver too.
    Asio,
}

impl OutputMode {
    /// The value [`super::shared::Shared`] stores; 0 is "nothing opened".
    pub(crate) fn to_u8(self) -> u8 {
        match self {
            Self::Standard => 1,
            Self::LowLatency => 2,
            Self::Shared => 3,
            Self::RealStyle => 4,
            Self::Exclusive => 5,
            Self::Asio => 6,
        }
    }

    pub(crate) fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            1 => Self::Standard,
            2 => Self::LowLatency,
            3 => Self::Shared,
            4 => Self::RealStyle,
            5 => Self::Exclusive,
            6 => Self::Asio,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug)]
pub struct StartConfig {
    /// Set when the input is an ASIO driver (Windows only).
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub asio_clsid: Option<u128>,
    pub input_id: Option<String>,
    pub output_id: String,
    /// Set when the selected output is an ASIO driver.  The bridge then opens
    /// that driver for playback instead of handing the device to a system host.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub output_asio_clsid: Option<u128>,
    /// Input side: the rate and buffer asked of the capture device.
    pub sample_rate: u32,
    pub buffer_size: usize,
    /// Output side rate; `None` follows whatever rate the input really runs at.
    pub output_rate: Option<u32>,
    /// Output side buffer in frames for a normal shared stream: what a stream
    /// asks for when no low-latency request is made, and on Windows what the
    /// fallback after a refused request asks for.  A refused fixed buffer is
    /// retried at the driver's default instead; see
    /// [`Self::low_latency_request`].
    pub output_buffer: usize,
    /// A wrapper driver (ASIO4ALL and friends) does not name its hardware, so
    /// playback errors get an extra hint about sharing one sound card.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub asio_wrapper: bool,
    /// Ask the clock output for the lowest latency the backend can open.  On
    /// Windows that is WASAPI's IAudioClient3 at the smallest shared engine
    /// step the driver allows (REAL-style); on Linux it is ALSA's smallest
    /// usable period, opened as the shared stream's fixed buffer.  A refusal
    /// falls back to the shared cpal stream, at `output_buffer` on Windows and
    /// at ALSA's requested period on Linux; when that stream does not open at
    /// the requested buffer, it is retried once at the driver's default, and
    /// only a second failure is an error.  Ignored when
    /// [`Self::exclusive_output`] is set; ASIO playback ignores both.
    pub low_latency_request: bool,
    /// WASAPI exclusive mode for the clock output only.  On refuse, falls back
    /// to shared cpal with `output_buffer` (not REAL-style).
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub exclusive_output: bool,
    /// Linux: ask PipeWire for its low quantum while the bridge runs.  The
    /// quantum is released as soon as the bridge stops.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub pipewire_low_latency: bool,
}
