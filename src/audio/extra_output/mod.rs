//! Generic extra outputs: other cpal devices that play along with the clock
//! output, each on its own stream and clock.
//!
//! PipeWire duplex keeps its in-graph fan-out (`render_direct_devices`).
//! This path is for ALSA / cpal clock outputs (also on the PipeWire
//! two-stream bridge with a cpal clock) and WASAPI extra outputs on Windows.
//! With a PipeWire clock, that bridge drains the same rings into one
//! PipeWire playback stream per node extra output (`linux::native`).
//!
//! # Flow
//!
//! ```text
//! clock callback (OutputMixer)                extra output stream (ExtraOutputMixer)
//!   paths → master → per-path delay lines       ring → resampler → drift
//!   per extra output: DeviceMapper                  → dropout fade → device
//!     (alignment taps, per-path map, level,
//!      polarity, device level / mute)
//!   → device channels ──▶ ExtraOutputBus ring ──▶
//! ```
//!
//! - **The clock side mixes every extra output.**  Each device needs its own sum
//!   of the paths (per-path map, level and polarity) and its own alignment
//!   taps, so the clock callback runs one [`DeviceMapper`] per extra output slot,
//!   exactly as for the clock output and the PipeWire duplex extra outputs.
//!   While an extra output's stream runs (`Shared::feeds_extra_output`), the mapped
//!   frame goes into that extra output's ring.
//! - **The ring carries device channels**, one lane per channel up to
//!   `MAX_OUTPUT_CHANNELS` plus the shared lane for channels past it
//!   (`frame_lanes`).  Up to 4 channels, the click-free lanes a take
//!   records follow them (`bus::carries_record`); a wider extra output's take
//!   keeps the click.  Its storage is allocated once, with the bus, for two
//!   seconds of every lane at the engine rate (about 3.5 MB at 48 kHz), so
//!   the bridge buffer ceiling (half the ring) is one second at any channel
//!   count.  A layout generation, read at the start of each clock block,
//!   stops a block from pushing into a ring that was reopened meanwhile.
//! - **The extra output stream only crosses the clock**: ring fill, bridge buffer,
//!   drift steering, rate conversion and the dropout fade, on every lane.
//!   It never maps or levels; with drift steering off, an extra output at the
//!   clock rate plays the mapper's samples bit for bit.
//! - **Meters** come from the clock side's mapper (before the extra output's
//!   dropout fade), like the clock output's; an extra output without a running
//!   stream is not mapped and reads 0.
//! - **Latency**: an extra output plays its alignment delay (it waits for the
//!   slowest path routed to it) plus its own bridge buffer and device buffer.
//!   Alignment is per device and leaves out the extra output's bridge buffer and
//!   device buffer (and the clock output's), so two devices are not
//!   aligned with each other.  The UI that shows a device's alignment
//!   should say so.
//! - **Open / close**: the supervisor lays the ring out for the device's
//!   channel count while the slot's ring-active flag is off, then turns it
//!   on; the clock side reads the flag once per block (and again per frame
//!   before it pushes) and maps the extra output onto that many channels.
//!
//! [`DeviceMapper`]: super::output_map::DeviceMapper

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

/// Whether `device_id` should play through a generic extra output ring (cpal),
/// not through a PipeWire graph link.  On Windows only WASAPI endpoints
/// qualify (no DirectSound / MME / ASIO extra outputs).
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
mod name_tests;
