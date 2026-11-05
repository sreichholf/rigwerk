//! Playback through an ASIO driver, fed by any input.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::audio::mixer::OutputMixer;
use crate::audio::shared::{initial_target, Shared};
use crate::audio::types::StartConfig;

use cpal::traits::StreamTrait;

use super::callback::{
    wait_for_callbacks, AsioCallbackContext, AsioCallbackSlot, AsioSignal, CallbackRegistration,
    CallbackWait,
};
use super::duplex::run_asio_duplex_bridge;
use super::input::run_asio_input_only;
use super::samples::write_asio_sample;
use super::ASIO_OUTPUT_CALLBACKS;
use crate::audio::bridge::rate_path;
use crate::audio::devices::pick_input_config;
use crate::audio::input::{make_input_stream, open_input_device};

pub(crate) fn run_asio_output_bridge(
    config: &StartConfig,
    output_clsid: u128,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
    // One ASIO driver can carry both directions.  That is the cleanest case:
    // a single callback moves input into the ring and output out of it.
    if config.asio_clsid == Some(output_clsid) {
        return run_asio_duplex_bridge(config, output_clsid, shared, stop);
    }

    shared.set_status("Opening ASIO output…");
    let (worker, output_rate, output_channels) = open_asio_output_worker(
        output_clsid,
        config.output_rate.unwrap_or(config.sample_rate),
        config.output_buffer,
        Arc::clone(&shared),
        Arc::clone(&stop),
    )?;
    shared.prepare_plugins(output_rate);

    if let Some(input_clsid) = config.asio_clsid {
        shared.set_status("Opening ASIO input…");
        run_asio_input_only(
            input_clsid,
            config.sample_rate,
            config.buffer_size,
            output_rate,
            &shared,
            &stop,
        )?;
    } else {
        shared.set_status("Opening input…");
        let input_device = open_input_device(config)?;
        let input_config = pick_input_config(&input_device, config.sample_rate)?;
        let input_rate = input_config.sample_rate();
        let input_channels = input_config.channels() as usize;
        shared.set_source_rate(input_rate);
        let input_stream = make_input_stream(
            &input_device,
            input_config,
            input_channels,
            Arc::clone(&shared),
            config.buffer_size,
        )?;
        input_stream
            .play()
            .map_err(|error| format!("Starting input: {error}"))?;

        shared.running.store(true, Ordering::Relaxed);
        shared.set_status(format!(
            "Bridging {} ch · {} → ASIO output ({} ch)",
            input_channels,
            rate_path(input_rate, output_rate),
            output_channels
        ));

        while !stop.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(20));
        }
        drop(input_stream);
    }

    drop(worker);
    Ok(())
}

pub(crate) struct AsioOutputWorker {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Drop for AsioOutputWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

pub(crate) fn open_asio_output_worker(
    clsid: u128,
    sample_rate: u32,
    requested_buffer: usize,
    shared: Arc<Shared>,
    bridge_stop: Arc<AtomicBool>,
) -> Result<(AsioOutputWorker, u32, usize), String> {
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(u32, usize), String>>();
    let worker_stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&worker_stop);
    let handle = thread::Builder::new()
        .name("rigwerk-asio-output".to_owned())
        .spawn(move || {
            let result = run_asio_output_loop(
                clsid,
                sample_rate,
                requested_buffer,
                Arc::clone(&shared),
                Arc::clone(&thread_stop),
                Arc::clone(&bridge_stop),
                ready_tx,
            );
            if let Err(error) = result {
                shared.set_error(error);
            }
        })
        .map_err(|error| format!("Could not start ASIO output thread: {error}"))?;

