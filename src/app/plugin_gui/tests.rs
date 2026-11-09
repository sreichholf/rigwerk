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

#[test]
fn poll_with_no_editor_leaves_the_window_unset() {
    let mut app = crate::app::blank_app();
    app.poll_plugin_gui();
    assert!(app.plugin_gui.is_none());
}

#[test]
fn editor_service_skips_a_busy_instance_lock() {
    let plugin = Mutex::new(Probe(0));
    let _held = plugin.lock().expect("lock");
    let ran = try_with_instance(&plugin, |_| {
        panic!("must not run while the audio callback holds the lock");
    });
    assert!(!ran);
}

#[test]
fn editor_service_recovers_a_poisoned_instance_lock() {
    let plugin = Mutex::new(Probe(0));
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _held = plugin.lock().expect("lock");
        panic!("instance panicked while locked");
    }));
    assert!(plugin.is_poisoned());
    assert!(try_with_instance(&plugin, |probe| probe.0 += 1));
    let probe = plugin.lock().unwrap_or_else(|poison| poison.into_inner());
    assert_eq!(probe.0, 1);
}

/// Stand-in for the plugin instance in the lock tests.
struct Probe(u8);
