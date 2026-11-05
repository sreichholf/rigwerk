//! PipeWire graph quantum, read and requested through `pw-metadata`.
//!
//! The device list and the bridge streams are a native client (`native`).
//! This module only owns the desktop-wide `clock.force-quantum` switch, which
//! stays behind the low-latency control because it pins every app on the
//! graph. Node latency for our own streams is set separately.
//!
//! The quantum is the number of frames PipeWire processes per graph cycle.
//! 64 frames at 48 kHz is about 1.3 ms, which RigWerk counts as low latency;
//! anything above [`PIPEWIRE_LOW_QUANTUM_MAX`] is shown as the desktop's
//! shared size instead.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

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

/// A cpal ALSA id that names the PipeWire plugin directly.
pub(crate) fn is_pipewire_pcm(device_id: &str) -> bool {
    alsa_pcm_name(device_id)
        .to_ascii_lowercase()
        .contains("pipewire")
}

/// The `default` PCM is PipeWire only when ALSA's own default plugin says so.
///
/// A running PipeWire socket is not enough: PulseAudio often leaves that
/// socket on a machine whose `pcm.!default` is still `type pulse`. Forcing a
/// 64-frame quantum on that default would pin the wrong server.
pub(crate) fn is_pipewire_endpoint(device_id: &str, name: &str) -> bool {
    if is_pipewire_pcm(device_id) || name.to_ascii_lowercase().contains("pipewire") {
        return true;
    }
    alsa_pcm_is_default(device_id) && default_plugin_is_pipewire()
}

/// Whether the last `pcm.!default` block in an ALSA config uses `type pipewire`.
///
/// Later blocks win, which is how `/etc/asound.conf` and `~/.asoundrc` override
/// the snippets under `alsa.conf.d`.
pub(crate) fn default_pcm_is_pipewire(config: &str) -> bool {
    let mut last = false;
    let mut search_from = 0;
    let lower = config.to_ascii_lowercase();
    while let Some(rel) = lower[search_from..].find("pcm.!default") {
        let start = search_from + rel;
        let after = &config[start + "pcm.!default".len()..];
        let Some(brace) = after.find('{') else {
            break;
        };
        if after[..brace].chars().any(|c| !c.is_whitespace()) {
            search_from = start + "pcm.!default".len();
            continue;
        }
        let body_start = start + "pcm.!default".len() + brace + 1;
        let Some(end) = config[body_start..].find('}') else {
            break;
        };
        last = block_type_is_pipewire(&lower[body_start..body_start + end]);
        search_from = body_start + end;
    }
    last
}

fn block_type_is_pipewire(body_lower: &str) -> bool {
    for line in body_lower.lines() {
        let line = line.split('#').next().unwrap_or(line);
        let compact: String = line.chars().filter(|c| !c.is_whitespace()).collect();
        if compact.contains("typepipewire") {
            return true;
        }
    }
    false
}

fn default_plugin_is_pipewire() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| {
        let mut combined = String::new();
        for dir in ["/usr/share/alsa/alsa.conf.d", "/etc/alsa/conf.d"] {
            append_conf_dir(&mut combined, Path::new(dir));
        }
        append_file(&mut combined, Path::new("/etc/asound.conf"));
        if let Some(home) = env::var_os("HOME") {
            append_file(&mut combined, Path::new(&home).join(".asoundrc").as_path());
        }
        default_pcm_is_pipewire(&combined)
    })
}

fn append_conf_dir(combined: &mut String, dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "conf"))
        .collect();
    paths.sort();
    for path in paths {
        append_file(combined, &path);
    }
}

