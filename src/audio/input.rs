//! [`InputStage`] is the capture order every backend shares.  The tuner
//! tap is taken before the channel mute, so a muted channel still reaches
//! the tuner, and the scope sees the post-mute frame.
//!
//! The mono copy, the sink, and when the block size is noted stay in each
//! backend: the sites disagree.  A calibration arm is re-read per frame,
//! so it is not latched here.  No allocation, no lock.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{BufferSize, FromSample, Sample, SizedSample};

use super::denormal::DenormalGuard;
use super::panic_guard::contain;
use super::shared::{CallbackFailure, Shared};
use super::thread_priority::PromoteOnce;
use super::types::StartConfig;

pub(crate) struct InputStage {
    gain: f32,
    enabled_l: bool,
    enabled_r: bool,
    peak_l: f32,
    peak_r: f32,
}

impl InputStage {
    /// Once per callback, before the first frame.  The gain and the channel
    /// enables are latched for the block.
    pub(crate) fn new(shared: &Shared) -> Self {
        Self {
            gain: shared.input_gain(),
            enabled_l: shared.input_enabled[0].load(Ordering::Relaxed),
            enabled_r: shared.input_enabled[1].load(Ordering::Relaxed),
            peak_l: 0.0,
            peak_r: 0.0,
        }
    }

    pub(crate) fn frame(&mut self, shared: &Shared, raw_l: f32, raw_r: f32) -> (f32, f32) {
        // Raw, so a hot input gain is not mistaken for the device clipping.
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

    pub(crate) fn publish(&self, shared: &Shared) {
        shared.in_l.store(self.peak_l.to_bits(), Ordering::Relaxed);
        shared.in_r.store(self.peak_r.to_bits(), Ordering::Relaxed);
    }
}

/// A trailing chunk shorter than `channels` is not a frame, and a zero
/// channel count has none.
pub(crate) fn whole_frames(samples: usize, channels: usize) -> usize {
    samples.checked_div(channels).unwrap_or(0)
}

/// A caught panic leaves the counters and the tap untouched, so a block
/// that did not play is not counted as delivered.
pub(crate) fn input_block<T>(data: &[T], channels: usize, shared: &Shared)
where
    T: Sample,
    f32: FromSample<T>,
{
    let frames = whole_frames(data.len(), channels);
    let panicked = contain(|| {
        let mut stage = InputStage::new(shared);
        for frame in data.chunks_exact(channels) {
            let raw_left = f32::from_sample(frame[0]);
            let raw_right = if channels > 1 {
                f32::from_sample(frame[1])
            } else {
                raw_left
            };
            let (left, right) = stage.frame(shared, raw_left, raw_right);
            shared.push_frame(left, right);
        }
        stage.publish(shared);
        // A panicked body put nothing in the ring, so this block did not arrive.
        shared.note_input_block(frames);
        shared.note_input_frames(frames);
    });
    if panicked {
        // No output buffer to silence.  The flag is what stops the run.
        shared.note_callback_failure(CallbackFailure::Panic);
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

/// The host's default if the device will not take a fixed size.
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
                let _no_allocations = super::rt_guard::RtGuard::enter();
                if gain_shared.callback_failure().is_some() {
                    // The chain must not be driven again.
                    return;
                }
                input_block(data, channels, &gain_shared);
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
