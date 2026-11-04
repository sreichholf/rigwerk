//! PipeWire is the default Linux backend.  RigWerk reaches it through the
//! ALSA `pipewire` PCM and uses the `pw-metadata` settings store to read and
//! request the graph quantum, so the bridge can run at a low quantum without
//! a native PipeWire dependency.
//!
//! The quantum is the number of frames PipeWire processes per graph cycle.
//! 64 frames at 48 kHz is about 1.3 ms, which RigWerk counts as low latency;
//! anything above [`PIPEWIRE_LOW_QUANTUM_MAX`] is shown as the desktop's
//! shared size instead.

use std::env;
use std::path::PathBuf;
use std::process::Command;

use crate::audio::types::{DeviceItem, QuantumInfo};

use super::alsa::{alsa_pcm_is_default, alsa_pcm_name};

/// The quantum RigWerk asks for while the low-latency bridge runs.
pub(crate) const LOW_QUANTUM: u32 = crate::audio::types::PIPEWIRE_LOW_QUANTUM;

/// What the PipeWire settings store says right now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct State {
    pub(crate) quantum: u32,
    pub(crate) rate: u32,
}

fn runtime_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = env::var_os("PIPEWIRE_RUNTIME_DIR") {
        dirs.push(PathBuf::from(dir));
    }
    if let Some(dir) = env::var_os("XDG_RUNTIME_DIR") {
        dirs.push(PathBuf::from(dir));
    }
    dirs
}

/// Whether a PipeWire server socket is present.  This is only a cheap hint for
/// classification; the authoritative quantum values still come from
/// `pw-metadata`.
pub(crate) fn server_running() -> bool {
    runtime_dirs()
        .iter()
        .any(|dir| dir.join("pipewire-0").exists())
}

/// A cpal ALSA id that names the PipeWire plugin directly.
pub(crate) fn is_pipewire_pcm(device_id: &str) -> bool {
    alsa_pcm_name(device_id)
        .to_ascii_lowercase()
        .contains("pipewire")
}

/// The `default` PCM is owned by PipeWire on a normal desktop, so it is the
/// default backend even when its id does not mention the server.
pub(crate) fn is_pipewire_endpoint(device_id: &str, name: &str) -> bool {
    if is_pipewire_pcm(device_id) || name.to_ascii_lowercase().contains("pipewire") {
        return true;
    }
    server_running() && alsa_pcm_is_default(device_id)
}

pub(crate) fn is_pipewire_device(device: &DeviceItem) -> bool {
    device.backend.eq_ignore_ascii_case("PIPEWIRE")
        || is_pipewire_endpoint(&device.id, &device.name)
}

/// Reads one value from `pw-metadata -n settings` output, e.g.
/// `update: id:0 key:'clock.quantum' value:'1024' type:''`.
fn metadata_value(text: &str, key: &str) -> Option<u32> {
    let needle = format!("key:'{key}'");
    for line in text.lines() {
        let Some(rest) = line.split_once(needle.as_str()).map(|(_, rest)| rest) else {
            continue;
        };
        let Some((_, after)) = rest.split_once("value:'") else {
            continue;
        };
        let value = after.split('\'').next()?.trim();
        if let Ok(value) = value.parse::<u32>() {
            return Some(value);
        }
    }
    None
}

fn parse_state(text: &str) -> State {
    let forced_quantum = metadata_value(text, "clock.force-quantum").unwrap_or(0);
    let quantum = if forced_quantum > 0 {
        forced_quantum
    } else {
        metadata_value(text, "clock.quantum").unwrap_or(0)
    };
    let forced_rate = metadata_value(text, "clock.force-rate").unwrap_or(0);
    let rate = if forced_rate > 0 {
        forced_rate
    } else {
        metadata_value(text, "clock.rate").unwrap_or(0)
    };
    State { quantum, rate }
}

fn read_metadata() -> Option<String> {
    let output = Command::new("pw-metadata")
        .args(["-n", "settings"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The server's current quantum and rate, or `None` when `pw-metadata` is not
/// installed or PipeWire is not running.
pub(crate) fn state() -> Option<State> {
    read_metadata().map(|text| parse_state(&text))
}

fn write_metadata(key: &str, value: &str) -> Result<(), String> {
    let output = Command::new("pw-metadata")
        .args(["-n", "settings", "0", key, value])
        .output()
        .map_err(|error| format!("pw-metadata not available: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "pw-metadata rejected {key}={value}: {}",
            stderr.trim()
        ))
    }
}

/// Asks PipeWire for the low quantum.  The setting is global while it is
/// forced, so the bridge releases it when it stops.
pub(crate) fn force_low_quantum() -> Result<State, String> {
    write_metadata("clock.force-quantum", &LOW_QUANTUM.to_string())?;
    state().ok_or_else(|| {
        "PipeWire accepted the quantum but did not report it back; is the server running?"
            .to_owned()
    })
}

/// The quantum someone forced before RigWerk started, so stopping the bridge
/// can put it back instead of clearing their setting.
pub(crate) fn forced_quantum() -> u32 {
    read_metadata()
        .map(|text| metadata_value(&text, "clock.force-quantum").unwrap_or(0))
        .unwrap_or(0)
}

/// Gives the desktop its own quantum back.
pub(crate) fn restore_quantum(previous: u32) {
    let _ = write_metadata("clock.force-quantum", &previous.to_string());
}

/// Compares the output's ALSA period with the current PipeWire quantum.
pub(crate) fn check_output(device: &DeviceItem, alsa_period: u32) -> Option<QuantumInfo> {
    if !is_pipewire_device(device) {
        return None;
    }
    let state = state();
    Some(match state {
        Some(state) => QuantumInfo::assess(true, state.quantum, state.rate, alsa_period),
        None => QuantumInfo::assess(false, 0, 0, alsa_period),
    })
}

#[cfg(test)]
mod tests {
    use super::{is_pipewire_pcm, metadata_value, parse_state};

    #[test]
    fn the_pipewire_pcm_is_recognised_with_or_without_a_host_prefix() {
        assert!(is_pipewire_pcm("ALSA:pipewire"));
        assert!(is_pipewire_pcm("alsa:pipewire"));
        assert!(is_pipewire_pcm("pipewire"));
        assert!(!is_pipewire_pcm("ALSA:default"));
        assert!(!is_pipewire_pcm("ALSA:pulse"));
    }

    #[test]
    fn a_forced_quantum_wins_over_the_default() {
        let text = "update: id:0 key:'clock.quantum' value:'1024' type:''\n\
                    update: id:0 key:'clock.force-quantum' value:'64' type:''\n\
                    update: id:0 key:'clock.rate' value:'48000' type:''";
        let state = parse_state(text);
        assert_eq!(state.quantum, 64);
        assert_eq!(state.rate, 48_000);
    }

    #[test]
    fn zero_force_falls_back_to_the_default_quantum() {
        let text = "update: id:0 key:'clock.quantum' value:'1024' type:''\n\
                    update: id:0 key:'clock.force-quantum' value:'0' type:''";
        assert_eq!(parse_state(text).quantum, 1024);
    }

    #[test]
    fn values_are_read_from_the_metadata_line() {
        let text = "update: id:0 key:'clock.rate' value:'44100' type:''";
        assert_eq!(metadata_value(text, "clock.rate"), Some(44_100));
    }
}
