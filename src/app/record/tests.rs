use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use super::{
    default_record_folder, folder_from_env, format_elapsed, overrun_label, record_source,
    record_source_for, recording_path, resolved_record_folder, sanitize_take_name, stamp_from_unix,
};
use crate::app::settings::FollowerPref;
use crate::audio::RecordSource;

fn follower(name: &str) -> FollowerPref {
    FollowerPref {
        name: name.to_owned(),
        ..FollowerPref::default()
    }
}

#[test]
fn record_source_uses_the_clock_without_a_cable() {
    let followers = [
        follower("Speakers (Realtek)"),
        follower("CABLE Output (VB-Audio Virtual Cable)"),
    ];
    assert!(matches!(
        record_source_for(true, &followers),
        RecordSource::Clock
    ));
    assert!(matches!(
        record_source_for(false, &followers),
        RecordSource::Clock
    ));
    assert!(matches!(record_source_for(true, &[]), RecordSource::Clock));
}

#[test]
fn record_source_uses_the_first_vb_cable_on_windows() {
    let followers = [
        follower("Speakers (Realtek)"),
        follower("CABLE Input (VB-Audio Virtual Cable)"),
        follower("cable input (vb-audio)"),
    ];
    assert!(matches!(
        record_source_for(true, &followers),
        RecordSource::Follower(1)
    ));
    assert!(matches!(
        record_source_for(false, &followers),
        RecordSource::Clock
    ));
    let only = [follower("CABLE Input (VB-Audio Virtual Cable)")];
    assert!(matches!(
        record_source_for(true, &only),
        RecordSource::Follower(0)
    ));

    // The publish slot keeps the name "RigWerk Out" after it is bound to the
    // cable.  The display name is not "CABLE Input".
    let published = [
        follower("Speakers (Realtek)"),
        FollowerPref {
            name: "RigWerk Out".into(),
            virtual_out: true,
            ..FollowerPref::default()
        },
        follower("CABLE Input (VB-Audio Virtual Cable)"),
    ];
    assert!(matches!(
        record_source_for(true, &published),
        RecordSource::Follower(1)
    ));
    assert!(matches!(
        record_source_for(false, &published),
        RecordSource::Clock
    ));

    let chosen = record_source(&followers);
    if cfg!(target_os = "windows") {
        assert!(matches!(chosen, RecordSource::Follower(1)));
    } else {
        assert!(matches!(chosen, RecordSource::Clock));
    }
}

#[test]
fn record_take_name_keeps_letters_numbers_space_and_hyphen() {
    assert_eq!(sanitize_take_name("My guitar rig"), "My guitar rig");
    assert_eq!(sanitize_take_name("Take-02"), "Take-02");
    assert_eq!(sanitize_take_name("  Gig #1 / live! "), "Gig 1  live");
    assert_eq!(sanitize_take_name("!!!"), "take");
    assert_eq!(sanitize_take_name("   "), "take");
    assert_eq!(sanitize_take_name(""), "take");
}

#[test]
fn record_folder_uses_home_then_userprofile_then_recordings() {
    assert_eq!(
        folder_from_env(
            Some(OsStr::new("/home/ada")),
            Some(OsStr::new("C:/Users/Ada"))
        ),
        Path::new("/home/ada").join("Recordings").to_string_lossy()
    );
    assert_eq!(
        folder_from_env(Some(OsStr::new("")), Some(OsStr::new("C:/Users/Ada"))),
        Path::new("C:/Users/Ada")
            .join("Recordings")
            .to_string_lossy()
    );
    assert_eq!(folder_from_env(None, Some(OsStr::new(""))), "recordings");
    assert_eq!(folder_from_env(None, None), "recordings");
}

#[test]
fn record_folder_blank_uses_the_default_and_a_set_folder_is_kept() {
    assert_eq!(resolved_record_folder(""), default_record_folder());
    assert_eq!(resolved_record_folder("   "), default_record_folder());
    assert_eq!(resolved_record_folder("  /takes  "), "/takes");
}

#[test]
fn record_stamp_from_unix_is_compact() {
    assert_eq!(stamp_from_unix(0), "19700101-000000");
    assert_eq!(stamp_from_unix(1_700_000_000), "20231114-221320");
}

#[test]
fn record_elapsed_and_overruns_are_short() {
    assert_eq!(format_elapsed(0, 0), "0:00");
    assert_eq!(format_elapsed(100, 0), "0:00");
    assert_eq!(format_elapsed(48_000, 48_000), "0:01");
    assert_eq!(format_elapsed(48_000 * 90, 48_000), "1:30");
    assert_eq!(format_elapsed(48_000 * (125 * 60 + 3), 48_000), "125:03");
    assert_eq!(overrun_label(1), "1 overrun");
    assert_eq!(overrun_label(3), "3 overruns");
}

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "rigwerk-take-{}-{}",
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
fn record_path_sanitises_the_session_and_does_not_create_the_file() {
    let dir = Scratch::new();
    let path = recording_path(&dir.0, "Gig #1!", "20261113-094700");
    assert_eq!(
        path.file_name().and_then(|name| name.to_str()),
        Some("Gig 1 20261113-094700.wav")
    );
    assert!(!path.exists());
    let empty = recording_path(&dir.0, "!!!", "20261113-094700");
    assert_eq!(
        empty.file_name().and_then(|name| name.to_str()),
        Some("take 20261113-094700.wav")
    );
    assert!(!empty.exists());
}

#[test]
fn record_path_appends_a_suffix_when_the_take_exists() {
    let dir = Scratch::new();
    let stamp = "20261113-094700";
    let first = recording_path(&dir.0, "take", stamp);
    assert_eq!(
        first.file_name().and_then(|name| name.to_str()),
        Some("take 20261113-094700.wav")
    );
    std::fs::write(&first, b"").expect("first take");
    let second = recording_path(&dir.0, "take", stamp);
    assert_eq!(
        second.file_name().and_then(|name| name.to_str()),
        Some("take 20261113-094700-2.wav")
    );
    assert!(!second.exists());
    std::fs::write(&second, b"").expect("second take");
    let third = recording_path(&dir.0, "take", stamp);
    assert_eq!(
        third.file_name().and_then(|name| name.to_str()),
        Some("take 20261113-094700-3.wav")
    );
    assert!(!third.exists());
}
