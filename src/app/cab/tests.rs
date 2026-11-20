use crate::app::blank_app;
use crate::app::nam::normalize_models_folder;
use crate::audio::CabControls;

#[test]
fn a_wav_file_uses_its_folder() {
    let dir = std::env::temp_dir().join(format!("rigwerk-cab-folder-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp folder");
    let file = dir.join("speaker.wav");
    std::fs::write(&file, b"x").expect("temp impulse");
    let got = normalize_models_folder(&file.to_string_lossy());
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(got, dir.to_string_lossy());
}

#[test]
fn the_cab_setter_clamps_the_high_cut() {
    let mut app = blank_app();
    app.set_path_cab(CabControls {
        high_cut_hz: 40.0,
        ..CabControls::default()
    });
    assert_eq!(app.paths[0].as_ref().unwrap().mix.cab.high_cut_hz, 1_000.0);
}
