//! One ASIO driver carrying both directions: one callback moves input into
//! the ring and output out of it.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use crate::audio::mixer::OutputMixer;
use crate::audio::shared::{initial_target, Shared};
use crate::audio::types::StartConfig;

use super::output::fill_asio_output;
use super::samples::read_asio_sample;
use super::{asio_switch_slot, ASIO_INPUT_CALLBACKS};

pub(crate) fn run_asio_duplex_bridge(
    config: &StartConfig,
    clsid: u128,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
    use azo::driver::Driver;
    use azo::dto::ChannelId;
    use azo::windows_core::GUID;
    use std::ffi::c_void;

    shared.set_status("Opening ASIO driver for input and output…");
    let host = azo::Host::new();
    let driver = host
        .create_driver(&GUID::from_u128(clsid))
        .map_err(|error| format!("Opening ASIO driver: {error}"))?;
    if !driver.init(None) {
        return Err("The ASIO driver refused to initialize".to_owned());
    }

    let _ = driver.set_sample_rate(config.sample_rate as f64);
    let rate = driver
        .get_sample_rate()
        .ok()
        .map(|rate| rate.round() as i64)
        .filter(|rate| (8_000..=768_000).contains(rate))
        .map(|rate| rate as u32)
        .unwrap_or(config.sample_rate);

    let counts = driver
        .channel_counts()
        .map_err(|error| format!("ASIO channels: {error}"))?;
    if counts.in_ < 1 {
        return Err("The ASIO driver has no input channels".to_owned());
    }
    if counts.out < 1 {
        return Err("The ASIO driver has no output channels".to_owned());
    }
    let input_channels = (counts.in_ as usize).min(2);
    let output_channels = (counts.out as usize).min(4);

    let buffer = driver
        .buffer_size()
        .map_err(|error| format!("ASIO buffer size: {error}"))?;
    let buffer_frames =
        if config.buffer_size >= buffer.min as usize && config.buffer_size <= buffer.max as usize {
            config.buffer_size as i32
        } else {
            buffer.preferred
        };
    shared.set_target_frames(initial_target(buffer_frames.max(1) as usize));
    // One driver, one clock: both sides run at the same rate and step.
    shared.set_source_rate(rate);
    shared.note_input_block(buffer_frames.max(1) as usize);
    shared.set_output(
        rate,
        buffer_frames.max(1) as u32,
        format!("ASIO in and out · one driver, {buffer_frames} smp"),
    );
    shared.prepare_plugins(rate);

    let mut input_types = Vec::new();
    let mut output_types = Vec::new();
    let mut channel_ids = Vec::new();
    for index in 0..input_channels {
        let id = ChannelId {
            input: true,
            index: index as i32,
        };
        let info = driver
            .channel_info(id)
            .map_err(|error| format!("ASIO input channel info: {error}"))?;
        input_types.push(info.sample_type);
        channel_ids.push(id);
    }
    for index in 0..output_channels {
        let id = ChannelId {
            input: false,
            index: index as i32,
        };
        let info = driver
            .channel_info(id)
            .map_err(|error| format!("ASIO output channel info: {error}"))?;
        output_types.push(info.sample_type);
        channel_ids.push(id);
    }

    let (sender, receiver) = mpsc::sync_channel::<i32>(2);
    if let Ok(mut slot) = asio_switch_slot().lock() {
        *slot = Some(sender);
    }

    let all_buffers: Vec<[*mut c_void; 2]> = match unsafe {
        driver.create_buffers(
            channel_ids.iter().copied(),
            buffer_frames,
            &raw const ASIO_INPUT_CALLBACKS,
        )
    } {
        Ok(buffers) => buffers.collect(),
        Err(error) => {
            if let Ok(mut slot) = asio_switch_slot().lock() {
                *slot = None;
            }
            return Err(format!("ASIO buffers: {error}"));
        }
    };
    let input_buffers = &all_buffers[..input_channels];
    let output_buffers = &all_buffers[input_channels..];

    if let Err(error) = driver.start() {
        if let Ok(mut slot) = asio_switch_slot().lock() {
            *slot = None;
        }
        let _ = driver.dispose_buffers();
        return Err(format!("Starting ASIO: {error}"));
    }

    shared.running.store(true, Ordering::Relaxed);
    shared.set_status(format!(
        "ASIO linked · {} in / {} out · {} smp @ {} Hz",
        input_channels, output_channels, buffer_frames, rate
    ));

    let mut output_state = OutputMixer::new(rate);
    let mut last_callback = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        match receiver.recv_timeout(Duration::from_millis(80)) {
            Ok(buffer_index) => {
                last_callback = Instant::now();
                let index = buffer_index.clamp(0, 1) as usize;
                let gain = shared.input_gain();
                let enabled_l = shared.input_enabled[0].load(Ordering::Relaxed);
                let enabled_r = shared.input_enabled[1].load(Ordering::Relaxed);
                let mut peak_l = 0.0f32;
                let mut peak_r = 0.0f32;
                for frame in 0..buffer_frames as usize {
                    let mut left = unsafe {
                        read_asio_sample(
                            input_buffers[0][index].cast::<u8>(),
                            frame,
                            input_types[0],
                        )
                    } * gain;
                    let mut right = if input_channels > 1 {
                        (unsafe {
                            read_asio_sample(
                                input_buffers[1][index].cast::<u8>(),
                                frame,
                                input_types[1],
                            )
                        }) * gain
                    } else {
                        left
                    };
                    if !enabled_l {
                        left = 0.0;
                    }
                    if !enabled_r {
                        right = 0.0;
                    }
                    peak_l = peak_l.max(left.abs());
                    peak_r = peak_r.max(right.abs());
                    shared.push_frame(left, right);
                }
                shared.in_l.store(peak_l.to_bits(), Ordering::Relaxed);
                shared.in_r.store(peak_r.to_bits(), Ordering::Relaxed);

                fill_asio_output(
                    &shared,
                    output_buffers,
                    &output_types,
                    buffer_frames as usize,
                    index,
                    &mut output_state,
                );
                let _ = driver.output_ready();
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if last_callback.elapsed() > Duration::from_secs(2) {
                    let _ = driver.stop();
                    if let Ok(mut slot) = asio_switch_slot().lock() {
                        *slot = None;
                    }
                    let _ = driver.dispose_buffers();
                    shared.set_device_lost(
                        "The ASIO driver stopped responding — the interface may have been unplugged"
                            .to_owned(),
                    );
                    return Ok(());
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    let _ = driver.stop();
    let _ = driver.dispose_buffers();
    if let Ok(mut slot) = asio_switch_slot().lock() {
        *slot = None;
    }
    Ok(())
}
