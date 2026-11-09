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
fn only_a_running_callback_counts_as_audio_active() {
    assert!(!audio_active(EngineState::Idle));
    assert!(audio_active(EngineState::Starting));
    assert!(audio_active(EngineState::Running));
    assert!(audio_active(EngineState::Stopping));
    assert!(!audio_active(EngineState::Preview));
    assert!(!audio_active(EngineState::Error));
}

#[test]
fn the_run_loop_is_due_at_once_when_an_editor_opens() {
    let now = Instant::now();
    // The opening seed: one interval in the past, or now where the clock
    // cannot go that far back.  (The constructor must use this same seed.)
    assert!(run_loop_due(true, run_loop_start(now), now));
}

#[test]
fn an_open_editor_services_its_run_loop_less_often_while_audio_runs() {
    let now = Instant::now();
    // With no bridge running there is nothing to starve: every frame.
    assert!(run_loop_due(false, now, now));
    assert!(run_loop_due(
        false,
        now,
        now.checked_sub(Duration::from_millis(1)).unwrap_or(now)
    ));
    // While it runs, the interval has to pass first.
    assert!(!run_loop_due(true, now, now));
    assert!(!run_loop_due(
        true,
        now,
        now + VST3_RUN_LOOP_INTERVAL - Duration::from_millis(1)
    ));
    assert!(run_loop_due(true, now, now + VST3_RUN_LOOP_INTERVAL));
    assert!(run_loop_due(true, now, now + Duration::from_millis(100)));
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
