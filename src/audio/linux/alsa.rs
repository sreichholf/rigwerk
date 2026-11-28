use cpal::traits::{DeviceTrait, HostTrait};

use super::super::types::OutputMode;

// The id grammar is `devices`'s, because that file compiles on every platform
// while this module is Linux and tests only.
pub(crate) use super::super::devices::{alsa_pcm_is_direct, alsa_pcm_name};

pub(crate) fn alsa_pcm_is_placeholder(device_id: &str) -> bool {
    let pcm = alsa_pcm_name(device_id);
    pcm == "null" || pcm == "oss" || pcm.starts_with("oss:")
}

/// On a normal desktop PipeWire owns `default`, so the id is that path
/// even though it does not say so.
pub(crate) fn alsa_pcm_is_default(device_id: &str) -> bool {
    alsa_pcm_name(device_id).eq_ignore_ascii_case("default")
}

/// Smallest ALSA period, floored so a one-frame driver minimum is not a
/// stream of xruns.
pub(crate) fn smallest_output_buffer(config: &cpal::SupportedStreamConfig, floor: usize) -> usize {
    match config.buffer_size() {
        cpal::SupportedBufferSize::Range { min, max } => {
            (*min as usize).max(floor).min(*max as usize)
        }
        cpal::SupportedBufferSize::Unknown => floor,
    }
}

pub(crate) fn requested_output_buffer(
    config: &cpal::SupportedStreamConfig,
    buffer_frames: usize,
    low_latency: bool,
) -> usize {
    if low_latency {
        smallest_output_buffer(config, 32)
    } else {
        crate::audio::output::clamp_output_buffer(config, buffer_frames)
    }
}

/// A refused fixed buffer is the driver's default, so Standard even when
/// low latency asked for the smallest period.
pub(crate) fn describe(low_latency: bool, fixed_buffer: bool) -> (OutputMode, String) {
    let note = if fixed_buffer {
        ""
    } else {
        crate::audio::output::FIXED_BUFFER_REFUSED
    };
    let mode = if low_latency && fixed_buffer {
        OutputMode::LowLatency
    } else {
        OutputMode::Standard
    };
    let line = if low_latency {
        format!("Low latency · ALSA smallest period{note}")
    } else {
        format!("Standard output{note}")
    };
    (mode, line)
}

/// The period asked for, without opening a stream.  `0` is a missing or
/// demo device.
pub(crate) fn planned_output_period(
    device_id: &str,
    low_latency: bool,
    buffer_frames: usize,
) -> u32 {
    let Ok(id) = device_id.parse::<cpal::DeviceId>() else {
        return 0;
    };
    let Ok(host) = cpal::host_from_id(id.host()) else {
        return 0;
    };
    let Some(device) = host.device_by_id(&id) else {
        return 0;
    };
    let Ok(config) = device.default_output_config() else {
        return 0;
    };
    let frames = requested_output_buffer(&config, buffer_frames, low_latency);
    frames.min(u32::MAX as usize) as u32
}

#[cfg(test)]
mod tests;
