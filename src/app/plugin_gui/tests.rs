use super::*;

#[cfg(any(target_os = "windows", target_os = "linux"))]
#[test]
fn opens_and_closes_a_clap_window() {
    use std::path::PathBuf;

    let Ok(path) = std::env::var("RIGWERK_CLAP_FIXTURE") else {
        return;
    };
    let path = PathBuf::from(path);
    let candidates = crate::audio::clap::scan_bundle(&path).expect("scan the fixture");
    let Some(candidate) = candidates.first() else {
        return;
    };
    let app = crate::app::blank_app();
    let pref = crate::audio::PluginPref {
        path: path.display().to_string(),
        format: crate::audio::PluginFormat::Clap,
        class_id: Some(candidate.plugin_id.clone()),
        name: candidate.name.clone(),
        ..crate::audio::PluginPref::default()
    };
    app.engine
        .add_plugin(app.selected_path, pref)
        .expect("load the fixture");
    let Some(handle) = app.engine.plugin_handle(app.selected_path, 0) else {
        panic!("the fixture did not load");
    };
    if !handle.has_editor() {
        return;
    }
    let mut window =
        ClapGuiWindow::open(handle, &candidate.name, 1.0).expect("open the fixture window");
    assert!(!window.service(1.0));
    drop(window);
}

/// Opens AGain's VSTGUI editor the way the Plugins page does, runs a few
/// frames (the engine's service, which drives the run loop, and the
/// window's poll) and closes it.  SDK 3.8 VSTGUI takes its run loop from
/// the factory's host context, which offers one.  On Linux the editor's
/// run-loop entries are gone once the plugin is.  Needs
/// `RIGWERK_VST3_FIXTURES` (see `scripts/vst3-fixtures.sh`) and, on Linux,
/// an X server: CI runs it under `xvfb-run`.
#[cfg(any(target_os = "windows", target_os = "linux"))]
#[test]
fn vst3_fixture_opens_and_closes_a_vst3_window() {
    let Some(bundle) = crate::audio::vst3_fixture("again") else {
        return;
    };
    #[cfg(target_os = "linux")]
    assert!(
        std::env::var_os("DISPLAY").is_some(),
        "RIGWERK_VST3_FIXTURES is set but there is no X server; run under xvfb-run"
    );
    let _main = crate::audio::claim_main_thread();
    // Xvfb resets itself when its last client disconnects, and refuses
    // connections while it does: between the old editor window closing and
    // the reopened one connecting there would be none.  A desktop X server
    // never resets, so the test keeps a client of its own.
    #[cfg(target_os = "linux")]
    let _x_client = xcb::Connection::connect(None).expect("reach the X server");
    let mut app = crate::app::blank_app();
    assert!(app.add_plugin_from_bundle(&bundle, Some("AGain VST3")));
    let key = EffectKey {
        path: app.selected_path,
        effect: 0,
    };
    let views = app.engine.plugin_views(key.path);
    assert_eq!(views.len(), 1);
    assert_eq!(views[0].error, None);
    assert!(views[0].has_editor, "AGain has a VSTGUI editor");
    assert!(app.open_plugin_gui(key), "the editor did not open");
    for _ in 0..10 {
        app.engine.service_plugins();
        app.poll_plugin_gui();
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(app.plugin_gui.is_some(), "the editor closed on its own");
    #[cfg(target_os = "linux")]
    let (fds, timers) = crate::audio::vst3::run_loop_counts();
    #[cfg(target_os = "linux")]
    assert!(
        fds + timers > 0,
        "VSTGUI registered nothing with the run loop"
    );
    // AGain asks to be reloaded (as `kReloadComponent` does): the effect
    // is rebuilt, and its editor opens again on the new instance.
    let old = app
        .engine
        .plugin_handle(key.path, key.effect)
        .expect("loaded");
    old.request_restart_for_test(crate::audio::vst3::RELOAD_COMPONENT);
    let old_id = old.instance_id();
    drop(old);
    for _ in 0..5 {
        app.engine.service_plugins();
        app.poll_plugin_gui();
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let new = app
        .engine
        .plugin_handle(key.path, key.effect)
        .expect("reloaded");
    assert_ne!(new.instance_id(), old_id, "the effect was rebuilt");
    assert_eq!(app.engine.plugin_views(key.path)[0].error, None);
    let log: Vec<String> = app.log.iter().map(|entry| entry.text.clone()).collect();
    let gui = app
        .plugin_gui
        .as_ref()
        .unwrap_or_else(|| panic!("the editor opened again: {log:?}"));
    assert_eq!(gui.instance_id(), new.instance_id());
    drop(new);
    app.close_plugin_gui();
    assert!(app.plugin_gui.is_none());
    // Removing the effect unloads AGain: none of its handlers is left for
    // the run loop to call.
    app.engine.remove_plugin(key.path, key.effect);
    app.engine.service_plugins();
    #[cfg(target_os = "linux")]
    assert_eq!(crate::audio::vst3::run_loop_counts(), (0, 0));
}

#[test]
fn poll_with_no_editor_leaves_the_window_unset() {
    let mut app = crate::app::blank_app();
    app.poll_plugin_gui();
    assert!(app.plugin_gui.is_none());
}
