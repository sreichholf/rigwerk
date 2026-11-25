//! Direct ALSA helpers: PCM classification and the period the bridge asks
//! for.  The PCM id grammar is `devices`'s; this module re-exports it for the
//! Linux call paths.  They stay separate from PipeWire so the two Linux paths
//! can be reasoned about on their own.

use cpal::traits::{DeviceTrait, HostTrait};

// The id grammar is `devices`'s, because that file compiles on every platform
// while this module is Linux and tests only.
pub(crate) use super::super::devices::{alsa_pcm_is_direct, alsa_pcm_name};

pub(crate) fn alsa_pcm_is_placeholder(device_id: &str) -> bool {
    let pcm = alsa_pcm_name(device_id);
    pcm == "null" || pcm == "oss" || pcm.starts_with("oss:")
}

/// The default PCM is the one PipeWire owns on a normal desktop, so it is the
/// PipeWire path even though the id does not say so.
pub(crate) fn alsa_pcm_is_default(device_id: &str) -> bool {
    alsa_pcm_name(device_id).eq_ignore_ascii_case("default")
}

/// The smallest period ALSA allows, with a floor so a driver that reports a
/// one-frame minimum cannot turn the bridge into a stream of xruns.
pub(crate) fn smallest_output_buffer(config: &cpal::SupportedStreamConfig, floor: usize) -> usize {
    match config.buffer_size() {
        cpal::SupportedBufferSize::Range { min, max } => {
            (*min as usize).max(floor).min(*max as usize)
        }
        cpal::SupportedBufferSize::Unknown => floor,
    }
}

/// The period the bridge will ask this output for, without opening a stream.
/// `0` means the period is not known (a missing or demo device).
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
    let frames = if low_latency {
        smallest_output_buffer(&config, 32)
    } else {
        crate::audio::output::clamp_output_buffer(&config, buffer_frames)
    };
    frames.min(u32::MAX as usize) as u32
}

#[cfg(test)]
mod tests;
