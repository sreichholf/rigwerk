//! Generic followers: other cpal devices that play along with the clock
//! device, each on its own stream and clock.
//!
//! PipeWire duplex keeps its in-graph fan-out (`render_direct_devices`).
//! This path is for ALSA / cpal clock devices and WASAPI followers on
//! Windows.
//!
//! # Flow
//!
//! ```text
//! clock callback (OutputMixer)                follower stream (FollowerMixer)
//!   paths → master                              ring → resampler → drift
//!   per follower: DeviceMapper                  → dropout fade → device
//!     (per-path map, level, polarity,
//!      device level / mute)
//!   → device channels ──▶ FollowerBus ring ──▶
//! ```
//!
//! - **The clock side mixes every follower.**  Each device needs its own sum
//!   of the paths (per-path map, level and polarity), so the clock
//!   callback runs one [`DeviceMapper`] per follower slot,
//!   exactly as for the clock device and the PipeWire duplex followers.
//!   While a follower's stream runs (`Shared::feeds_follower`), the mapped
//!   frame goes into that follower's ring.
//! - **The ring carries device channels**, one lane per channel up to
//!   `MAX_OUTPUT_CHANNELS` plus the shared lane for channels past it
//!   (`frame_lanes`).  Its storage is allocated once, with the bus, for two
//!   seconds of every lane at the engine rate (about 3.5 MB at 48 kHz), so
//!   the cushion ceiling (half the ring) is one second at any channel
//!   count.  A layout generation, read at the start of each clock block,
//!   stops a block from pushing into a ring that was reopened meanwhile.
//! - **The follower stream only crosses the clock**: ring fill, cushion,
//!   drift steering, rate conversion and the dropout fade, on every lane.
//!   It never maps or levels; with drift steering off, a follower at the
//!   clock rate plays the mapper's samples bit for bit.
//! - **Meters** come from the clock side's mapper (before the follower's
//!   dropout fade), like the clock device's; a follower without a running
//!   stream is not mapped and reads 0.
//! - **Latency**: a follower plays the clock side's block plus its own
//!   cushion and device buffer, as before paths existed.
//! - **Open / close**: the supervisor lays the ring out for the device's
//!   channel count while the slot's ring-active flag is off, then turns it
//!   on; the clock side reads the flag once per block (and again per frame
//!   before it pushes) and maps the follower onto that many channels.
//!
//! [`DeviceMapper`]: super::output_map::DeviceMapper

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
