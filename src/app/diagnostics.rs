//! The diagnostics export: where the file goes, what it is called, and the
//! write.
//!
//! The sheet builds the report's text; this module owns the folder policy
//! and the filesystem calls, so the UI never chooses a path or writes a
//! file.  On Windows `%USERPROFILE%\Downloads` comes first, because Git Bash
//! and MSYS set `HOME` to a POSIX path that `Path` reads as drive-relative;
//! on Unix `$HOME/Downloads` comes first.  The other one follows, then the
//! temp directory.  The writer creates the folder and never replaces a
//! report another export already wrote.

use std::ffi::OsStr;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::app::RigWerkApp;

/// How many `-N` suffixes an export tries before it gives up.
const NAME_ATTEMPTS: u32 = 100;

/// `rigwerk-diagnostics-<stamp>.txt` in the Downloads folder of `home` or
/// `userprofile`, whichever `home_first` puts first, or in `temp` when
/// neither is set.  `stamp` is a unix second; the writer adds a suffix when
/// that second already has a file.
pub(crate) fn diagnostics_report_path(
    home: Option<&OsStr>,
    userprofile: Option<&OsStr>,
    temp: &Path,
    stamp: u64,
    home_first: bool,
) -> PathBuf {
    let (first, second) = if home_first {
        (home, userprofile)
    } else {
        (userprofile, home)
    };
    first
        .or(second)
        .map(|base| Path::new(base).join("Downloads"))
        .unwrap_or_else(|| temp.to_path_buf())
        .join(format!("rigwerk-diagnostics-{stamp}.txt"))
}

/// Create the parent folder, then write `text` to `path` without replacing a
/// file another export already wrote.  A taken name becomes `-1`, `-2` ...
/// before the extension.  `owner_only` keeps the file to its owner (0600) on
/// Unix, where a report in the temp directory is readable by everyone
/// otherwise.  Returns the path written.
fn write_report(path: &Path, text: &str, owner_only: bool) -> Result<PathBuf, String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("Could not create {}: {error}", parent.display()))?;
        }
    }
    for attempt in 0..=NAME_ATTEMPTS {
        let name = if attempt == 0 {
            path.to_path_buf()
        } else {
            numbered(path, attempt)
        };
        match create_new(&name, text, owner_only) {
            Ok(()) => return Ok(name),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("Could not write {}: {error}", name.display())),
        }
    }
    Err(format!(
        "Could not write {}: every name through -{NAME_ATTEMPTS} is taken",
        path.display()
    ))
}

/// Create `path` and write `text`; never open a file that already exists.
fn create_new(path: &Path, text: &str, owner_only: bool) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        if owner_only {
            // The temp directory is shared; a report names devices and the log.
            options.mode(0o600);
        }
    }
    #[cfg(not(unix))]
    let _ = owner_only;
    options.open(path)?.write_all(text.as_bytes())
}

/// `rigwerk-diagnostics-42-1.txt` for `rigwerk-diagnostics-42.txt`.
fn numbered(path: &Path, attempt: u32) -> PathBuf {
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "rigwerk-diagnostics".to_owned());
    let name = match path.extension() {
        Some(extension) => format!("{stem}-{attempt}.{}", extension.to_string_lossy()),
        None => format!("{stem}-{attempt}"),
    };
    path.with_file_name(name)
}

impl RigWerkApp {
    /// Write `text` to the default report path.  Returns the path written,
    /// or a message for the activity log.
    pub(crate) fn save_diagnostics_report(&self, text: &str) -> Result<PathBuf, String> {
        let home = std::env::var_os("HOME");
        let userprofile = std::env::var_os("USERPROFILE");
        let temp = std::env::temp_dir();
        let path = diagnostics_report_path(
            home.as_deref(),
            userprofile.as_deref(),
            &temp,
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_secs())
                .unwrap_or(0),
            !cfg!(windows),
        );
        let owner_only = home.is_none() && userprofile.is_none();
        write_report(&path, text, owner_only)
    }
}

#[cfg(test)]
mod tests;
