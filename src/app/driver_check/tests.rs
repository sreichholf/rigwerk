use super::*;
use crate::app::blank_app;
use crate::audio::DeviceItem;

fn device(id: &str, name: &str, backend: &str, asio_clsid: Option<u128>) -> DeviceItem {
    DeviceItem {
        id: id.to_owned(),
        name: name.to_owned(),
        backend: backend.to_owned(),
        channels: 2,
        demo: false,
        preferred: false,
        asio_clsid,
    }
}

fn actual(rate: u32, frames: u32, notes: &[&str], tone: DriverTone) -> DriverActual {
    DriverActual {
        rate,
        frames,
        notes: notes.iter().map(|note| (*note).to_owned()).collect(),
        tone,
    }
}

/// An app whose active input is the ASIO driver or the system input.
fn input_app(asio: bool) -> RigWerkApp {
    let mut app = blank_app();
    if asio {
        app.devices
            .inventory
            .asio
            .push(device("asio:Focusrite", "Focusrite", "ASIO", Some(1)));
        app.devices.asio_idx = 0;
        app.devices.use_asio_input = true;
    } else {
        app.devices
            .inventory
            .inputs
            .push(device("hw:Mic", "Mic", "ALSA", None));
        app.devices.input_idx = 0;
        app.devices.use_asio_input = false;
    }
    app
}

/// An app whose clock output is the system's: probed on Linux and, on
/// Windows, seen as a WASAPI endpoint.
fn output_app(id: &str) -> RigWerkApp {
    let backend = if cfg!(target_os = "windows") {
        "WASAPI"
    } else {
        "ALSA"
    };
    let mut app = blank_app();
    app.devices
        .inventory
        .outputs
        .push(device(id, "Speaker", backend, None));
    app.devices.output_idx = 0;
    app
}

fn asio_output_app(id: &str) -> RigWerkApp {
    let mut app = blank_app();
    app.devices
        .inventory
        .outputs
        .push(device(id, "Focusrite", "ASIO", Some(1)));
    app.devices.output_idx = 0;
    app
}

fn running_output(app: &mut RigWerkApp) {
    app.engine.state = EngineState::Running;
    app.snapshot.opened_output_mode = Some(OutputMode::Shared);
    // A running cpal/ASIO output publishes its rate and step as readings.
    app.snapshot.output_rate_reported = true;
    app.snapshot.output_step_reported = true;
}

/// The snapshot half of a running input: the driver published a rate.
fn running_input(app: &mut RigWerkApp) {
    app.engine.state = EngineState::Running;
    app.snapshot.input_rate_reported = true;
}

#[test]
fn the_host_prefix_is_not_part_of_the_endpoint() {
    let endpoint = "{0.0.0.00000000}.{9f0b0d09-0b0b-4a0b-9b0b-0b0b0b0b0b0b}";
    assert_eq!(wasapi_endpoint(&format!("WASAPI:{endpoint}")), endpoint);
}

