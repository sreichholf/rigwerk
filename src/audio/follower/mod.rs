//! Generic followers: finished-mix fan-out onto other cpal devices.
//!
//! PipeWire duplex keeps its in-graph fan-out (`render_direct_devices`).
//! This path is for ALSA / cpal clock devices (Step 3) and, later, WASAPI.

mod bus;
mod mixer;
mod stream;
mod supervisor;

#[cfg(test)]
mod tests;

pub(crate) use bus::FollowerBus;
pub(crate) use supervisor::FollowerOutputs;

/// Whether `device_id` should play through a generic follower ring (cpal),
/// not through a PipeWire graph link.
pub(crate) fn is_generic_follower(device_id: &str) -> bool {
    if device_id.is_empty() {
        return false;
    }
    #[cfg(target_os = "linux")]
    if super::linux::native::is_native_id(device_id) {
        return false;
    }
    // cpal DeviceId parse is the gate the open path uses too.
    device_id.parse::<cpal::DeviceId>().is_ok()
}