    match ready_rx.recv_timeout(Duration::from_secs(6)) {
        Ok(Ok((rate, channels))) => Ok((
            AsioOutputWorker {
                stop: worker_stop,
                handle: Some(handle),
            },
            rate,
            channels,
        )),
        Ok(Err(error)) => {
            worker_stop.store(true, Ordering::Relaxed);
            let _ = handle.join();
            Err(error)
        }
        Err(error) => {
            worker_stop.store(true, Ordering::Relaxed);
            Err(format!("ASIO output did not start: {error}"))
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_asio_output_loop(
    clsid: u128,
    requested_rate: u32,
    requested_buffer: usize,
    shared: Arc<Shared>,
    worker_stop: Arc<AtomicBool>,
    bridge_stop: Arc<AtomicBool>,
    ready: mpsc::Sender<Result<(u32, usize), String>>,
) -> Result<(), String> {
    use azo::driver::Driver;
    use azo::dto::ChannelId;
    use azo::windows_core::GUID;
    use std::ffi::c_void;

    let host = azo::Host::new();
    let driver = host
        .create_driver(&GUID::from_u128(clsid))
        .map_err(|error| format!("Opening ASIO output driver: {error}"))?;
    if !driver.init(None) {
        let error = "The ASIO output driver refused to initialize".to_owned();
        let _ = ready.send(Err(error.clone()));
        return Err(error);
    }

    let _ = driver.set_sample_rate(requested_rate as f64);
    let rate = driver
        .get_sample_rate()
        .ok()
        .map(|rate| rate.round() as i64)
        .filter(|rate| (8_000..=768_000).contains(rate))
        .map(|rate| rate as u32)
        .unwrap_or(requested_rate);

    let counts = match driver.channel_counts() {
        Ok(counts) => counts,
        Err(error) => {
            let error = format!("ASIO output channels: {error}");
            let _ = ready.send(Err(error.clone()));
            return Err(error);
        }
    };
    if counts.out < 1 {
        let error = "The ASIO driver has no output channels".to_owned();
        let _ = ready.send(Err(error.clone()));
        return Err(error);
    }
    let output_channels = (counts.out as usize).min(4);

    let buffer = driver
        .buffer_size()
        .map_err(|error| format!("ASIO output buffer size: {error}"))?;
    let buffer_frames =
        if requested_buffer >= buffer.min as usize && requested_buffer <= buffer.max as usize {
            requested_buffer as i32
        } else {
            buffer.preferred
        };
    shared.set_target_frames(initial_target(buffer_frames.max(1) as usize));

    let mut channel_types = Vec::new();
    let mut channel_ids = Vec::new();
    for index in 0..output_channels {
        let id = ChannelId {
            input: false,
            index: index as i32,
        };
        let info = match driver.channel_info(id) {
            Ok(info) => info,
            Err(error) => {
                let error = format!("ASIO output channel info: {error}");
                let _ = ready.send(Err(error.clone()));
                return Err(error);
            }
        };
        channel_types.push(info.sample_type);
        channel_ids.push(id);
    }

    let mut mixer = OutputMixer::new(rate);
    mixer.follow_source_rate(&shared);
    mixer.prepare(buffer_frames.max(1) as usize);

    let buffers: Vec<[*mut c_void; 2]> = match unsafe {
        driver.create_buffers(
            channel_ids.iter().copied(),
            buffer_frames,
            &raw const ASIO_OUTPUT_CALLBACKS,
        )
    } {
        Ok(buffers) => buffers.collect(),
        Err(error) => {
            let error = format!("ASIO output buffers: {error}");
            let _ = ready.send(Err(error.clone()));
            return Err(error);
        }
    };
    silence_asio_output(&buffers, &channel_types, buffer_frames.max(1) as usize);

    let signal = AsioSignal::new();
    let context = AsioCallbackContext::new(
        Arc::clone(&shared),
        Arc::clone(&signal),
        buffer_frames.max(1) as usize,
    )
    .with_output(buffers, channel_types, mixer);
    let registration =
        match CallbackRegistration::install(AsioCallbackSlot::Output, Box::new(context), signal) {
            Ok(registration) => registration,
            Err(error) => {
                let _ = driver.dispose_buffers();
                let error = format!("ASIO output callbacks: {error}");
                let _ = ready.send(Err(error.clone()));
                return Err(error);
            }
        };

    if let Err(error) = driver.start() {
        let _ = driver.stop();
        let _ = driver.dispose_buffers();
        registration.clear();
        let error = format!("Starting ASIO output: {error}");
        let _ = ready.send(Err(error.clone()));
        return Err(error);
    }

    if ready.send(Ok((rate, output_channels))).is_err() {
        let _ = driver.stop();
        let _ = driver.dispose_buffers();
        registration.clear();
        return Ok(());
    }

    shared.set_output(
        rate,
        buffer_frames.max(1) as u32,
        format!("ASIO output · driver buffer {buffer_frames} smp"),
    );

    match wait_for_callbacks(
        registration.signal(),
        || worker_stop.load(Ordering::Relaxed) || bridge_stop.load(Ordering::Relaxed),
        || {
            let _ = driver.output_ready();
        },
    ) {
        CallbackWait::Stopped => {}
        CallbackWait::Hung => {
            let _ = driver.stop();
            let _ = driver.dispose_buffers();
            registration.clear();
            shared.set_device_lost(
                "The ASIO output stopped responding — the interface may have been unplugged"
                    .to_owned(),
            );
            return Ok(());
        }
    }

    let _ = driver.stop();
    let _ = driver.dispose_buffers();
    registration.clear();
    Ok(())
}

/// Fills both ping-pong buffers of every output channel with silence.  The
/// driver may start playing before the first callback fills a buffer.
pub(crate) fn silence_asio_output(
    buffers: &[[*mut std::ffi::c_void; 2]],
    channel_types: &[azo::sys::SampleType],
    buffer_frames: usize,
) {
    for (channel, buffer) in buffers.iter().enumerate() {
        let sample_type = channel_types
            .get(channel)
            .copied()
            .unwrap_or(azo::sys::SampleType::PCM_F32_LSB);
        for index in 0..2 {
            for frame in 0..buffer_frames {
                unsafe {
                    write_asio_sample(buffer[index].cast::<u8>(), frame, sample_type, 0.0);
                }
            }
        }
    }
}
