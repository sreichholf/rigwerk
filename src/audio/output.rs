//! Opening the playback side: the platform's own clock output when it has one,
//! and the shared cpal stream fed by the mixer behind it.
//!
//! What each platform tries there lives in its own folder: `win/output` owns
//! the WASAPI exclusive and REAL-style attempt and Windows' "device in use"
//! answer, `linux/alsa` owns the period and the mode line Linux asks for.

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

/// What `open_output` hands back: the live stream plus the rate and channel
/// count it really runs at.
pub(crate) struct OpenedOutput {
    pub(crate) _stream: OutputStream,
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
/// On Windows, `exclusive` tries WASAPI exclusive mode first (clock output
/// only).  On refuse it falls back to shared cpal with `buffer_frames`, not
/// REAL-style.  `low_latency` asks the platform's clock output for the lowest
/// latency it can open: IAudioClient3 at the driver's smallest shared engine
/// step on Windows, ALSA's smallest usable period on Linux.  A refusal falls
/// back to the shared cpal stream (at `buffer_frames` on Windows, at ALSA's
/// requested period on Linux); when that stream does not open at the
/// requested buffer, it is retried once at the driver's default, and only a
/// second failure is an error.
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

    // A platform with a clock output of its own opens that first, and says
    // what it fell back from when the attempt is refused.
    #[cfg(target_os = "windows")]
    let refusal = match win_output::attempt(&id, sample_rate, shared, low_latency, exclusive) {
        win_output::Attempt::Opened(opened) => return Ok(opened),
        win_output::Attempt::Refused(reason) => reason,
    };
    #[cfg(not(target_os = "windows"))]
    let _ = exclusive;
    // A host with neither a clock output nor a period of its own keeps the
    // neutral open below.
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    let _ = low_latency;

    let host = cpal::host_from_id(id.host()).map_err(|error| format!("Output host: {error}"))?;
    let device = host
        .device_by_id(&id)
        .ok_or_else(|| "The selected output disappeared".to_owned())?;
    let config = pick_output_config(&device, sample_rate)?;
    let channels = config.channels() as usize;
    let rate = config.sample_rate();
    // What the shared stream asks for and the mode it then runs in: the
    // platform owns both, because a refused request is a platform's answer.
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
            // A host or driver that cannot give us a fixed small buffer still
            // gets to play; the buffer request simply does not apply.
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
        _stream: OutputStream::Cpal(stream),
        rate,
        channels,
    })
}

/// What the mode line carries when the fixed buffer was refused and the stream
/// runs at whatever the driver defaults to.
pub(crate) const FIXED_BUFFER_REFUSED: &str = " · fixed buffer refused, driver default";

/// Turns a Windows "device in use" answer into a sentence; every other error
/// is passed through as it came.
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

/// How many whole interleaved frames one output block holds.  `None` when the
/// block does not end on a frame boundary, or has no channels: a partial frame
/// has no whole-frame count, so the loop that writes
/// `data[index * channels..(index + 1) * channels]` would leave its tail
/// unwritten, and a zero channel count would divide by zero.  The caller
/// refuses such a block instead of playing a partly written buffer.
///
/// The refusal is the chain's failure, not the device's, so it stops the run on
/// an extra output too.  The rule that an extra output never stops the bridge
/// covers a device that goes away, not a bad block.
pub(crate) fn block_frames(samples: usize, channels: usize) -> Option<usize> {
    if channels == 0 {
        return None;
    }
    let frames = samples / channels;
    // No `%`: a block is whole only when every sample belongs to a frame.
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

/// Runs one interleaved output block's sample body.  A caught panic silences
/// the block and marks the run failed, so a stream that cannot be trusted
/// plays nothing rather than the half-written mix.  Both cpal output callbacks
/// go through this, and `output/tests` fails if the guard is removed here.
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

/// Splits one host block into prepared-size chunks, with the offset of each.
/// A block larger than the scratch is rendered in several calls instead of
/// growing it inside the callback.
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
                let Some(frames) = block_frames(data.len(), channels) else {
                    // A block with no channel or a partial frame cannot be
                    // written frame by frame: its whole-frame count is
                    // unknown and its tail would stay unwritten.  Silence it
                    // and publish the failure.  This stops the run even on an
                    // extra output: the sample path failed, not the device.
                    data.fill(T::EQUILIBRIUM);
                    mix_shared.note_callback_failure(CallbackFailure::BadBlock);
                    return;
                };
                if mix_shared.callback_failure().is_some() {
                    // A previous callback failed; the app is stopping this
                    // run, and the chain must not be driven again.
                    data.fill(T::EQUILIBRIUM);
                    return;
                }
                // The block the host hands over is the step the device really
                // runs at, whatever buffer was asked for.
                mix_shared.note_output_step(frames as u32);
                // The bridge buffer floor follows the whole host block, also
                // when it is rendered in prepared-size pieces.
                mixer.note_host_block(&mix_shared, frames);
                render_or_silence(&mix_shared, data, |block| {
                    for (offset, count) in block_chunks(frames, PREPARED_BLOCK) {
                        mixer.render_chunk(&mix_shared, count, |index, mapped| {
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
