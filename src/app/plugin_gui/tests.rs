use super::*;
use std::path::PathBuf;

#[test]
fn opens_and_closes_a_clap_window() {
    let Ok(path) = std::env::var("RIGWERK_CLAP_FIXTURE") else {
        return;
    };
    let path = PathBuf::from(path);
    let candidates = crate::audio::clap::scan_bundle(&path).expect("scan the fixture");
    let Some(candidate) = candidates.first() else {
        return;
    };
    let plugin = Arc::new(Mutex::new(
        ClapPlugin::load_fresh(&path, &candidate.plugin_id, 48_000, 512).expect("load the fixture"),
    ));
    if !plugin
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .has_editor()
    {
        return;
    }
    let mut window =
        ClapGuiWindow::open(plugin.clone(), &candidate.name, 1.0).expect("open the fixture window");
    assert!(!window.service(1.0));
    drop(window);
}
