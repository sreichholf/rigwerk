use super::{normalize_amp_folder, paste_chord_edge};

#[test]
fn a_plain_folder_keeps_its_path() {
    assert_eq!(normalize_amp_folder("  /amps/nam  "), "/amps/nam");
}

#[test]
fn copy_as_path_quotes_are_stripped() {
    assert_eq!(
        normalize_amp_folder("\"D:\\Captures\\Nam\""),
        "D:\\Captures\\Nam"
    );
    assert_eq!(normalize_amp_folder("'/amps/nam'"), "/amps/nam");
}

#[test]
fn a_windows_file_uri_becomes_a_drive_path() {
    assert_eq!(
        normalize_amp_folder("file:///C:/Users/Ada/Captures"),
        "C:/Users/Ada/Captures"
    );
    assert_eq!(
        normalize_amp_folder("file://localhost/C:/Captures"),
        "C:/Captures"
    );
}

#[test]
fn a_file_uri_decodes_spaces() {
    assert_eq!(
        normalize_amp_folder("file:///home/ada/my%20amps"),
        "/home/ada/my amps"
    );
}

#[test]
fn ctrl_v_pastes_once_on_the_rising_edge() {
    let (fire, held) = paste_chord_edge(true, true, false);
    assert!(fire);
    assert!(held);
    let (fire, held) = paste_chord_edge(true, true, held);
    assert!(!fire);
    assert!(held);
    let (fire, held) = paste_chord_edge(true, false, held);
    assert!(!fire);
    assert!(!held);
}

#[test]
fn ctrl_v_while_inactive_does_not_paste_when_the_field_is_entered() {
    let (fire, held) = paste_chord_edge(false, true, false);
    assert!(!fire);
    let (fire, _) = paste_chord_edge(true, true, held);
    assert!(!fire);
}

#[test]
fn a_nam_file_uses_its_folder() {
    let dir = std::env::temp_dir().join(format!("rigwerk-amp-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp folder");
    let file = dir.join("amp.nam");
    std::fs::write(&file, b"x").expect("temp capture");
    let got = normalize_amp_folder(&file.to_string_lossy());
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(got, dir.to_string_lossy());
}
