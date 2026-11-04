//! Opening the playback side: REAL-style WASAPI first when asked, otherwise
//! a normal cpal stream, both fed by the shared mixer.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, FromSample, Sample, SizedSample};

use super::devices::pick_output_config;
use super::mixer::OutputMixer;
#[cfg(target_os = "windows")]
use super::real_output;
use super::shared::Shared;

/// What `open_output` hands back: the live stream plus the rate and channel
/// count it really runs at.
pub(crate) struct OpenedOutput {
    _stream: OutputStream,
    pub(crate) rate: u32,
    pub(crate) channels: usize,
}

/// Either a cpal stream or RigWerk's own REAL-style WASAPI stream.  Both
/// keep playing until they are dropped.
#[allow(dead_code)]
pub(crate) enum OutputStream {
    Cpal(cpal::Stream),
    #[cfg(target_os = "windows")]
    Real(real_output::RealOutput),
}

/// Opens the playback side.  With `real_style` set, a WASAPI output is first
/// opened through IAudioClient3 at the smallest engine step its driver allows,
/// which shortens the processing period of the whole device the way the REAL
/// tool does.  If the driver refuses, the output opens exactly as before: a
/// shared stream on whichever host owns the device, asking for `buffer_frames`.
pub(crate) fn open_output(
    output_id: &str,
    sample_rate: u32,
    shared: &Arc<Shared>,
    buffer_frames: usize,
    real_style: bool,
) -> Result<OpenedOutput, String> {
    let id: cpal::DeviceId = output_id
        .parse()
        .map_err(|error| format!("Output device id: {error}"))?;

    // Only the Windows REAL-style attempt can fill this in.
    #[cfg_attr(not(target_os = "windows"), allow(unused_mut))]
    let mut refused: Option<String> = None;
    #[cfg(target_os = "windows")]
    if real_style {
        if id.host() == cpal::HostId::Wasapi {
            match real_output::open(id.id(), sample_rate, Arc::clone(shared)) {
                Ok((stream, info)) => {
                    shared.set_output(info.rate, info.step_frames, info.describe(sample_rate));
                    return Ok(OpenedOutput {
                        _stream: OutputStream::Real(stream),
                        rate: info.rate,
                        channels: info.channels,
                    });
                }
                Err(reason) => refused = Some(reason),
            }
        } else {
            refused = Some(format!("{} has no adjustable engine step", id.host()));
        }
    }
    #[cfg(not(target_os = "windows"))]
    let _ = real_style;
    // Linux handles REAL-style as an ALSA period request, so the Windows
    // refusal reason is not part of its mode text.
    #[cfg(target_os = "linux")]
    let _ = &refused;

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
        Some(reason) => format!("Standard output{note} · REAL-style refused: {reason}"),
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

/// The smallest period ALSA allows, with a floor so a driver that reports a
/// one-frame minimum cannot turn the bridge into a stream of xruns.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn smallest_output_buffer(config: &cpal::SupportedStreamConfig, floor: usize) -> usize {
    match config.buffer_size() {
        cpal::SupportedBufferSize::Range { min, max } => {
            (*min as usize).max(floor).min(*max as usize)
        }
        cpal::SupportedBufferSize::Unknown => floor,
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

/// Writes one frame of tap values into an interleaved device frame.  Taps 1-4
/// go to the first four channels; any further channels get the L/R average.
pub(crate) fn write_frame<T: Sample + FromSample<f32>>(frame: &mut [T], out: [f32; 4]) {
    let rest = (out[0] + out[1]) * 0.5;
    for (channel, sample) in frame.iter_mut().enumerate() {
        *sample = T::from_sample(out.get(channel).copied().unwrap_or(rest));
    }
}

#[cfg(test)]
mod tests {
    use super::smallest_output_buffer;

    fn config(min: u32, max: u32) -> cpal::SupportedStreamConfig {
        cpal::SupportedStreamConfig::new(
            2,
            48_000,
            cpal::SupportedBufferSize::Range { min, max },
            cpal::SampleFormat::F32,
        )
    }

    #[test]
    fn the_smallest_period_respects_a_sane_floor() {
        assert_eq!(smallest_output_buffer(&config(16, 4096), 32), 32);
        assert_eq!(smallest_output_buffer(&config(128, 4096), 32), 128);
        assert_eq!(smallest_output_buffer(&config(16, 24), 32), 24);
    }
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
    let mut mixer = OutputMixer::new(config.sample_rate);

    let stream = device
        .build_output_stream(
            config,
            move |data: &mut [T], _info: &cpal::OutputCallbackInfo| {
                let frames = data.len() / channels;
                // The block the host hands over is the step the device really
                // runs at, whatever buffer was asked for.
                mix_shared
                    .output_step_frames
                    .store(frames as u32, Ordering::Relaxed);
                mixer.render(&mix_shared, frames, |index, out| {
                    write_frame(&mut data[index * channels..(index + 1) * channels], out);
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
