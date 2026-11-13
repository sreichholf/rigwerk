//! The Live record transport: the folder, the take name, and arming the engine.
//!
//! The UI thread never opens, writes, or joins the wav.  `start_recording`
//! builds a path and calls `AudioEngine::arm_recording`, which returns
//! immediately.  The writer creates parent directories.  Phase, elapsed
//! time, and overruns are read from `AudioEngine::recording_status`.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::app::settings::FollowerPref;
use crate::app::{LogTone, RigWerkApp};
use crate::audio::{is_vb_cable_render, EngineState, RecordPhase, RecordSource};

/// Where an empty recordings folder lands.
///
/// `$HOME/Recordings`, then `$USERPROFILE/Recordings`.  When neither variable
/// is set, `recordings` in the current directory.
pub(crate) fn default_record_folder() -> String {
    folder_from_env(
        std::env::var_os("HOME").as_deref(),
        std::env::var_os("USERPROFILE").as_deref(),
    )
}

fn folder_from_env(home: Option<&OsStr>, userprofile: Option<&OsStr>) -> String {
    for dir in [home, userprofile].into_iter().flatten() {
        if !dir.is_empty() {
            return Path::new(dir)
                .join("Recordings")
                .to_string_lossy()
                .into_owned();
        }
    }
    "recordings".to_owned()
}

/// An empty or blank folder becomes [`default_record_folder`].  Anything
/// else is kept, with surrounding whitespace removed.  The directory is not
/// created here.
pub(crate) fn resolved_record_folder(folder: &str) -> String {
    let trimmed = folder.trim();
    if trimmed.is_empty() {
        default_record_folder()
    } else {
        trimmed.to_owned()
    }
}

/// Letters, numbers, spaces, and hyphens.  Anything else is dropped.
/// Empty, once the ends are trimmed, becomes `take`.
pub(crate) fn sanitize_take_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == ' ' || *c == '-')
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        "take".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// `{session} {YYYYMMDD-HHMMSS}.wav` inside `folder`.  A path that is already
/// there gets `-2`, then `-3`, and so on.  This only stats.  It does not
/// create the file or the folder.
pub(crate) fn recording_path(folder: &Path, session_name: &str, stamp: &str) -> PathBuf {
    let stem = format!("{} {stamp}", sanitize_take_name(session_name));
    let mut path = folder.join(format!("{stem}.wav"));
    let mut suffix = 2u32;
    while path.exists() {
        path = folder.join(format!("{stem}-{suffix}.wav"));
        if suffix == u32::MAX {
            break;
        }
        suffix += 1;
    }
    path
}