fn append_file(combined: &mut String, path: &Path) {
    if let Ok(text) = fs::read_to_string(path) {
        combined.push('\n');
        combined.push_str(&text);
    }
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

/// The quantum someone forced before RigWerk changes it.
///
/// `Ok(0)` means the store was read and nothing is forced. `Err` means the
/// store could not be read, which is not the same as "force nothing": a later
/// restore must not invent a 0 and clear a setting it never saw.
pub(crate) fn read_forced_quantum() -> Result<u32, String> {
    let text = read_metadata().ok_or_else(|| {
        "pw-metadata not available, so the current quantum was not read".to_owned()
    })?;
    Ok(metadata_value(&text, "clock.force-quantum").unwrap_or(0))
}

/// What to do with `clock.force-quantum` after one attempt to set it.
///
/// `previous` is `None` when the store could not be read. In that case nothing
/// is written. A successful write is owned even when the read-back fails, so
/// stop can still restore `previous`. `previous == 0` is a real "nothing was
/// forced", not a stand-in for a failed read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QuantumForce {
    Untouched,
    Owned { previous: u32 },
    RolledBack,
}

pub(crate) fn decide_quantum_force(
    previous: Option<u32>,
    wrote: bool,
    confirmed: bool,
    restored_after_unconfirmed: bool,
) -> QuantumForce {
    let Some(previous) = previous else {
        return QuantumForce::Untouched;
    };
    if !wrote {
        return QuantumForce::Untouched;
    }
    if confirmed {
        return QuantumForce::Owned { previous };
    }
    if restored_after_unconfirmed {
        QuantumForce::RolledBack
    } else {
        QuantumForce::Owned { previous }
    }
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

/// Asks PipeWire for the low quantum.
///
/// `Ok(None)` means the write was accepted and the following read failed. The
/// caller owns the force and must restore the previous value; a failed read is
/// not a failed write.
pub(crate) fn force_low_quantum() -> Result<Option<State>, String> {
    write_metadata("clock.force-quantum", &LOW_QUANTUM.to_string())?;
    Ok(state())
}

/// Gives the desktop its own quantum back. The error is returned so a failed
/// restore can be shown instead of discarded.
pub(crate) fn restore_quantum(previous: u32) -> Result<(), String> {
    write_metadata("clock.force-quantum", &previous.to_string())
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
    use super::{
        decide_quantum_force, default_pcm_is_pipewire, is_pipewire_pcm, metadata_value,
        parse_state, QuantumForce,
    };

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

    #[test]
    fn a_pulse_default_is_not_pipewire_just_because_the_name_default_appears() {
        let pulse =
            "pcm.!default {\n    type pulse\n    hint { show on description \"Default\" }\n}\n";
        assert!(!default_pcm_is_pipewire(pulse));
        let pipewire = "pcm.!default {\n    type pipewire\n}\n";
        assert!(default_pcm_is_pipewire(pipewire));
    }

    #[test]
    fn a_later_default_block_overrides_an_earlier_one() {
        let config = "pcm.!default { type pipewire }\n\
                      pcm.!default { type pulse }\n";
        assert!(!default_pcm_is_pipewire(config));
        let back = "pcm.!default { type pulse }\n\
                    pcm.!default { type pipewire }\n";
        assert!(default_pcm_is_pipewire(back));
    }

    #[test]
    fn a_commented_pipewire_type_does_not_count() {
        let config = "pcm.!default {\n    # type pipewire\n    type pulse\n}\n";
        assert!(!default_pcm_is_pipewire(config));
    }

    #[test]
    fn an_unread_quantum_is_not_treated_as_zero_and_a_write_is_owned() {
        assert_eq!(
            decide_quantum_force(None, false, false, false),
            QuantumForce::Untouched
        );
        assert_eq!(
            decide_quantum_force(Some(0), false, false, false),
            QuantumForce::Untouched
        );
        assert_eq!(
            decide_quantum_force(Some(0), true, true, false),
            QuantumForce::Owned { previous: 0 }
        );
        assert_eq!(
            decide_quantum_force(Some(1024), true, false, true),
            QuantumForce::RolledBack
        );
        assert_eq!(
            decide_quantum_force(Some(1024), true, false, false),
            QuantumForce::Owned { previous: 1024 }
        );
    }

    #[test]
    fn restore_quantum_reports_a_missing_tool() {
        match super::restore_quantum(0) {
            Ok(()) => {}
            Err(error) => assert!(
                error.contains("pw-metadata"),
                "restore error should name the tool: {error}"
            ),
        }
    }
}
