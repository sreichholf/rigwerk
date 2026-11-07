//! Direct ALSA helpers: PCM classification and the period the bridge asks
//! for.  These stay separate from PipeWire so the two Linux paths can be
//! reasoned about on their own.

use cpal::traits::{DeviceTrait, HostTrait};

/// The PCM part of a cpal ALSA id (`ALSA:hw:CARD=0,DEV=0` -> `hw:CARD=0,DEV=0`).
pub(crate) fn alsa_pcm_name(device_id: &str) -> &str {
    // cpal prints the host in lower case (`alsa:hw:CARD=0,DEV=0`), while other
    // backends and older versions use the display spelling (`ALSA:...`).
    match device_id.split_once(':') {
        Some((host, pcm)) if host.eq_ignore_ascii_case("alsa") => pcm.trim(),
        _ => device_id.trim(),
    }
}

/// Whether a cpal ALSA id names a physical PCM rather than a virtual plugin.
/// `hw:` bypasses every conversion; `plughw:` still goes straight to the card
/// but lets ALSA convert formats.  Both are the low-latency ALSA side.
pub(crate) fn alsa_pcm_is_direct(device_id: &str) -> bool {
    let pcm = alsa_pcm_name(device_id);
    pcm.starts_with("hw:") || pcm.starts_with("plughw:")
}

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
