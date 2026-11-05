//! Opening the capture side through cpal and pushing its frames into the
//! ring.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{BufferSize, FromSample, Sample, SizedSample};

use super::denormal::DenormalGuard;
use super::shared::Shared;
use super::types::StartConfig;

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

    let stream = device
        .build_input_stream(
            config,
            move |data: &[T], _info: &cpal::InputCallbackInfo| {
                let _denormals = DenormalGuard::new();
                // The ring has to hold at least one whole input block, or
                // the output runs dry between two deliveries.
                gain_shared.note_input_block(data.len() / channels);
                gain_shared.note_input_frames(data.len() / channels);
                let gain = gain_shared.input_gain();
                let enabled_l = gain_shared.input_enabled[0].load(Ordering::Relaxed);
                let enabled_r = gain_shared.input_enabled[1].load(Ordering::Relaxed);
                let mut peak_l = 0.0f32;
                let mut peak_r = 0.0f32;
                for frame in data.chunks(channels) {
                    let mut left = f32::from_sample(frame[0]) * gain;
                    let mut right = if channels > 1 {
                        f32::from_sample(frame[1]) * gain
                    } else {
                        left
                    };
                    if !enabled_l {
                        left = 0.0;
                    }
                    if !enabled_r {
                        right = 0.0;
                    }
                    peak_l = peak_l.max(left.abs());
                    peak_r = peak_r.max(right.abs());
                    gain_shared.push_frame(left, right);
                }
                gain_shared.in_l.store(peak_l.to_bits(), Ordering::Relaxed);
                gain_shared.in_r.store(peak_r.to_bits(), Ordering::Relaxed);
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
