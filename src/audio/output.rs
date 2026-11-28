//! The platform's own clock output opens first, when it has one.  A
//! refusal falls back to the shared cpal stream, retried once at the
//! driver's default.  On Windows, exclusive falls back to shared cpal
//! with `buffer_frames`, not REAL-style.

use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, FromSample, Sample, SizedSample};

use super::devices::pick_output_config;
#[cfg(target_os = "linux")]
use super::linux::alsa;
use super::mixer::OutputMixer;
use super::output_map::DeviceFrame;
use super::panic_guard::contain;
use super::record::{push_device_frame, RecordSource};
use super::shared::{CallbackFailure, Shared};
use super::thread_priority::PromoteOnce;
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
use super::types::OutputMode;
#[cfg(target_os = "windows")]
use super::win::output as win_output;
#[cfg(target_os = "windows")]
use super::win::wasapi;

pub(crate) struct OpenedOutput {
    pub(crate) _stream: OutputStream,
    pub(crate) rate: u32,
    pub(crate) channels: usize,
}

/// Both keep playing until dropped.  The payloads are unread: dropping the
/// value is what stops the stream.
pub(crate) enum OutputStream {
    Cpal {
        _stream: cpal::Stream,
    },
    #[cfg(target_os = "windows")]
    Real {
        _stream: wasapi::RealOutput,
    },
    #[cfg(target_os = "windows")]
    Exclusive {
        _stream: wasapi::ExclusiveOutput,
    },
}

pub(crate) fn open_output(
    output_id: &str,
    sample_rate: u32,
    shared: &Arc<Shared>,
    buffer_frames: usize,
    low_latency: bool,
    exclusive: bool,
) -> Result<OpenedOutput, String> {
    let id: cpal::DeviceId = output_id
        .parse()
        .map_err(|error| format!("Output device id: {error}"))?;

    // A platform clock output opens first and reports what it fell back from.
    #[cfg(target_os = "windows")]
    let refusal = match win_output::attempt(&id, sample_rate, shared, low_latency, exclusive) {
        win_output::Attempt::Opened(opened) => return Ok(opened),
        win_output::Attempt::Refused(reason) => reason,
    };
    #[cfg(not(target_os = "windows"))]
    let _ = exclusive;
    // No clock output and no period of its own: the shared open below.
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    let _ = low_latency;

    let host = cpal::host_from_id(id.host()).map_err(|error| format!("Output host: {error}"))?;
    let device = host
        .device_by_id(&id)
        .ok_or_else(|| "The selected output disappeared".to_owned())?;
    let config = pick_output_config(&device, sample_rate)?;
    let channels = config.channels() as usize;
    let rate = config.sample_rate();
    // A refused request is that platform's answer, so it owns the mode line.
    #[cfg(target_os = "linux")]
    let requested = alsa::requested_output_buffer(&config, buffer_frames, low_latency);
    #[cfg(not(target_os = "linux"))]
    let requested = clamp_output_buffer(&config, buffer_frames);
    #[cfg(target_os = "linux")]
    let describe = |fixed_buffer: bool| alsa::describe(low_latency, fixed_buffer);
    #[cfg(target_os = "windows")]
    let describe = |fixed_buffer: bool| win_output::describe(refusal.as_deref(), fixed_buffer);
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    let describe = |_: bool| (OutputMode::Shared, "Standard output".to_owned());
    let stream = match make_output_stream(
        &device,
        config,
        channels,
        Arc::clone(shared),
        Some(requested),
    ) {
        Ok(stream) => {
            let (mode, line) = describe(true);
            shared.set_output(rate, requested as u32, mode, line);
            stream
        }
        Err(error) => {
            // A driver that cannot give a fixed buffer still plays.  The
            // request simply does not apply.
            let (mode, line) = describe(false);
            shared.set_output(rate, 0, mode, line);
            make_output_stream(&device, config, channels, Arc::clone(shared), None).map_err(
                |fallback| {
                    explain_output_error(format!("{error}; fallback failed too: {fallback}"))
                },
            )?
        }
    };
    stream
        .play()
        .map_err(|error| format!("Starting output: {error}"))?;
    Ok(OpenedOutput {
        _stream: OutputStream::Cpal { _stream: stream },
        rate,
        channels,
    })
}

pub(crate) const FIXED_BUFFER_REFUSED: &str = " · fixed buffer refused, driver default";

/// A Windows "device in use" answer becomes a sentence.  Every other
/// error is passed through.
pub(crate) fn explain_output_error(error: String) -> String {
    #[cfg(target_os = "windows")]
    if let Some(explained) = win_output::explain_device_in_use(&error) {
        return explained.to_owned();
    }
    error
}

pub(crate) fn clamp_output_buffer(config: &cpal::SupportedStreamConfig, frames: usize) -> usize {
    match config.buffer_size() {
        cpal::SupportedBufferSize::Range { min, max } => {
            (frames as cpal::FrameCount).clamp(*min, *max) as usize
        }
        cpal::SupportedBufferSize::Unknown => frames,
    }
}

/// `None` for a partial frame or no channels.  That is the chain's
/// failure, not the device's, so it stops the run on an extra output too.
/// An extra output that goes away does not.
pub(crate) fn block_frames(samples: usize, channels: usize) -> Option<usize> {
    if channels == 0 {
        return None;
    }
    let frames = samples / channels;
    // Whole only when every sample belongs to a frame.
    (frames * channels == samples).then_some(frames)
}

