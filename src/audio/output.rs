//! Opening the playback side: exclusive or REAL-style WASAPI when asked,
//! otherwise a normal cpal stream, both fed by the shared mixer.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, FromSample, Sample, SizedSample};

use super::devices::pick_output_config;
#[cfg(target_os = "linux")]
use super::linux::alsa::smallest_output_buffer;
use super::mixer::OutputMixer;
use super::output_map::DeviceFrame;
use super::record::{push_device_frame, RecordSource};
use super::shared::Shared;
use super::thread_priority::PromoteOnce;
#[cfg(target_os = "windows")]
use super::win::wasapi;

#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug)]
enum RefuseKind {
    Exclusive,
    RealStyle,
}

/// What `open_output` hands back: the live stream plus the rate and channel
/// count it really runs at.
pub(crate) struct OpenedOutput {
    _stream: OutputStream,
    pub(crate) rate: u32,
    pub(crate) channels: usize,
}

/// Either a cpal stream or RigWerk's own WASAPI stream (REAL-style or
/// exclusive).  Both keep playing until they are dropped.
#[allow(dead_code)]
pub(crate) enum OutputStream {
    Cpal(cpal::Stream),
    #[cfg(target_os = "windows")]
    Real(wasapi::RealOutput),
    #[cfg(target_os = "windows")]
    Exclusive(wasapi::ExclusiveOutput),
}

/// Opens the playback side.
///
/// On Windows, `exclusive` tries WASAPI exclusive mode first (clock device
/// only).  On refuse it falls back to shared cpal with `buffer_frames` — not
/// REAL-style.  `real_style` opens IAudioClient3 at the driver's smallest
/// shared engine step; on refuse it also falls back to shared cpal.
pub(crate) fn open_output(
    output_id: &str,
    sample_rate: u32,
    shared: &Arc<Shared>,
    buffer_frames: usize,
    real_style: bool,
    exclusive: bool,
) -> Result<OpenedOutput, String> {
    let id: cpal::DeviceId = output_id
        .parse()
        .map_err(|error| format!("Output device id: {error}"))?;

    // Only the Windows exclusive / REAL-style attempts can fill this in.
    #[cfg(target_os = "windows")]
    let mut refused: Option<(RefuseKind, String)> = None;
    #[cfg(target_os = "windows")]
    if exclusive {
        if id.host() == cpal::HostId::Wasapi {
            match wasapi::open_exclusive(id.id(), sample_rate, Arc::clone(shared)) {
                Ok((stream, info)) => {
                    shared.set_output(info.rate, info.step_frames, info.describe(sample_rate));
                    return Ok(OpenedOutput {
                        _stream: OutputStream::Exclusive(stream),
                        rate: info.rate,
                        channels: info.channels,
                    });
                }
                Err(reason) => refused = Some((RefuseKind::Exclusive, reason)),
            }
        } else {
            refused = Some((
                RefuseKind::Exclusive,
                format!("{} has no exclusive mode", id.host()),
            ));
        }
    } else if real_style {
        if id.host() == cpal::HostId::Wasapi {
            match wasapi::open(id.id(), sample_rate, Arc::clone(shared)) {
                Ok((stream, info)) => {
                    shared.set_output(info.rate, info.step_frames, info.describe(sample_rate));
                    return Ok(OpenedOutput {
                        _stream: OutputStream::Real(stream),
                        rate: info.rate,
                        channels: info.channels,
                    });
                }
                Err(reason) => refused = Some((RefuseKind::RealStyle, reason)),
            }
        } else {
            refused = Some((
                RefuseKind::RealStyle,
                format!("{} has no adjustable engine step", id.host()),
            ));
        }
    }
    #[cfg(not(target_os = "windows"))]
    let _ = (real_style, exclusive);

    let host = cpal::host_from_id(id.host()).map_err(|error| format!("Output host: {error}"))?;
    let device = host
        .device_by_id(&id)
        .ok_or_else(|| "The selected output disappeared".to_owned())?;
    let config = pick_output_config(&device, sample_rate)?;
    let channels = config.channels() as usize;
    let rate = config.sample_rate();
    // On Linux the same switch asks ALSA for its smallest usable period
    // instead of the manual output buffer.  Other platforms keep the buffer.
    #[cfg(target_os = "linux")]
    let requested = if real_style {
        smallest_output_buffer(&config, 32)
    } else {
        clamp_output_buffer(&config, buffer_frames)
    };
    #[cfg(not(target_os = "linux"))]
    let requested = clamp_output_buffer(&config, buffer_frames);
    #[cfg(target_os = "linux")]
    let mode = |note: &str| {
        if real_style {
            format!("Low latency · ALSA smallest period{note}")
        } else {
            format!("Standard output{note}")
        }
    };
    #[cfg(not(target_os = "linux"))]
    let mode = |note: &str| match &refused {
        Some((RefuseKind::Exclusive, reason)) => {
            format!("Standard output{note} · Exclusive refused: {reason}")
        }
        Some((RefuseKind::RealStyle, reason)) => {
            format!("Standard output{note} · REAL-style refused: {reason}")
        }
        None => format!("Standard output{note}"),
    };
    let stream = match make_output_stream(
        &device,
        config,
        channels,
        Arc::clone(shared),
        Some(requested),
    ) {
        Ok(stream) => {
            shared.set_output(rate, requested as u32, mode(""));
            stream
        }
        Err(error) => {
            // A host or driver that cannot give us a fixed small buffer still
            // gets to play; the buffer request simply does not apply.
            shared.set_output(rate, 0, mode(" · fixed buffer refused, driver default"));
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
        _stream: OutputStream::Cpal(stream),
        rate,
        channels,
    })
}

