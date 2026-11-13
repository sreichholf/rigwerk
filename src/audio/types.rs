//! The public vocabulary of the audio side: devices, inventory, engine
//! state, live snapshot, cushion mode and the bridge's start settings.

use super::output_map::{MAX_FOLLOWERS, MAX_OUTPUT_CHANNELS};

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
                note: "PipeWire's quantum is not readable (pw-metadata is missing or the server is not running). RigWerk still uses the ALSA period; start PipeWire to get the low-quantum path.".to_owned(),
            };
        }
        let low_latency = quantum <= PIPEWIRE_LOW_QUANTUM_MAX;
        let fits = alsa_period > 0 && alsa_period <= quantum && quantum.is_multiple_of(alsa_period);
        let note = if !low_latency {
            format!(
                "Desktop PipeWire graph is still at {quantum} frames ({:.1} ms at the graph's {:.1} kHz) — that is the shared desktop block, not the OUT CLOCK below. RigWerk marks this route as shared; low latency asks the graph for {} frames.",
                ms(quantum),
                rate as f32 / 1000.0,
                PIPEWIRE_LOW_QUANTUM
            )
        } else if alsa_period == 0 {
            format!(
                "PipeWire is running at a {quantum}-frame quantum ({:.1} ms). The ALSA period is not known yet; the bridge compares them as soon as the output opens.",
                ms(quantum)
            )
        } else if fits {
            if alsa_period == quantum {
                format!(
                    "ALSA period {alsa_period} frames matches the PipeWire quantum ({quantum} frames, {:.1} ms). Low latency path.",
                    ms(quantum)
                )
            } else {
                format!(
                    "ALSA period {alsa_period} frames fits {}× into the PipeWire quantum ({quantum} frames, {:.1} ms). Low latency path.",
                    quantum / alsa_period,
                    ms(quantum)
                )
            }
        } else if alsa_period > quantum {
            format!(
                "The ALSA period ({alsa_period} frames) is larger than the PipeWire quantum ({quantum} frames), so the coarser ALSA block sets the real latency and the small quantum is wasted. Set the output buffer to {quantum} frames or lower, or raise the quantum."
            )
        } else {
            format!(
                "The ALSA period ({alsa_period} frames) does not divide the PipeWire quantum ({quantum} frames), so the two clocks do not line up and can xrun. Pick a period that divides {quantum} (for example 32 or 64 frames)."
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
    /// Block peak per clock-device channel, after its channel map, level,
    /// safety ceiling, metronome and mute.  The tick is included.  Channels
    /// the device does not have read 0.
    pub out_channels: [f32; MAX_OUTPUT_CHANNELS],
    /// Safety-limiter gain reduction on the clock device, dB.  0 when it
    /// is not limiting.
    pub out_reduction_db: f32,
    /// The clock device's real channel count (0 until its first block).
    pub output_channels: u32,
    /// How many followers the engine currently maps (≤ [`MAX_FOLLOWERS`]).
    #[allow(dead_code)] // Exposed for Live / Mixer follower summaries.
    pub follower_count: usize,
    /// Block peak per follower channel, after that follower's map, level,
    /// safety ceiling, metronome and mute.  The tick is included.
    pub follower_channels: [[f32; MAX_OUTPUT_CHANNELS]; MAX_FOLLOWERS],
    /// Safety-limiter gain reduction per follower, dB.  0 when not limiting.
    /// The virtual publish device is one of these slots.
    pub follower_reduction_db: [f32; MAX_FOLLOWERS],
    /// Each follower's real channel count (0 until published).
    pub follower_channel_counts: [u32; MAX_FOLLOWERS],
    /// Generic followers that failed to open or were lost, and are being
    /// retried.  PipeWire in-graph followers always read false.
    pub follower_missing: [bool; MAX_FOLLOWERS],
    /// Underruns of each generic follower's own ring since its stream
    /// opened (a reopen starts again from 0).
    pub follower_underruns: [u64; MAX_FOLLOWERS],
    /// Generic followers whose drift loop is pinned at its limit.
    pub follower_drift_at_limit: [bool; MAX_FOLLOWERS],
    pub underruns: u64,
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
    /// The bridge's own status line. The header pill follows [`EngineState`]
    /// and does not read this, so an error pill stays an error pill.
    #[allow(dead_code)] // Published for diagnostics; no UI consumer yet.
    pub status: String,
    /// The rate the input clock really delivers (0 until it is known).
    pub input_rate: u32,
    /// The block size the input driver really delivers, in samples (0 until
    /// the first block arrives).
    pub input_block_frames: u32,
    /// What the output side really got: its rate and the step it is fed in.
    pub output_rate: u32,
    pub output_step_frames: u32,
    /// One line on how the output runs, e.g. REAL-style or why it fell back.
    pub output_mode: String,
    /// Milliseconds from our playback node to the graph edge (device/filters),
    /// when PipeWire reports it. `None` on non-PipeWire outputs or before the
    /// first timing sample. Bluetooth A2DP often lands around 100–200 ms here.
    pub path_delay_ms: Option<f32>,
    /// True when input and output run in one PipeWire graph callback (the
    /// duplex node): there is no ring, cushion or drift correction.
    pub direct: bool,
    /// Metronome beat the mixer last published.  0 when it is stopped,
    /// otherwise `1..=clicks` of [`Self::metronome_rhythm`].  The Live page
    /// reads this.  0 after the bridge stops or the device is lost.
    pub metronome_beat: u8,
    /// Rhythm discriminant the published beat belongs to.  Written with
    /// the beat, so the strip does not pair a beat from the bar that is
    /// still playing with a signature that is waiting for the next click.
    pub metronome_rhythm: u8,
}

/// How the cushion between input and output (the ring buffer's fill target)
/// is sized.  It can change while the bridge runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CushionMode {
    /// Twice the input buffer, at least two output blocks; grows 256 samples
    /// per dropout and shrinks 64 after 4 calm seconds.
    Safe = 0,
    /// One input block plus one output block; grows and shrinks in smaller,
    /// quicker steps.
    LowLatency = 1,
    /// A fixed size in milliseconds that never grows on its own.
    Manual = 2,
}

