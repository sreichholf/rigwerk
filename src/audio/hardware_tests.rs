//! Tests that open real audio devices.  They are ignored by default and
//! only ever play silence.  Run with `cargo test -- --ignored --nocapture`.

/// Opens every WASAPI output REAL-style for a moment.  The ring is empty,
/// so only silence plays.  Run with `cargo test -- --ignored`.
#[cfg(target_os = "windows")]
#[test]
#[ignore = "opens the output devices"]
fn real_style_opens_the_outputs() {
    use cpal::traits::{DeviceTrait, HostTrait};
    use std::sync::Arc;

    let host = cpal::host_from_id(cpal::HostId::Wasapi).unwrap();
    for device in host.output_devices().unwrap() {
        let name = device
            .description()
            .map(|description| description.name().to_owned())
            .unwrap_or_default();
        let Ok(id) = device.id() else { continue };
        let shared = Arc::new(super::shared::Shared::new(48_000));
        match super::real_output::open(id.id(), 48_000, Arc::clone(&shared)) {
            Ok((stream, info)) => {
                println!("{name}: {}", info.describe(48_000));
                std::thread::sleep(std::time::Duration::from_millis(300));
                drop(stream);
                let snapshot = shared.snapshot();
                assert!(snapshot.error.is_none(), "{name}: {:?}", snapshot.error);
                assert!(
                    snapshot.frames > 0,
                    "{name}: the device never asked for audio"
                );
            }
            Err(reason) => println!("{name}: REAL-style refused, fallback applies: {reason}"),
        }
    }
}

/// Asks every WASAPI output which engine steps its driver allows, without
/// opening a stream.  Run with `cargo test probe_reports -- --ignored`.
#[cfg(target_os = "windows")]
#[test]
#[ignore = "asks the real output drivers"]
fn probe_reports_each_output_driver() {
    use cpal::traits::{DeviceTrait, HostTrait};

    let host = cpal::host_from_id(cpal::HostId::Wasapi).unwrap();
    for device in host.output_devices().unwrap() {
        let name = device
            .description()
            .map(|description| description.name().to_owned())
            .unwrap_or_default();
        let Ok(id) = device.id() else { continue };
        match super::real_output::probe(id.id()) {
            Ok(info) => println!(
                "{name}: smallest {} smp · default {} smp",
                info.min_frames, info.default_frames
            ),
            Err(reason) => println!("{name}: {reason}"),
        }
    }
}