#[test]
fn a_plain_endpoint_is_left_alone() {
    let endpoint = "{0.0.0.00000000}.{9f0b0d09-0b0b-4a0b-9b0b-0b0b0b0b0b0b}";
    assert_eq!(wasapi_endpoint(endpoint), endpoint);
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
#[test]
fn only_a_real_output_gets_a_stand_alone_check() {
    let mut real = output_app("hw:Speaker");
    real.sample_rate = 44_100;
    let plan = real
        .driver_probe_plan()
        .expect("a system output can be probed");
    assert_eq!(plan.id, "hw:Speaker");
    assert_eq!(plan.name, "Speaker");
    #[cfg(target_os = "linux")]
    {
        assert_eq!(
            plan.asked_rate, 44_100,
            "an output rate of 0 follows the input rate"
        );
        assert!(plan.low_latency, "the low-latency switch is captured");
        assert_eq!(plan.buffer_frames, 128, "the output buffer is captured");
    }

    #[cfg(target_os = "linux")]
    {
        let mut set = output_app("hw:Speaker");
        set.output_rate = 96_000;
        set.low_latency_output = false;
        set.output_buffer = 512;
        let plan = set.driver_probe_plan().expect("a system output is probed");
        assert_eq!(plan.id, "hw:Speaker");
        assert_eq!(plan.asked_rate, 96_000, "a set output rate wins");
        assert!(!plan.low_latency);
        assert_eq!(plan.buffer_frames, 512);
    }

    let mut demo = output_app("demo:speakers");
    demo.devices.inventory.outputs[0].demo = true;
    assert!(
        demo.driver_probe_plan().is_none(),
        "a demo row has no driver"
    );

    let asio = asio_output_app("asio:Focusrite");
    assert!(
        asio.driver_probe_plan().is_none(),
        "an ASIO driver reads itself while it runs"
    );

    let mut other = output_app("mme:Speakers");
    other.devices.inventory.outputs[0].backend = "MME".to_owned();
    #[cfg(target_os = "windows")]
    assert!(
        other.driver_probe_plan().is_none(),
        "only WASAPI outputs are probed on Windows"
    );
    #[cfg(target_os = "linux")]
    assert!(
        other.driver_probe_plan().is_some(),
        "Linux does not filter on the backend name"
    );
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
#[test]
fn a_manual_check_while_the_bridge_runs_only_says_to_stop_it() {
    let mut app = output_app("hw:Speaker");
    app.engine.state = EngineState::Running;

    app.start_output_driver_check(true);
    assert!(
        app.driver_probe_job.is_none(),
        "nothing starts while the bridge runs"
    );
    let entry = app.log.last().expect("the guard said why");
    assert_eq!(
        entry.text,
        "Stop the bridge before checking its output driver."
    );
    assert_eq!(entry.tone, LogTone::Amber);

    app.start_output_driver_check(false);
    assert_eq!(
        app.log.len(),
        1,
        "a quiet launch does not repeat the stop advice"
    );
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
#[test]
fn a_check_already_running_is_not_started_again() {
    let mut app = output_app("hw:Speaker");
    let (sender, receiver) = mpsc::channel();
    sender
        .send(DriverCheck {
            device_id: "hw:Speaker".to_owned(),
            name: "Sentinel".to_owned(),
            result: Err("sentinel".to_owned()),
            manual: false,
        })
        .expect("the sentinel is queued");
    app.driver_probe_job = Some(receiver);

    app.start_output_driver_check(true);
    assert!(app.log.is_empty(), "a busy check is not announced again");
    // A second start would replace the receiver and lose the sentinel, so the
    // queued check arriving is proof the original job was left alone.
    app.poll_output_driver_check();
    let check = app
        .driver_probe
        .as_ref()
        .expect("the original job delivered its check");
    assert_eq!(check.name, "Sentinel");
}

#[test]
fn an_input_reading_needs_a_running_bridge_a_reported_rate_and_a_first_block() {
    let mut app = input_app(false);
    app.snapshot.input_rate = 48_000;
    app.snapshot.input_block_frames = 128;
    assert!(app.input_actual().is_none(), "the bridge is not running");

    app.engine.state = EngineState::Running;
    app.snapshot.input_rate = 0;
    assert!(app.input_actual().is_none(), "no rate arrived yet");

    app.snapshot.input_rate = 48_000;
    app.snapshot.input_block_frames = 0;
    assert!(app.input_actual().is_none(), "no block arrived yet");

    // The block arrived, but the native path has only published the rate it
    // asked for.
    app.snapshot.input_block_frames = 128;
    assert!(
        app.input_actual().is_none(),
        "the rate is only the one asked for"
    );
    app.snapshot.input_rate_reported = true;
    assert!(app.input_actual().is_some(), "a reading at last");
}

#[test]
fn an_input_that_runs_as_asked_is_good() {
    let mut app = input_app(false);
    running_input(&mut app);
    app.sample_rate = 44_100;
    app.buffer_size = 256;
    app.snapshot.input_rate = 44_100;
    app.snapshot.input_block_frames = 256;

    let actual = app.input_actual().expect("the bridge runs");
    assert_eq!(actual.rate, 44_100);
    assert_eq!(actual.frames, 256);
    assert!(matches!(actual.tone, DriverTone::Good), "as asked is Good");
    assert!(actual.notes.is_empty(), "{:?}", actual.notes);
}

#[test]
fn an_input_at_another_rate_names_both_rates_and_where_to_change_it() {
    for asio in [false, true] {
        let mut app = input_app(asio);
        running_input(&mut app);
        app.sample_rate = 48_000;
        app.buffer_size = 128;
        app.snapshot.input_rate = 44_100;
        app.snapshot.input_block_frames = 128;

        let actual = app.input_actual().expect("the bridge runs");
        assert_eq!(actual.rate, 44_100, "the driver's rate is shown");
        assert_eq!(actual.frames, 128);
        assert!(
            matches!(actual.tone, DriverTone::Notice),
            "a converted rate is worth reading"
        );
        assert_eq!(actual.notes.len(), 1, "{:?}", actual.notes);
        let note = &actual.notes[0];
        assert!(
            note.starts_with("The input runs at 44.1 kHz instead of 48 kHz."),
            "{note}"
        );
        let remedy = if asio {
            "the ASIO driver's control panel"
        } else {
            system_rate_settings()
        };
        assert!(note.contains(remedy), "{note}");
        assert_eq!(
            note.contains("the ASIO driver's control panel"),
            asio,
            "each input points at the place that sets its rate: {note}"
        );
    }
}

#[test]
fn a_system_input_at_another_block_says_shared_mode_decided_it() {
    let mut app = input_app(false);
    running_input(&mut app);
    app.sample_rate = 48_000;
    app.buffer_size = 128;
    app.snapshot.input_rate = 48_000;
    app.snapshot.input_block_frames = 480;

    let actual = app.input_actual().expect("the bridge runs");
    assert_eq!(actual.frames, 480);
    assert!(
        matches!(actual.tone, DriverTone::Notice),
        "a different block is a limitation"
    );
    assert_eq!(actual.notes.len(), 1, "{:?}", actual.notes);
    let note = &actual.notes[0];
    assert!(
        note.starts_with(
            "This input delivers blocks of 480 samples (10.0 ms) whatever buffer is asked for"
        ),
        "{note}"
    );
    let smaller = if cfg!(target_os = "windows") {
        "an ASIO driver"
    } else if cfg!(target_os = "linux") {
        "a direct ALSA hardware input (hw: or plughw:)"
    } else {
        "JACK"
    };
    assert!(
        note.ends_with(&format!("For smaller blocks use {smaller}.")),
        "{note}"
    );
}

#[test]
fn an_asio_input_at_another_block_points_at_its_own_panel() {
    let mut app = input_app(true);
    running_input(&mut app);
    app.sample_rate = 48_000;
    app.buffer_size = 128;
    app.snapshot.input_rate = 48_000;
    app.snapshot.input_block_frames = 256;

    let actual = app.input_actual().expect("the bridge runs");
    assert_eq!(actual.frames, 256);
    assert!(matches!(actual.tone, DriverTone::Notice), "not as asked");
    assert_eq!(actual.notes.len(), 1, "{:?}", actual.notes);
    let note = &actual.notes[0];
    assert!(
        note.starts_with("The ASIO driver did not accept 128 samples and delivers 256 (5.3 ms)."),
        "{note}"
    );
    assert!(
        note.contains(
            "Set the buffer in the driver's own control panel; it lists the sizes it accepts."
        ),
        "{note}"
    );
}

#[test]
fn an_input_at_another_rate_and_block_gets_both_notes() {
    let mut app = input_app(false);
    running_input(&mut app);
    app.sample_rate = 48_000;
    app.buffer_size = 128;
    app.snapshot.input_rate = 44_100;
    app.snapshot.input_block_frames = 512;

    let actual = app.input_actual().expect("the bridge runs");
    assert!(matches!(actual.tone, DriverTone::Notice), "two differences");
    assert_eq!(actual.notes.len(), 2, "{:?}", actual.notes);
    assert!(
        actual.notes[0].starts_with("The input runs at 44.1 kHz instead of 48 kHz."),
        "{:?}",
        actual.notes
    );
    assert!(
        actual.notes[1].starts_with("This input delivers blocks of 512 samples"),
        "{:?}",
        actual.notes
    );
}

#[test]
fn a_running_output_whose_graph_has_not_reported_shows_no_driver_reading() {
    let mut app = output_app("hw:Speaker");
    app.engine.state = EngineState::Running;
    // What the native path publishes before the graph reports: the rate and
    // quantum asked for, with neither read from the stream.
    app.snapshot.output_rate = 48_000;
    app.snapshot.output_step_frames = 128;
    assert!(app.output_actual().is_none());

    app.snapshot.output_step_reported = true;
    assert!(
        app.output_actual().is_none(),
        "the rate is still the one asked for"
    );

    app.snapshot.output_rate_reported = true;
    assert!(app.output_actual().is_some(), "both readings arrived");
}

#[test]
fn a_running_output_reads_the_snapshot_not_the_request() {
    let mut app = output_app("hw:Speaker");
    running_output(&mut app);
    app.output_rate = 48_000;
    app.output_buffer = 128;
    app.snapshot.output_rate = 44_100;
    app.snapshot.output_step_frames = 64;

    let actual = app.output_actual().expect("the bridge runs");
    assert_eq!(actual.rate, 44_100, "the snapshot's rate, not output_rate");
    assert_eq!(actual.frames, 64, "the snapshot's step, not output_buffer");
    assert!(matches!(actual.tone, DriverTone::Notice));
    assert_eq!(actual.notes.len(), 2, "{:?}", actual.notes);
    assert!(
        actual.notes[0].starts_with("The output runs at 44.1 kHz instead of 48 kHz."),
        "{:?}",
        actual.notes
    );
    assert!(
        actual.notes[1]
            .starts_with("This output works in steps of 64 samples (1.5 ms), not the 128 chosen"),
        "{:?}",
        actual.notes
    );
}

#[test]
fn a_running_output_at_another_rate_says_who_decided_it() {
    let mut system = output_app("hw:Speaker");
    running_output(&mut system);
    system.output_rate = 48_000;
    system.output_buffer = 128;
    system.snapshot.output_rate = 44_100;
    system.snapshot.output_step_frames = 128;

    let reading = system.output_actual().expect("the bridge runs");
    assert_eq!(reading.rate, 44_100);
    assert!(matches!(reading.tone, DriverTone::Notice));
    assert_eq!(
        reading.notes,
        vec!["The output runs at 44.1 kHz instead of 48 kHz. Shared outputs mix at the rate set for the device in the system. RigWerk converts to it.".to_owned()]
    );

    let mut asio = asio_output_app("asio:Focusrite");
    running_output(&mut asio);
    asio.output_rate = 48_000;
    asio.output_buffer = 128;
    asio.snapshot.output_rate = 44_100;
    asio.snapshot.output_step_frames = 128;

    let reading = asio.output_actual().expect("the bridge runs");
    assert!(matches!(reading.tone, DriverTone::Notice));
    assert_eq!(
        reading.notes,
        vec!["The output runs at 44.1 kHz instead of 48 kHz. The ASIO driver decides its rate. RigWerk converts to it.".to_owned()]
    );
}

#[test]
fn an_output_that_follows_the_input_names_both_rates() {
    let mut app = output_app("hw:Speaker");
    running_output(&mut app);
    app.output_rate = 0;
    app.output_buffer = 128;
    app.sample_rate = 48_000;
    app.snapshot.input_rate = 48_000;
    app.snapshot.output_rate = 44_100;
    app.snapshot.output_step_frames = 128;

    let reading = app.output_actual().expect("the bridge runs");
    assert!(matches!(reading.tone, DriverTone::Notice));
    assert_eq!(
        reading.notes,
        vec![format!(
            "The output runs at 44.1 kHz while the input runs at 48 kHz, so RigWerk converts between them. Setting both devices to the same rate in {} avoids converting.",
            system_rate_settings()
        )]
    );
}

#[test]
fn a_real_style_output_is_quiet_unless_its_step_is_the_drivers_minimum() {
    let mut plain = output_app("hw:Speaker");
    running_output(&mut plain);
    plain.output_rate = 44_100;
    plain.output_buffer = 64;
    plain.snapshot.output_rate = 44_100;
    plain.snapshot.output_step_frames = 128;
    plain.snapshot.output_mode = "REAL-style · 128 smp step (2.9 ms)".to_owned();
    plain.snapshot.opened_output_mode = Some(OutputMode::RealStyle);

    let reading = plain.output_actual().expect("the bridge runs");
    assert_eq!(reading.frames, 128);
    assert!(
        matches!(reading.tone, DriverTone::Good),
        "a REAL-style step above the chosen buffer is not a mixing note"
    );
    assert!(reading.notes.is_empty(), "{:?}", reading.notes);

    let mut held = output_app("hw:Speaker");
    running_output(&mut held);
    held.output_rate = 44_100;
    held.output_buffer = 64;
    held.snapshot.output_rate = 44_100;
    held.snapshot.output_step_frames = 128;
    held.snapshot.output_mode =
        "REAL-style · 128 smp step (2.9 ms) · another app holds the device at this step (driver minimum 64 smp)"
            .to_owned();
    held.snapshot.opened_output_mode = Some(OutputMode::RealStyle);

    let reading = held.output_actual().expect("the bridge runs");
    assert!(matches!(reading.tone, DriverTone::Notice));
    assert_eq!(reading.notes.len(), 1, "{:?}", reading.notes);
    assert_eq!(
        reading.notes[0],
        "Shared, smallest buffer is on, but another app holds this device at 128 samples (2.9 ms), above the driver's minimum. Close other audio apps on this device and restart the bridge."
    );

    let mut minimum = output_app("hw:Speaker");
    running_output(&mut minimum);
    minimum.output_rate = 44_100;
    minimum.output_buffer = 64;
    minimum.snapshot.output_rate = 44_100;
    minimum.snapshot.output_step_frames = 128;
    minimum.snapshot.output_mode =
        "REAL-style · 128 smp step (2.9 ms) · no smaller step: the driver's minimum is its default"
            .to_owned();
    minimum.snapshot.opened_output_mode = Some(OutputMode::RealStyle);

    let reading = minimum.output_actual().expect("the bridge runs");
    assert!(matches!(reading.tone, DriverTone::Notice));
    assert_eq!(reading.notes.len(), 1, "{:?}", reading.notes);
    assert!(
        reading.notes[0].starts_with(
            "This output offers only 2.9 ms steps in shared mode: that is the driver's own minimum"
        ),
        "{}",
        reading.notes[0]
    );
    assert!(
        reading.notes[0].contains("install Windows' in-box \"High Definition Audio Device\""),
        "{}",
        reading.notes[0]
    );
}

#[test]
fn an_asio_output_names_the_buffer_its_driver_chose() {
    let mut app = asio_output_app("asio:Focusrite");
    running_output(&mut app);
    app.output_rate = 48_000;
    app.output_buffer = 128;
    app.snapshot.output_rate = 48_000;
    app.snapshot.output_step_frames = 64;

    let reading = app.output_actual().expect("the bridge runs");
    assert_eq!(reading.frames, 64, "the driver's step, not output_buffer");
    assert!(matches!(reading.tone, DriverTone::Notice), "not as asked");
    assert_eq!(
        reading.notes,
        vec!["The ASIO output driver did not accept 128 samples and uses 64 (1.3 ms). Set the buffer in the driver's own control panel.".to_owned()]
    );
}

#[test]
fn a_low_latency_output_is_quiet_when_its_step_differs() {
    let mut app = output_app("hw:Speaker");
    running_output(&mut app);
    app.output_rate = 48_000;
    app.output_buffer = 256;
    app.snapshot.output_rate = 48_000;
    app.snapshot.output_step_frames = 64;
    app.snapshot.opened_output_mode = Some(OutputMode::LowLatency);

    let reading = app.output_actual().expect("the bridge runs");
    assert_eq!(reading.frames, 64);
    assert!(
        matches!(reading.tone, DriverTone::Good),
        "low latency asks for a different step on purpose"
    );
    assert!(reading.notes.is_empty(), "{:?}", reading.notes);
}

#[test]
fn an_exclusive_output_does_not_treat_its_step_as_shared_mixing() {
    let mut app = output_app("hw:Speaker");
    running_output(&mut app);
    app.output_rate = 44_100;
    app.output_buffer = 128;
    app.snapshot.output_rate = 44_100;
    app.snapshot.output_step_frames = 64;
    app.snapshot.output_mode = "Exclusive · 64 smp step (1.5 ms)".to_owned();
    app.snapshot.opened_output_mode = Some(OutputMode::Exclusive);

    let reading = app.output_actual().expect("the bridge runs");
    assert_eq!(reading.frames, 64);
    assert!(
        matches!(reading.tone, DriverTone::Good),
        "exclusive mode uses the driver's own period"
    );
    assert!(reading.notes.is_empty(), "{:?}", reading.notes);
}

#[test]
fn a_refused_mode_is_explained_at_the_end_of_the_step_note() {
    let mut exclusive = output_app("hw:Speaker");
    running_output(&mut exclusive);
    exclusive.output_rate = 48_000;
    exclusive.output_buffer = 64;
    exclusive.snapshot.output_rate = 44_100;
    exclusive.snapshot.output_step_frames = 128;
    exclusive.snapshot.output_mode =
        "Standard output · Exclusive refused: The device is in use".to_owned();

    let reading = exclusive.output_actual().expect("the bridge runs");
    assert_eq!(
        reading.notes.len(),
        2,
        "the rate first, then the step: {:?}",
        reading.notes
    );
    assert!(
        reading.notes[0].starts_with("The output runs at 44.1 kHz instead of 48 kHz."),
        "{:?}",
        reading.notes
    );
    assert_eq!(
        reading.notes[1],
        "This output works in steps of 128 samples (2.9 ms), not the 64 chosen: the system mixes shared outputs in its own steps. Exclusive mode was refused: The device is in use."
    );

    let mut shared = output_app("hw:Speaker");
    running_output(&mut shared);
    shared.output_rate = 44_100;
    shared.output_buffer = 64;
    shared.snapshot.output_rate = 44_100;
    shared.snapshot.output_step_frames = 128;
    shared.snapshot.output_mode =
        "Standard output · Shared, smallest buffer refused: no adjustable engine step".to_owned();

    let reading = shared.output_actual().expect("the bridge runs");
    assert_eq!(reading.notes.len(), 1, "{:?}", reading.notes);
    assert_eq!(
        reading.notes[0],
        "This output works in steps of 128 samples (2.9 ms), not the 64 chosen: the system mixes shared outputs in its own steps. Shared, smallest buffer could not go lower: no adjustable engine step."
    );
}

#[test]
fn a_stopped_output_uses_a_matching_stand_alone_check() {
    for manual in [false, true] {
        let mut app = output_app("hw:Speaker");
        app.driver_probe = Some(DriverCheck {
            device_id: "hw:Speaker".to_owned(),
            name: "Speaker".to_owned(),
            result: Ok(actual(
                48_000,
                96,
                &["ALSA reports periods from 32 to 8192 frames."],
                DriverTone::Good,
            )),
            manual,
        });

        let reading = app.output_actual().expect("the check answered");
        assert_eq!(reading.rate, 48_000);
        assert_eq!(reading.frames, 96);
        assert!(matches!(reading.tone, DriverTone::Good));
        assert_eq!(
            reading.notes,
            vec!["ALSA reports periods from 32 to 8192 frames.".to_owned()],
            "manual only affects the log, not the reading"
        );
    }
}

#[test]
fn a_failed_check_falls_back_to_the_last_running_reading_and_says_so() {
    let mut app = output_app("hw:Speaker");
    app.driver_probe = Some(DriverCheck {
        device_id: "hw:Speaker".to_owned(),
        name: "Speaker".to_owned(),
        result: Err("The selected output disappeared".to_owned()),
        manual: true,
    });
    app.last_output_driver = Some((
        "hw:Speaker".to_owned(),
        actual(
            48_000,
            128,
            &["This output works in steps of 128 samples."],
            DriverTone::Good,
        ),
    ));

    let reading = app.output_actual().expect("the last reading is kept");
    assert_eq!(reading.rate, 48_000);
    assert_eq!(reading.frames, 128);
    assert!(
        matches!(reading.tone, DriverTone::Good),
        "the fallback note alone does not change the reading's tone"
    );
    assert_eq!(
        reading.notes,
        vec![
            "This output works in steps of 128 samples.".to_owned(),
            "This is the last reading from a running bridge. The stand-alone check could not read the output just now.".to_owned(),
        ]
    );
}

#[test]
fn a_running_bridge_without_a_first_block_shows_the_stand_alone_check() {
    let mut app = output_app("hw:Speaker");
    app.engine.state = EngineState::Running;
    app.driver_probe = Some(DriverCheck {
        device_id: "hw:Speaker".to_owned(),
        name: "Speaker".to_owned(),
        result: Ok(actual(
            48_000,
            96,
            &["ALSA reports periods from 32 to 8192 frames."],
            DriverTone::Good,
        )),
        manual: false,
    });

    let reading = app
        .output_actual()
        .expect("the check's reading is shown while no block has arrived");
    assert_eq!(reading.rate, 48_000);
    assert_eq!(reading.frames, 96);
    assert!(matches!(reading.tone, DriverTone::Good));
    assert_eq!(
        reading.notes,
        vec!["ALSA reports periods from 32 to 8192 frames.".to_owned()],
        "a successful check needs no 'last reading' caveat"
    );
}

#[test]
fn a_running_bridge_without_a_first_block_and_a_failed_check_shows_the_last_reading() {
    let mut app = output_app("hw:Speaker");
    app.engine.state = EngineState::Running;
    app.driver_probe = Some(DriverCheck {
        device_id: "hw:Speaker".to_owned(),
        name: "Speaker".to_owned(),
        result: Err("The selected output disappeared".to_owned()),
        manual: false,
    });
    app.last_output_driver = Some((
        "hw:Speaker".to_owned(),
        actual(48_000, 128, &[], DriverTone::Good),
    ));

    let reading = app.output_actual().expect("the last reading is kept");
    assert_eq!(reading.rate, 48_000);
    assert_eq!(reading.frames, 128);
    assert_eq!(
        reading.notes,
        vec!["This is the last reading from a running bridge. The stand-alone check could not read the output just now.".to_owned()]
    );
}

#[test]
fn a_stopped_output_with_neither_a_check_nor_a_reading_has_nothing_to_show() {
    assert!(output_app("hw:Speaker").output_actual().is_none());

    let mut failed = output_app("hw:Speaker");
    failed.driver_probe = Some(DriverCheck {
        device_id: "hw:Speaker".to_owned(),
        name: "Speaker".to_owned(),
        result: Err("No usable output configuration".to_owned()),
        manual: true,
    });
    assert!(
        failed.output_actual().is_none(),
        "a failed check is not a driver failure to show"
    );
}

#[test]
fn a_check_for_another_output_is_not_used() {
    let mut app = output_app("hw:Speaker");
    app.driver_probe = Some(DriverCheck {
        device_id: "hw:Other".to_owned(),
        name: "Other".to_owned(),
        result: Ok(actual(48_000, 96, &[], DriverTone::Good)),
        manual: false,
    });

    assert!(app.output_actual().is_none());
}

#[test]
fn a_reading_for_another_output_is_not_used() {
    let mut app = output_app("hw:Speaker");
    app.last_output_driver = Some((
        "hw:Other".to_owned(),
        actual(48_000, 128, &[], DriverTone::Good),
    ));

    assert!(app.output_actual().is_none());
}
