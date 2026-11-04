//! The public vocabulary of the audio side: devices, inventory, engine
//! state, live snapshot, cushion mode and the bridge's start settings.

pub const DEMO_ASIO_ID: &str = "demo:asio";
pub const DEMO_INPUT_ID: &str = "demo:input";
pub const DEMO_OUTPUT_ID: &str = "demo:output";

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
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub notes: Vec<String>,
    pub os: String,
    /// What the dedicated low-latency input is called on this platform.
    /// "ASIO" on Windows, "JACK" on Linux.
    pub low_latency_label: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineState {
    Idle,
    Preview,
    Starting,
    Running,
    Error,
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub in_l: f32,
    pub in_r: f32,
    pub out_l: f32,
    pub out_r: f32,
    pub underruns: u64,
    pub frames: u64,
    pub ring_fill: f32,
    pub target_frames: usize,
    pub drift_ppm: f32,
    pub error: Option<String>,
    pub device_lost: bool,
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
    /// REAL-style (ASIO playback, or a WASAPI driver that refused).
    pub output_buffer: usize,
    /// A wrapper driver (ASIO4ALL and friends) does not name its hardware, so
    /// playback errors get an extra hint about sharing one sound card.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub asio_wrapper: bool,
    /// REAL-style output: open WASAPI outputs through IAudioClient3 at the
    /// smallest engine step the driver allows.  If the driver refuses, the
    /// output falls back to a normal shared stream with `output_buffer`.
    pub real_style_output: bool,
}
