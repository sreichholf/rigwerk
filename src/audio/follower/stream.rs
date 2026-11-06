//! Opening a cpal playback stream for one generic follower.  Errors here
//! never stop the bridge: the slot is marked missing and the supervisor
//! retries later.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, FromSample, SampleFormat, SizedSample};

use super::super::devices::pick_output_config;
use super::super::output::{clamp_output_buffer, explain_output_error, write_frame};
use super::super::shared::Shared;
use super::super::thread_priority::PromoteOnce;
use super::mixer::FollowerMixer;

/// A live follower stream.  Dropping it stops playback on that device only.
pub(crate) struct OpenedFollower {
    _stream: cpal::Stream,
    pub(crate) device_id: String,
}

/// Opens one follower on its own cpal stream.  On failure the slot is marked
/// missing and the error is returned; the clock bridge keeps running.
pub(crate) fn open_follower(
    slot: usize,
    device_id: &str,
    sample_rate: u32,
    shared: &Arc<Shared>,
    buffer_frames: usize,
) -> Result<OpenedFollower, String> {
    let id: cpal::DeviceId = device_id
        .parse()
        .map_err(|error| format!("Follower device id: {error}"))?;
    let host = cpal::host_from_id(id.host()).map_err(|error| format!("Follower host: {error}"))?;
    let device = host
        .device_by_id(&id)
        .ok_or_else(|| "The follower output disappeared".to_owned())?;
    let config = pick_output_config(&device, sample_rate)?;
    let channels = config.channels() as usize;
    let rate = config.sample_rate();
    let requested = clamp_output_buffer(&config, buffer_frames);

    let bus = shared.follower_bus(slot);
    bus.reset_playback();
    bus.set_source_rate(shared.output_rate.load(Ordering::Relaxed).max(rate));
    bus.set_target_frames(super::super::shared::initial_target(buffer_frames));

    let stream = match make_follower_stream(
        slot,
        &device,
        config,
        channels,
        Arc::clone(shared),
        Some(requested),
    ) {
        Ok(stream) => stream,
        Err(error) => {
            make_follower_stream(slot, &device, config, channels, Arc::clone(shared), None)
                .map_err(|fallback| {
                    explain_output_error(format!("{error}; fallback failed too: {fallback}"))
                })?
        }
    };
    stream
        .play()
        .map_err(|error| format!("Starting follower: {error}"))?;
    Ok(OpenedFollower {
        _stream: stream,
        device_id: device_id.to_owned(),
    })
}

fn make_follower_stream(
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
        SampleFormat::F32 => build_follower::<f32>(slot, device, stream_config, channels, shared),
        SampleFormat::I16 => build_follower::<i16>(slot, device, stream_config, channels, shared),
        SampleFormat::I32 => build_follower::<i32>(slot, device, stream_config, channels, shared),
        SampleFormat::I8 => build_follower::<i8>(slot, device, stream_config, channels, shared),
        SampleFormat::U8 => build_follower::<u8>(slot, device, stream_config, channels, shared),
        SampleFormat::U16 => build_follower::<u16>(slot, device, stream_config, channels, shared),
        SampleFormat::U32 => build_follower::<u32>(slot, device, stream_config, channels, shared),
        SampleFormat::F64 => build_follower::<f64>(slot, device, stream_config, channels, shared),
        SampleFormat::I64 => build_follower::<i64>(slot, device, stream_config, channels, shared),
        SampleFormat::U64 => build_follower::<u64>(slot, device, stream_config, channels, shared),
        other => Err(format!("Unsupported follower sample format: {other:?}")),
    }
}

fn build_follower<T>(
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
    let mut mixer = FollowerMixer::new(slot, config.sample_rate, channels);
    mixer.follow_source_rate(&shared);
    mixer.publish_channels(&shared);

    let stream = device
        .build_output_stream(
            config,
            move |data: &mut [T], _info: &cpal::OutputCallbackInfo| {
                promote.run();
                let frames = data.len() / channels;
                mixer.render(&mix_shared, frames, |index, mapped| {
                    write_frame(&mut data[index * channels..(index + 1) * channels], mapped);
                });
            },
            move |error| {
                if matches!(
                    error.kind(),
                    cpal::ErrorKind::DeviceNotAvailable | cpal::ErrorKind::StreamInvalidated
                ) {
                    // Never stop the bridge for a follower disconnect.
                    error_shared
                        .follower_bus(slot)
                        .missing
                        .store(true, Ordering::Relaxed);
                }
            },
            None,
        )
        .map_err(|error| format!("Building follower stream: {error}"))?;
    Ok(stream)
}