pub(crate) fn make_output_stream(
    device: &cpal::Device,
    config: cpal::SupportedStreamConfig,
    channels: usize,
    shared: Arc<Shared>,
    requested_buffer: Option<usize>,
) -> Result<cpal::Stream, String> {
    let mut stream_config = config.config();
    if let Some(frames) = requested_buffer {
        stream_config.buffer_size = BufferSize::Fixed(frames as cpal::FrameCount);
    }
    match config.sample_format() {
        cpal::SampleFormat::F32 => build_output::<f32>(device, stream_config, channels, shared),
        cpal::SampleFormat::I16 => build_output::<i16>(device, stream_config, channels, shared),
        cpal::SampleFormat::I32 => build_output::<i32>(device, stream_config, channels, shared),
        cpal::SampleFormat::I8 => build_output::<i8>(device, stream_config, channels, shared),
        cpal::SampleFormat::U8 => build_output::<u8>(device, stream_config, channels, shared),
        cpal::SampleFormat::U16 => build_output::<u16>(device, stream_config, channels, shared),
        cpal::SampleFormat::U32 => build_output::<u32>(device, stream_config, channels, shared),
        cpal::SampleFormat::F64 => build_output::<f64>(device, stream_config, channels, shared),
        cpal::SampleFormat::I64 => build_output::<i64>(device, stream_config, channels, shared),
        cpal::SampleFormat::U64 => build_output::<u64>(device, stream_config, channels, shared),
        other => Err(format!("Unsupported output sample format: {other:?}")),
    }
}

/// A caught panic silences the block.  A stream that cannot be trusted
/// plays nothing rather than a half-written mix.
pub(crate) fn render_or_silence<T: Sample>(
    shared: &Shared,
    block: &mut [T],
    render: impl FnOnce(&mut [T]),
) {
    if contain(|| render(block)) {
        block.fill(T::EQUILIBRIUM);
        shared.note_callback_failure(CallbackFailure::Panic);
    }
}

/// Conversion only.  The mixer already applied the map, level and mute.
pub(crate) fn write_frame<T: Sample + FromSample<f32>>(frame: &mut [T], mapped: &DeviceFrame) {
    for (channel, sample) in frame.iter_mut().enumerate() {
        *sample = T::from_sample(mapped.sample(channel));
    }
}

/// A default-buffer host hands over at most a few thousand frames.
const PREPARED_BLOCK: usize = 8192;

/// A block larger than the scratch is rendered in several calls, so the
/// callback does not grow it.
pub(crate) fn block_chunks(frames: usize, limit: usize) -> impl Iterator<Item = (usize, usize)> {
    (0..frames)
        .step_by(limit)
        .map(move |offset| (offset, (frames - offset).min(limit)))
}

pub(crate) fn build_output<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    channels: usize,
    shared: Arc<Shared>,
) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32>,
{
    let mix_shared = Arc::clone(&shared);
    let error_shared = Arc::clone(&shared);
    let channels = channels.max(1);
    let mut promote = PromoteOnce::default();
    let mut mixer = OutputMixer::new(config.sample_rate, channels);
    // The first callback must not build the kernel.
    mixer.configure_source_rate(&shared);
    mixer.publish_device_channels(&shared);
    // cpal reports the block size in the callback.  Size the scratch for the
    // request, or for a generous one when the host picks.
    mixer.prepare(match config.buffer_size {
        cpal::BufferSize::Fixed(frames) => (frames as usize).max(PREPARED_BLOCK),
        cpal::BufferSize::Default => PREPARED_BLOCK,
    });

    let stream = device
        .build_output_stream(
            config,
            move |data: &mut [T], _info: &cpal::OutputCallbackInfo| {
                // cpal's stream thread starts at normal priority on Linux.
                promote.run();
                let Some(frames) = block_frames(data.len(), channels) else {
                    // A partial frame has no whole-frame count.  The sample
                    // path failed, not the device, so this stops the run.
                    data.fill(T::EQUILIBRIUM);
                    mix_shared.note_callback_failure(CallbackFailure::BadBlock);
                    return;
                };
                if mix_shared.callback_failure().is_some() {
                    // The chain must not be driven again.
                    data.fill(T::EQUILIBRIUM);
                    return;
                }
                // The host block is the step the device really runs, whatever
                // buffer was asked for.
                mix_shared.note_output_step(frames as u32);
                // The floor follows the whole host block, also when it is
                // rendered in prepared-size pieces.
                mixer.note_host_block(&mix_shared, frames);
                render_or_silence(&mix_shared, data, |block| {
                    for (offset, count) in block_chunks(frames, PREPARED_BLOCK) {
                        mixer.render_chunk(&mix_shared, count, |index, mapped| {
                            // The take records the mapped frame, duck included.
                            if channels > 0 {
                                push_device_frame(
                                    &mix_shared.recorder,
                                    RecordSource::Clock,
                                    mapped,
                                    channels,
                                );
                            }
                            let at = offset + index;
                            write_frame(&mut block[at * channels..(at + 1) * channels], mapped);
                        });
                    }
                });
            },
            move |error| {
                if matches!(
                    error.kind(),
                    cpal::ErrorKind::DeviceNotAvailable | cpal::ErrorKind::StreamInvalidated
                ) {
                    error_shared.set_device_lost(format!("Output device disconnected: {error}"));
                }
            },
            None,
        )
        .map_err(|error| format!("Building output stream: {error}"))?;
    Ok(stream)
}

#[cfg(test)]
mod tests;
