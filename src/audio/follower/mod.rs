//! Generic followers: finished-mix fan-out onto other cpal devices.
//!
//! PipeWire duplex keeps its in-graph fan-out (`render_direct_devices`).
//! This path is for ALSA / cpal clock devices (Step 3) and WASAPI followers
//! on Windows (Step 4).

mod bus;
mod mixer;
mod stream;
mod supervisor;

#[cfg(test)]
mod tests;

pub(crate) use bus::FollowerBus;
pub(crate) use supervisor::FollowerOutputs;

/// Whether `device_id` should play through a generic follower ring (cpal),
/// not through a PipeWire graph link.  On Windows only WASAPI endpoints
/// qualify (no DirectSound / MME / ASIO followers).
pub(crate) fn is_generic_follower(device_id: &str) -> bool {
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

/// VB-Audio Cable render endpoint (other apps capture from CABLE Output).
/// Matched on the display name Windows reports.
pub(crate) fn is_vb_cable_render(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    // Playback side is "CABLE Input …"; capture side is "CABLE Output …".
    lower.contains("cable input")
        && (lower.contains("vb-audio")
            || lower.contains("vb audio")
            || lower.contains("vb-cable")
            || lower.contains("virtual cable"))
}

#[cfg(test)]
mod name_tests {
    use super::is_vb_cable_render;

    #[test]
    fn vb_cable_render_names_are_recognised() {
        assert!(is_vb_cable_render("CABLE Input (VB-Audio Virtual Cable)"));
        assert!(is_vb_cable_render("cable input (vb-audio)"));
        assert!(!is_vb_cable_render("CABLE Output (VB-Audio Virtual Cable)"));
        assert!(!is_vb_cable_render("Speakers (Realtek)"));
    }
}
