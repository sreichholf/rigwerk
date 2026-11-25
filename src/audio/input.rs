//! Opening the capture side through cpal and pushing its frames into the
//! ring.  The capture arithmetic the four input backends share is
//! [`InputStage`]; each backend keeps its own loop, sink and mono rule.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{BufferSize, FromSample, Sample, SizedSample};

use super::denormal::DenormalGuard;
use super::shared::Shared;
use super::thread_priority::PromoteOnce;
use super::types::StartConfig;

/// One input callback's capture arithmetic: the raw frame through the
/// measurement, the gain, the tuner tap, the channel enable and the peak.
/// Every input backend runs this same order, so it lives here once; the
/// loop around it, and what it pushes the result into, stay each backend's
/// own.
///
/// The order is load-bearing.  The tap is taken before the channel enable,
/// so a muted channel still reaches the tuner; taking it after the mute
/// silences the tuner while the audio still sounds right.  The scope is fed
/// the post-mute frame, so it correctly shows a muted channel as flat.
///
/// Deliberately not absorbed here, because the sites disagree on it:
/// - the mono rule: a source with one channel copies its only sample to
///   both sides before calling [`Self::frame`];
/// - the sink: [`Shared::push_frame`] into the bridge ring on cpal, ASIO and
///   `native::push_capture`, or [`Shared::push_scope_in`] plus the scratch
///   pair on the direct node, which never pushes the ring;
/// - [`Shared::note_input_block`], which the sites time differently: before
///   the loop, when the driver starts, or only while the node has a linked
///   input;
/// - the `has_input` gate the duplex node needs when its ports are unlinked;
/// - a calibration arm that changes mid-block: [`Shared::capture_calibration`]
///   re-reads it per frame, so latching it here would be a behaviour change.
///
/// No allocation, no lock, no logging: it runs on the audio thread.
pub(crate) struct InputStage {
    gain: f32,
    enabled_l: bool,
    enabled_r: bool,
    peak_l: f32,
    peak_r: f32,
}

impl InputStage {
    /// Hoists the per-block settings: the input gain and the two channel
    /// enables.  Call it once per callback, before the first frame.
    pub(crate) fn new(shared: &Shared) -> Self {
        Self {
            gain: shared.input_gain(),
            enabled_l: shared.input_enabled[0].load(Ordering::Relaxed),
            enabled_r: shared.input_enabled[1].load(Ordering::Relaxed),
            peak_l: 0.0,
            peak_r: 0.0,
        }
    }

    /// One raw frame, in the order every capture callback uses, returning
    /// what the caller publishes (the frame after the channel enable).
    pub(crate) fn frame(&mut self, shared: &Shared, raw_l: f32, raw_r: f32) -> (f32, f32) {
        // A calibration capture is raw, so a hot input gain cannot be
        // mistaken for the device clipping.
        shared.capture_calibration(raw_l, raw_r);
        let mut left = raw_l * self.gain;
        let mut right = raw_r * self.gain;
        shared.tap_frame(left, right);
        if !self.enabled_l {
            left = 0.0;
        }
        if !self.enabled_r {
            right = 0.0;
        }
        self.peak_l = self.peak_l.max(left.abs());
        self.peak_r = self.peak_r.max(right.abs());
        (left, right)
    }

    /// Writes this block's peaks.  Once per callback, after the last frame.
    pub(crate) fn publish(&self, shared: &Shared) {
        shared.in_l.store(self.peak_l.to_bits(), Ordering::Relaxed);
        shared.in_r.store(self.peak_r.to_bits(), Ordering::Relaxed);
    }
}

pub(crate) fn open_input_device(config: &StartConfig) -> Result<cpal::Device, String> {
    let input_id: cpal::DeviceId = config
        .input_id
        .as_deref()
        .ok_or_else(|| "Choose an input device".to_owned())?
        .parse()
        .map_err(|error| format!("Input device id: {error}"))?;
    let input_host =
        cpal::host_from_id(input_id.host()).map_err(|error| format!("Input host: {error}"))?;
    input_host
        .device_by_id(&input_id)
        .ok_or_else(|| "The selected input disappeared".to_owned())
}