/// Runs the real bridge for three seconds with the output muted, so only
/// silence plays.  Devices are picked by name:
/// `RIGWERK_IN` (an ASIO driver, or a system input) and `RIGWERK_OUT`
/// (an output), optionally `RIGWERK_OUT_BACKEND` (e.g. ASIO or WASAPI).
/// Run with `cargo test hardware_bridge -- --ignored --nocapture`.
#[test]
#[ignore = "opens real audio devices"]
fn hardware_bridge() {
    use super::{AudioEngine, EngineState, StartConfig};

    let input_name = std::env::var("RIGWERK_IN").expect("set RIGWERK_IN");
    let output_name = std::env::var("RIGWERK_OUT").expect("set RIGWERK_OUT");
    let output_backend = std::env::var("RIGWERK_OUT_BACKEND").ok();
    let inventory = super::scan_devices();
    let input = inventory
        .asio
        .iter()
        .chain(inventory.inputs.iter())
        .find(|device| !device.demo && device.name.contains(&input_name))
        .unwrap_or_else(|| {
            let names: Vec<String> = inventory
                .asio
                .iter()
                .chain(inventory.inputs.iter())
                .map(|device| format!("{} [{}]", device.name, device.backend))
                .collect();
            panic!("no input named {input_name}; inputs: {names:?}")
        })
        .clone();
    let output = inventory
        .outputs
        .iter()
        .find(|device| {
            !device.demo
                && device.name.contains(&output_name)
                && output_backend
                    .as_deref()
                    .is_none_or(|backend| device.backend.eq_ignore_ascii_case(backend))
        })
        .unwrap_or_else(|| {
            let names: Vec<String> = inventory
                .outputs
                .iter()
                .map(|device| format!("{} [{}]", device.name, device.backend))
                .collect();
            panic!("no output named {output_name}; outputs: {names:?}")
        })
        .clone();
    println!("input:  {} [{}]", input.name, input.backend);
    println!("output: {} [{}]", output.name, output.backend);

    let mut engine = AudioEngine::new(48_000);
    engine.set_mix(true, false, false, [true, true], [true, true], true);
    // RIGWERK_CUSHION: safe (default), low, or a manual size in ms.
    match std::env::var("RIGWERK_CUSHION").as_deref() {
        Ok("low") => engine.set_cushion(super::CushionMode::LowLatency, 10.0),
        Ok(ms) if ms.parse::<f32>().is_ok() => {
            engine.set_cushion(super::CushionMode::Manual, ms.parse().unwrap())
        }
        _ => engine.set_cushion(super::CushionMode::Safe, 10.0),
    }
    // RIGWERK_EQ=1 runs with every band boosted, to load the EQ fully.
    if std::env::var("RIGWERK_EQ").as_deref() == Ok("1") {
        let mut eq = super::EqSettings::default();
        for band in eq.bands.iter_mut() {
            band.gain_db = 6.0;
        }
        engine.set_eq(super::EqSlot::Pre, eq);
        engine.set_eq(super::EqSlot::Post, eq);
    }
    engine.start(StartConfig {
        asio_clsid: input.asio_clsid,
        input_id: input.asio_clsid.is_none().then(|| input.id.clone()),
        output_id: output.id.clone(),
        output_asio_clsid: output.asio_clsid,
        sample_rate: 48_000,
        buffer_size: 128,
        output_rate: None,
        output_buffer: 128,
        asio_wrapper: input.name.to_lowercase().contains("asio4all"),
        real_style_output: true,
    });
    let mut ring_seen = 0.0f32;
    for _ in 0..30 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        engine.poll();
        ring_seen = ring_seen.max(engine.snapshot().ring_fill);
    }
    let snapshot = engine.snapshot();
    let status = engine.shared.status.lock().unwrap().clone();
    println!("state:  {:?} · {status}", engine.state);
    println!(
        "rates:  in {} Hz → out {} Hz · step {} smp · {}",
        snapshot.input_rate,
        snapshot.output_rate,
        snapshot.output_step_frames,
        snapshot.output_mode
    );
    println!(
        "flow:   {} frames out · ring peak {:.0}% · target {} · underruns {}",
        snapshot.frames,
        ring_seen * 100.0,
        snapshot.target_frames,
        snapshot.underruns
    );
    engine.stop();
    assert!(
        snapshot.error.is_none(),
        "bridge error: {:?}",
        snapshot.error
    );
    assert_eq!(engine.state, EngineState::Idle);
    assert!(snapshot.frames > 0, "the output never asked for audio");
    assert!(ring_seen > 0.0, "the input never delivered audio");
}

/// Runs the IAudioClient3 stream at the driver's default step on the
/// default output, so the render loop is exercised even where no driver
/// offers a smaller step.  Only silence plays.
#[cfg(target_os = "windows")]
#[test]
#[ignore = "opens the default output device"]
fn real_style_stream_renders_on_the_default_output() {
    use cpal::traits::{DeviceTrait, HostTrait};
    use std::sync::Arc;

    let host = cpal::host_from_id(cpal::HostId::Wasapi).unwrap();
    let id = host.default_output_device().unwrap().id().unwrap();
    let shared = Arc::new(super::shared::Shared::new(48_000));
    let (stream, info) = super::real_output::open(id.id(), 48_000, Arc::clone(&shared))
        .expect("IAudioClient3 stream");
    std::thread::sleep(std::time::Duration::from_millis(500));
    drop(stream);
    let snapshot = shared.snapshot();
    println!(
        "{} · rendered {} frames",
        info.describe(48_000),
        snapshot.frames
    );
    assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
    // Half a second at the device rate, give or take the start-up.
    assert!(
        snapshot.frames as f32 > info.rate as f32 * 0.3,
        "{}",
        snapshot.frames
    );
}