/// `m:ss` from `frames / rate`.  A rate of 0 is `0:00` (no division).
pub(crate) fn format_elapsed(frames: u64, rate: u32) -> String {
    if rate == 0 {
        return "0:00".to_owned();
    }
    let secs = frames / u64::from(rate);
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// A short overrun count.  `1 overrun`, otherwise `N overruns`.
pub(crate) fn overrun_label(count: u64) -> String {
    match count {
        1 => "1 overrun".to_owned(),
        n => format!("{n} overruns"),
    }
}

/// The endpoint the take should capture.
///
/// On Windows the publish follower is the virtual cable, and its saved name
/// is "RigWerk Out", not "CABLE Input".  A cable added as an ordinary
/// follower is still matched by name.  Every other case is the clock output.
pub(crate) fn record_source(followers: &[FollowerPref]) -> RecordSource {
    record_source_for(cfg!(target_os = "windows"), followers)
}

fn record_source_for(windows: bool, followers: &[FollowerPref]) -> RecordSource {
    if !windows {
        return RecordSource::Clock;
    }
    if let Some(index) = followers.iter().position(|follower| follower.virtual_out) {
        return RecordSource::Follower(index);
    }
    match followers
        .iter()
        .position(|follower| is_vb_cable_render(&follower.name))
    {
        Some(index) => RecordSource::Follower(index),
        None => RecordSource::Clock,
    }
}

fn take_stamp() -> String {
    local_stamp().unwrap_or_else(|| stamp_from_unix(unix_now()))
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

fn stamp_from_unix(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let seconds = unix_secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format_stamp(
        year,
        month,
        day,
        (seconds / 3_600) as u32,
        ((seconds % 3_600) / 60) as u32,
        (seconds % 60) as u32,
    )
}

fn format_stamp(year: i64, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> String {
    format!("{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}")
}

/// Howard Hinnant's `civil_from_days`, the same conversion `app::latency` uses.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (year + i64::from(month <= 2), month, day)
}

#[cfg(target_os = "linux")]
fn local_stamp() -> Option<String> {
    let Ok(secs) = libc::time_t::try_from(unix_now()) else {
        return None;
    };
    let mut tm = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: `localtime_r` writes a complete `tm` on success and does not
    // retain the pointer.  A null return leaves `tm` uninitialized.
    let ok = unsafe { !libc::localtime_r(&secs, tm.as_mut_ptr()).is_null() };
    if !ok {
        return None;
    }
    // SAFETY: `localtime_r` returned non-null, so `tm` is initialized.
    let tm = unsafe { tm.assume_init() };
    Some(format_stamp(
        i64::from(tm.tm_year) + 1900,
        u32::try_from(tm.tm_mon).ok()? + 1,
        u32::try_from(tm.tm_mday).ok()?,
        u32::try_from(tm.tm_hour).ok()?,
        u32::try_from(tm.tm_min).ok()?,
        u32::try_from(tm.tm_sec).ok()?,
    ))
}

#[cfg(windows)]
fn local_stamp() -> Option<String> {
    #[repr(C)]
    struct SystemTime {
        year: u16,
        month: u16,
        _day_of_week: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        _milliseconds: u16,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetLocalTime(out: *mut SystemTime);
    }

    let mut time = SystemTime {
        year: 0,
        month: 0,
        _day_of_week: 0,
        day: 0,
        hour: 0,
        minute: 0,
        second: 0,
        _milliseconds: 0,
    };
    // SAFETY: `GetLocalTime` writes the local civil time into `time` and
    // does not retain the pointer.
    unsafe { GetLocalTime(&mut time) };
    if time.month == 0 || time.day == 0 {
        return None;
    }
    Some(format_stamp(
        i64::from(time.year),
        u32::from(time.month),
        u32::from(time.day),
        u32::from(time.hour),
        u32::from(time.minute),
        u32::from(time.second),
    ))
}

#[cfg(not(any(target_os = "linux", windows)))]
fn local_stamp() -> Option<String> {
    None
}

impl RigWerkApp {
    /// Arm a take of whatever the endpoint is writing.  Does nothing while
    /// the bridge is not running, or while a take is already arming,
    /// recording, or finishing.  Returns immediately; the writer owns the file.
    pub(crate) fn start_recording(&mut self) {
        if self.engine.state != EngineState::Running {
            return;
        }
        let status = self.engine.recording_status();
        if matches!(
            status.phase,
            RecordPhase::Arming | RecordPhase::Recording | RecordPhase::Finishing
        ) {
            return;
        }
        let folder = resolved_record_folder(&self.record_folder);
        let path = recording_path(Path::new(&folder), &self.session_name, &take_stamp());
        let source = record_source(&self.followers);
        // A publish follower that has not opened (no cable, or publish off)
        // has no rate yet.  The main output is then what is actually playing.
        let armed = self.engine.arm_recording(path.clone(), source)
            || (!matches!(source, RecordSource::Clock)
                && self.engine.arm_recording(path.clone(), RecordSource::Clock));
        if armed {
            self.push_log(format!("Recording to {}.", path.display()), LogTone::Green);
        } else {
            let message = self
                .engine
                .recording_status()
                .error
                .unwrap_or_else(|| "Could not start recording.".to_owned());
            self.push_log(message, LogTone::Red);
        }
    }

    /// Ask the writer to finish.  Returns immediately and does not join it.
    pub(crate) fn stop_recording(&mut self) {
        self.engine.disarm_recording();
        self.push_log("Recording stopped.", LogTone::Muted);
    }
}

#[cfg(test)]
mod tests;
