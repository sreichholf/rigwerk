//! The UI thread never opens, writes, or joins the wav.
//! `start_recording` returns immediately.  "Record the click" is set
//! straight through and never re-armed.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::app::settings::ExtraOutputPref;
use crate::app::{LogTone, RigWerkApp};
use crate::audio::{is_vb_cable_render, EngineState, RecordPhase, RecordSource, RecordStatus};

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

impl RigWerkApp {
    /// An empty folder becomes the default when a take is armed and when
    /// preferences are saved.
    pub(crate) fn set_record_folder(&mut self, folder: String) {
        self.record_folder = folder;
    }
}

/// The directory is not created here.
pub(crate) fn resolved_record_folder(folder: &str) -> String {
    let trimmed = folder.trim();
    if trimmed.is_empty() {
        default_record_folder()
    } else {
        trimmed.to_owned()
    }
}

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

/// This only stats.  It does not create the file or the folder.
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

pub(crate) fn format_elapsed(frames: u64, rate: u32) -> String {
    if rate == 0 {
        return "0:00".to_owned();
    }
    let secs = frames / u64::from(rate);
    format!("{}:{:02}", secs / 60, secs % 60)
}

pub(crate) fn lost_samples_label(count: u64) -> String {
    match count {
        1 => "1 lost sample".to_owned(),
        n => format!("{n} lost samples"),
    }
}

/// On Windows the virtual output's saved name is "RigWerk Out", not
/// "CABLE Input", so a cable added as an ordinary extra output is still
/// matched by name.
pub(crate) fn record_source(extra_outputs: &[ExtraOutputPref]) -> RecordSource {
    record_source_for(cfg!(target_os = "windows"), extra_outputs)
}

fn record_source_for(windows: bool, extra_outputs: &[ExtraOutputPref]) -> RecordSource {
    if !windows {
        return RecordSource::Clock;
    }
    if let Some(index) = extra_outputs
        .iter()
        .position(|extra_output| extra_output.virtual_output)
    {
        return RecordSource::ExtraOutput(index);
    }
    match extra_outputs
        .iter()
        .position(|extra_output| is_vb_cable_render(&extra_output.name))
    {
        Some(index) => RecordSource::ExtraOutput(index),
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
// musl 1.2 deprecates the `time_t` alias; `localtime_r` still takes it.
#[allow(deprecated)]
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
    pub(crate) fn engine_recording_status(&self) -> RecordStatus {
        self.engine.recording_status()
    }

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
        let source = record_source(&self.extra_outputs);
        // A virtual output that has not opened (no cable, or switched off)
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

    /// The engine applies it from the next frame, including during a take.
    pub(crate) fn set_record_click(&mut self, with_click: bool) {
        self.record_click = with_click;
        self.engine.set_record_click(with_click);
    }

    pub(crate) fn stop_recording(&mut self) {
        self.engine.disarm_recording();
        self.push_log("Recording stopped.", LogTone::Muted);
    }
}

#[cfg(test)]
mod tests;
