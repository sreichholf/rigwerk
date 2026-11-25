use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use super::{diagnostics_report_path, write_report};

#[test]
fn the_report_prefers_home_downloads_then_userprofile_then_temp() {
    let temp = Path::new("/tmp");
    let home = Some(OsStr::new("/home/ada"));
    let userprofile = Some(OsStr::new("C:/Users/Ada"));
    let stamp = 1_700_000_000;
    let name = "rigwerk-diagnostics-1700000000.txt";
    // Unix takes HOME first; Windows takes USERPROFILE first, because Git
    // Bash and MSYS set HOME to a POSIX path that `Path` reads as
    // drive-relative.
    assert_eq!(
        diagnostics_report_path(home, userprofile, temp, stamp, true),
        Path::new("/home/ada").join("Downloads").join(name)
    );
    assert_eq!(
        diagnostics_report_path(home, userprofile, temp, stamp, false),
        Path::new("C:/Users/Ada").join("Downloads").join(name)
    );
    // Whichever is set wins over the other one.
    assert_eq!(
        diagnostics_report_path(None, userprofile, temp, stamp, true),
        Path::new("C:/Users/Ada").join("Downloads").join(name)
    );
    assert_eq!(
        diagnostics_report_path(home, None, temp, stamp, false),
        Path::new("/home/ada").join("Downloads").join(name)
    );
    assert_eq!(
        diagnostics_report_path(None, None, temp, 42, true),
        temp.join("rigwerk-diagnostics-42.txt")
    );
}

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "rigwerk-diagnostics-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn the_report_creates_its_folder_and_writes_the_report() {
    let dir = Scratch::new();
    let path = diagnostics_report_path(Some(dir.0.as_os_str()), None, &dir.0, 1_700_000_000, true);
    assert_eq!(
        path.file_name().and_then(|name| name.to_str()),
        Some("rigwerk-diagnostics-1700000000.txt")
    );
    assert!(!dir.0.join("Downloads").exists());

    // The exported bytes are the report the sheet shows, not a literal.
    let text = crate::app::blank_app().diagnostics_text();
    let written = write_report(&path, &text, false).expect("write");
    assert_eq!(written, path);
    assert_eq!(std::fs::read_to_string(&written).expect("read"), text);
}

#[test]
fn a_taken_name_gets_a_suffix_instead_of_being_replaced() {
    let dir = Scratch::new();
    let wanted = dir.0.join("rigwerk-diagnostics-1700000000.txt");
    let first = write_report(&wanted, "first", false).expect("first write");
    let second = write_report(&wanted, "second", false).expect("second write");
    assert_eq!(first, wanted);
    assert_eq!(second, dir.0.join("rigwerk-diagnostics-1700000000-1.txt"));
    assert_eq!(std::fs::read_to_string(&first).expect("read"), "first");
    assert_eq!(std::fs::read_to_string(&second).expect("read"), "second");
}

#[cfg(unix)]
#[test]
fn a_report_written_owner_only_is_private() {
    use std::os::unix::fs::PermissionsExt;

    let dir = Scratch::new();
    let path = dir.0.join("rigwerk-diagnostics.txt");
    write_report(&path, "RigWerk diagnostics", true).expect("write");
    let mode = std::fs::metadata(&path)
        .expect("metadata")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}
