//! Each extra output is its own stream and clock.  The clock callback
//! pushes that device's finished frames; this side only resamples and
//! steers drift, and never maps or levels.
//!
//! A failed open is retried and does not stop the clock.  PipeWire duplex
//! fans out in the graph and does not open these streams.  Alignment
//! leaves out each device's own bridge buffer and device buffer, so two
//! devices are not aligned with each other.

mod bus;
mod mixer;
mod stream;
mod supervisor;

#[cfg(test)]
mod tests;

pub(crate) use bus::ExtraOutputBus;
#[cfg(target_os = "linux")]
pub(crate) use mixer::ExtraOutputMixer;
pub(crate) use mixer::DRIFT_LIMIT_PPM as EXTRA_OUTPUT_DRIFT_LIMIT_PPM;
#[cfg(target_os = "linux")]
pub(crate) use stream::lay_out_ring;
pub(crate) use supervisor::ExtraOutputOutputs;
#[cfg(target_os = "linux")]
pub(crate) use supervisor::ExtraOutputSlots;

/// A cpal ring, not a PipeWire graph link.  On Windows only WASAPI
/// endpoints qualify.
pub(crate) fn is_generic_extra_output(device_id: &str) -> bool {
    if device_id.is_empty() {
        return false;
    }
    #[cfg(target_os = "linux")]
    if super::linux::native::is_native_id(device_id) {
        return false;
    }
    let Ok(id) = device_id.parse::<cpal::DeviceId>() else {
        return false;
    };
    #[cfg(target_os = "windows")]
    if id.host() != cpal::HostId::Wasapi {
        return false;
    }
    #[cfg(not(target_os = "windows"))]
    let _ = id;
    true
}

/// Matched on the display name.  Playback is "CABLE Input"; capture is
/// "CABLE Output".
pub(crate) fn is_vb_cable_render(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("cable input")
        && (lower.contains("vb-audio")
            || lower.contains("vb audio")
            || lower.contains("vb-cable")
            || lower.contains("virtual cable"))
}

#[cfg(test)]
mod name_tests;