/// Opens the capture stream, asking for the input-side buffer first and the
/// host's default if the device will not take a fixed size.
pub(crate) fn make_input_stream(
    device: &cpal::Device,
    config: cpal::SupportedStreamConfig,
    channels: usize,
    shared: Arc<Shared>,
    buffer_frames: usize,
) -> Result<cpal::Stream, String> {
    let requested = match config.buffer_size() {
        cpal::SupportedBufferSize::Range { min, max } => {
            (buffer_frames as cpal::FrameCount).clamp(*min, *max)
        }
        cpal::SupportedBufferSize::Unknown => buffer_frames as cpal::FrameCount,
    };
    let mut fixed = config.config();
    fixed.buffer_size = BufferSize::Fixed(requested);
    match build_input_for(device, &config, fixed, channels, Arc::clone(&shared)) {
        Ok(stream) => Ok(stream),
        Err(_) => build_input_for(device, &config, config.config(), channels, shared),
    }
}

pub(crate) fn build_input_for(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    stream_config: cpal::StreamConfig,
    channels: usize,
    shared: Arc<Shared>,
) -> Result<cpal::Stream, String> {
    match config.sample_format() {
        cpal::SampleFormat::F32 => build_input::<f32>(device, stream_config, channels, shared),
        cpal::SampleFormat::I16 => build_input::<i16>(device, stream_config, channels, shared),
        cpal::SampleFormat::I32 => build_input::<i32>(device, stream_config, channels, shared),
        cpal::SampleFormat::I8 => build_input::<i8>(device, stream_config, channels, shared),
        cpal::SampleFormat::U8 => build_input::<u8>(device, stream_config, channels, shared),
        cpal::SampleFormat::U16 => build_input::<u16>(device, stream_config, channels, shared),
        cpal::SampleFormat::U32 => build_input::<u32>(device, stream_config, channels, shared),
        cpal::SampleFormat::F64 => build_input::<f64>(device, stream_config, channels, shared),
        cpal::SampleFormat::I64 => build_input::<i64>(device, stream_config, channels, shared),
        cpal::SampleFormat::U64 => build_input::<u64>(device, stream_config, channels, shared),
        other => Err(format!("Unsupported input sample format: {other:?}")),
    }
}

pub(crate) fn build_input<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    channels: usize,
    shared: Arc<Shared>,
) -> Result<cpal::Stream, String>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let gain_shared = Arc::clone(&shared);
    let error_shared = Arc::clone(&shared);
    let channels = channels.max(1);
    let mut promote = PromoteOnce::default();

    let stream = device
        .build_input_stream(
            config,
            move |data: &[T], _info: &cpal::InputCallbackInfo| {
                // cpal's stream thread starts at normal priority on Linux.
                promote.run();
                let _denormals = DenormalGuard::new();
                // Nothing in this callback may allocate (see `rt_guard`).
                let _no_allocations = super::rt_guard::RtGuard::enter();
                // The ring has to hold at least one whole input block, or
                // the output drops out between two deliveries.
                gain_shared.note_input_block(data.len() / channels);
                gain_shared.note_input_frames(data.len() / channels);
                let mut stage = InputStage::new(&gain_shared);
                for frame in data.chunks(channels) {
                    let raw_left = f32::from_sample(frame[0]);
                    let raw_right = if channels > 1 {
                        f32::from_sample(frame[1])
                    } else {
                        raw_left
                    };
                    let (left, right) = stage.frame(&gain_shared, raw_left, raw_right);
                    gain_shared.push_frame(left, right);
                }
                stage.publish(&gain_shared);
            },
            move |error| {
                if matches!(
                    error.kind(),
                    cpal::ErrorKind::DeviceNotAvailable | cpal::ErrorKind::StreamInvalidated
                ) {
                    error_shared.set_device_lost(format!("Input device disconnected: {error}"));
                }
            },
            None,
        )
        .map_err(|error| format!("Building input stream: {error}"))?;
    Ok(stream)
}

#[cfg(test)]
mod tests;