/// Windows' "device in use" answer (AUDCLNT_E_DEVICE_IN_USE, 0x8889000A) as
/// cpal prints it.  Measured: with ASIO4ALL as the input, the Realtek card it
/// drives refused every shared output stream with exactly this code.
pub(crate) const DEVICE_IN_USE: &str = "-2004287478";

pub(crate) fn explain_output_error(error: String) -> String {
    if error.contains(DEVICE_IN_USE) {
        "This output is in use: another driver or app has taken the sound card for itself"
            .to_owned()
    } else {
        error
    }
}

pub(crate) fn clamp_output_buffer(config: &cpal::SupportedStreamConfig, frames: usize) -> usize {
    match config.buffer_size() {
        cpal::SupportedBufferSize::Range { min, max } => {
            (frames as cpal::FrameCount).clamp(*min, *max) as usize
        }
        cpal::SupportedBufferSize::Unknown => frames,
    }
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

/// Copies a mapped device frame into one interleaved frame of the device
/// buffer.  The channel map, level and mute were applied by the mixer's
/// [`DeviceMapper`](super::output_map::DeviceMapper); this only converts.
pub(crate) fn write_frame<T: Sample + FromSample<f32>>(frame: &mut [T], mapped: &DeviceFrame) {
    for (channel, sample) in frame.iter_mut().enumerate() {
        *sample = T::from_sample(mapped.sample(channel));
    }
}

/// The block size the cpal output's scratch is sized for up front.  Hosts with
/// a default buffer hand over at most a few thousand frames.
const PREPARED_BLOCK: usize = 8192;

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
    // Design the filter before the stream starts: the first callback must not
    // spend a millisecond building the kernel.
    mixer.follow_source_rate(&shared);
    mixer.publish_device_channels(&shared);
    // cpal only knows the block size once it calls back; size the scratch for
    // the requested buffer or, with the host's default, for a generous one.
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
                let frames = data.len() / channels;
                // The block the host hands over is the step the device really
                // runs at, whatever buffer was asked for.
                mix_shared
                    .output_step_frames
                    .store(frames as u32, Ordering::Relaxed);
                mixer.render(&mix_shared, frames, |index, mapped| {
                    // The frame write_frame is about to convert: channel map,
                    // level, limiter, mute and the calibration duck.
                    if channels > 0 {
                        push_device_frame(
                            &mix_shared.recorder,
                            RecordSource::Clock,
                            mapped,
                            channels,
                        );
                    }
                    write_frame(&mut data[index * channels..(index + 1) * channels], mapped);
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
