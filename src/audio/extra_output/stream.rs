//! A failed open marks the slot missing.  The clock bridge keeps running.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, FromSample, SampleFormat, SizedSample};

use super::super::devices::pick_output_config;
use super::super::output::{
    block_frames, clamp_output_buffer, explain_output_error, render_or_silence, write_frame,
};
use super::super::record::{push_device_frame, RecordSource};
use super::super::shared::{CallbackFailure, Shared};
use super::super::thread_priority::PromoteOnce;
use super::mixer::ExtraOutputMixer;

/// Dropping it stops that device only.
pub(crate) struct OpenedExtraOutput {
    _stream: cpal::Stream,
}

/// The ring-active flag must be off: the clock side is not feeding the
/// ring while its layout changes.
pub(crate) fn lay_out_ring(
    shared: &Shared,
    slot: usize,
    channels: usize,
    rate: u32,
    buffer_frames: usize,
) {
    let bus = shared.extra_output_bus(slot);
    bus.set_device_channels(channels);
    bus.reset_playback();
    bus.set_source_rate(shared.output_rate.load(Ordering::Relaxed).max(rate));
    bus.set_target_frames(super::super::shared::initial_target(buffer_frames));
}

pub(crate) fn open_extra_output(
    slot: usize,
    device_id: &str,
    sample_rate: u32,
    shared: &Arc<Shared>,
    buffer_frames: usize,
) -> Result<OpenedExtraOutput, String> {
    let id: cpal::DeviceId = device_id
        .parse()
        .map_err(|error| format!("Extra output device id: {error}"))?;
    let host =
        cpal::host_from_id(id.host()).map_err(|error| format!("Extra output host: {error}"))?;
    let device = host
        .device_by_id(&id)
        .ok_or_else(|| "The extra output disappeared".to_owned())?;
    let config = pick_output_config(&device, sample_rate)?;
    let channels = config.channels() as usize;
    let rate = config.sample_rate();
    let requested = clamp_output_buffer(&config, buffer_frames);

    // The ring-active flag is off until this returns, so the clock side is
    // not feeding the ring while it is laid out for this device.
    lay_out_ring(shared, slot, channels, rate, buffer_frames);

    let stream = match make_extra_output_stream(
        slot,
        &device,
        config,
        channels,
        Arc::clone(shared),
        Some(requested),
    ) {
        Ok(stream) => stream,
        Err(error) => {
            make_extra_output_stream(slot, &device, config, channels, Arc::clone(shared), None)
                .map_err(|fallback| {
                    explain_output_error(format!("{error}; fallback failed too: {fallback}"))
                })?
        }
    };
    stream
        .play()
        .map_err(|error| format!("Starting extra output: {error}"))?;
    // The opened device rate.  The ring's source rate stays the clock rate,
    // which is not what this stream plays when the two differ.
    shared.extra_output_bus(slot).set_playback_rate(rate);
    Ok(OpenedExtraOutput { _stream: stream })
}

fn make_extra_output_stream(
    slot: usize,
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
        SampleFormat::F32 => {
            build_extra_output::<f32>(slot, device, stream_config, channels, shared)
        }
        SampleFormat::I16 => {
            build_extra_output::<i16>(slot, device, stream_config, channels, shared)
        }
        SampleFormat::I32 => {
            build_extra_output::<i32>(slot, device, stream_config, channels, shared)
        }
        SampleFormat::I8 => build_extra_output::<i8>(slot, device, stream_config, channels, shared),
        SampleFormat::U8 => build_extra_output::<u8>(slot, device, stream_config, channels, shared),
        SampleFormat::U16 => {
            build_extra_output::<u16>(slot, device, stream_config, channels, shared)
        }
        SampleFormat::U32 => {
            build_extra_output::<u32>(slot, device, stream_config, channels, shared)
        }
        SampleFormat::F64 => {
            build_extra_output::<f64>(slot, device, stream_config, channels, shared)
        }
        SampleFormat::I64 => {
            build_extra_output::<i64>(slot, device, stream_config, channels, shared)
        }
        SampleFormat::U64 => {
            build_extra_output::<u64>(slot, device, stream_config, channels, shared)
        }
        other => Err(format!("Unsupported extra output sample format: {other:?}")),
    }
}

fn build_extra_output<T>(
    slot: usize,
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
    let mut mixer = ExtraOutputMixer::new(slot, config.sample_rate, channels);
    mixer.configure_source_rate(&shared);
    mixer.publish_channels(&shared);

    let stream = device
        .build_output_stream(
            config,
            move |data: &mut [T], _info: &cpal::OutputCallbackInfo| {
                promote.run();
                let Some(frames) = block_frames(data.len(), channels) else {
                    // A partial frame has no whole-frame count.  The sample
                    // path failed, not the device, so this stops the run.
                    data.fill(T::EQUILIBRIUM);
                    mix_shared.note_callback_failure(CallbackFailure::BadBlock);
                    return;
                };
                if mix_shared.callback_failure().is_some() {
                    // The chain must not be driven again.  The supervisor
                    // drops this stream when the bridge stops.
                    data.fill(T::EQUILIBRIUM);
                    return;
                }
                render_or_silence(&mix_shared, data, |block| {
                    mixer.render(&mix_shared, frames, |index, mapped| {
                        // After the resampler and the dropout fade: the frame
                        // this extra output actually plays.
                        if channels > 0 {
                            push_device_frame(
                                &mix_shared.recorder,
                                RecordSource::ExtraOutput(slot),
                                mapped,
                                channels,
                            );
                        }
                        write_frame(&mut block[index * channels..(index + 1) * channels], mapped);
                    });
                });
            },
            move |error| {
                if matches!(
                    error.kind(),
                    cpal::ErrorKind::DeviceNotAvailable | cpal::ErrorKind::StreamInvalidated
                ) {
                    // An extra output disconnect does not stop the bridge.
                    error_shared
                        .extra_output_bus(slot)
                        .missing
                        .store(true, Ordering::Relaxed);
                }
            },
            None,
        )
        .map_err(|error| format!("Building extra output stream: {error}"))?;
    Ok(stream)
}