impl CushionMode {
    pub(crate) fn from_u32(value: u32) -> Self {
        match value {
            1 => Self::LowLatency,
            2 => Self::Manual,
            _ => Self::Safe,
        }
    }
}

/// How the Windows WASAPI clock device opens.  Followers stay shared.
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
    /// Exclusive mode on the clock device only; other apps cannot play there.
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
            Self::RealStyle => "REAL-style",
            Self::Exclusive => "Exclusive",
        }
    }

    /// Whether this mode asks for a low-latency attempt (not plain Shared).
    pub fn wants_low_latency(self) -> bool {
        !matches!(self, Self::Shared)
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
    /// Output side buffer in frames, used whenever the output is not running
    /// REAL-style or exclusive (ASIO playback, or a WASAPI driver that refused).
    pub output_buffer: usize,
    /// A wrapper driver (ASIO4ALL and friends) does not name its hardware, so
    /// playback errors get an extra hint about sharing one sound card.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub asio_wrapper: bool,
    /// REAL-style output: open WASAPI outputs through IAudioClient3 at the
    /// smallest engine step the driver allows.  If the driver refuses, the
    /// output falls back to a normal shared stream with `output_buffer`.
    /// Ignored when [`Self::exclusive_output`] is set.
    pub real_style_output: bool,
    /// WASAPI exclusive mode for the clock device only.  On refuse, falls back
    /// to shared cpal with `output_buffer` (not REAL-style).
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub exclusive_output: bool,
    /// Linux: ask PipeWire for its low quantum while the bridge runs.  The
    /// quantum is released as soon as the bridge stops.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub pipewire_low_latency: bool,
}
