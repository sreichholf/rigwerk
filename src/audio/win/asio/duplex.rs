//! One ASIO driver carrying both directions: one callback moves input into
//! the ring and output out of it.  Either side's Panel request opens this
//! driver's control panel, on this thread, between callbacks.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::audio::mixer::OutputMixer;
use crate::audio::shared::{initial_target, Shared};
use crate::audio::types::StartConfig;

use super::callback::{
    wait_for_callbacks, AsioCallbackContext, AsioCallbackSlot, AsioSignal, CallbackRegistration,
    CallbackWait, CALLBACK_PANIC_MESSAGE, DRIVER_RESET_MESSAGE,
};
use super::output::silence_asio_output;
use super::panel::service_asio_panel;
use super::ASIO_INPUT_CALLBACKS;
use crate::audio::AsioPanelSide;

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
    // Cap at the map's addressable width so routing layer ② can reach every
    // open channel.  Unused channels follow the clock device's unused mode.
    let output_channels = (counts.out as usize).min(crate::audio::MAX_OUTPUT_CHANNELS);

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
    // The UI thread reloads plugins from the rate published above.

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

    let all_buffers: Vec<[*mut c_void; 2]> = match unsafe {
        driver.create_buffers(
            channel_ids.iter().copied(),
            buffer_frames,
            &raw const ASIO_INPUT_CALLBACKS,
        )
    } {
        Ok(buffers) => buffers.collect(),
        Err(error) => return Err(format!("ASIO buffers: {error}")),
    };
    let (input_buffers, output_buffers) = all_buffers.split_at(input_channels);
    silence_asio_output(output_buffers, &output_types, buffer_frames.max(1) as usize);

    let mut output_state = OutputMixer::new(rate, output_channels);
    output_state.follow_source_rate(&shared);
    output_state.publish_device_channels(&shared);
    output_state.prepare(buffer_frames.max(1) as usize);
    let signal = AsioSignal::new();
    let context = AsioCallbackContext::new(
        Arc::clone(&shared),
        Arc::clone(&signal),
        buffer_frames.max(1) as usize,
    )
    .with_input(input_buffers.to_vec(), input_types)
    .with_output(output_buffers.to_vec(), output_types, output_state);
    let registration =
        match CallbackRegistration::install(AsioCallbackSlot::Input, Box::new(context), signal) {
            Ok(registration) => registration,
            Err(error) => {
                let _ = driver.dispose_buffers();
                return Err(format!("ASIO callbacks: {error}"));
            }
        };

    if let Err(error) = driver.start() {
        let _ = driver.stop();
        let _ = driver.dispose_buffers();
        registration.clear();
        return Err(format!("Starting ASIO: {error}"));
    }

    shared.running.store(true, Ordering::Relaxed);
    shared.set_status(format!(
        "ASIO linked · {} in / {} out · {} smp @ {} Hz",
        input_channels, output_channels, buffer_frames, rate
    ));

    let mut followers = crate::audio::follower::FollowerOutputs::new(config.output_buffer, rate);
    let wait = wait_for_callbacks(
        registration.signal(),
        || stop.load(Ordering::Relaxed),
        || {
            followers.sync(&shared);
            // One driver serves both cards.  A stop already in flight must
            // not open the panel on its way out.
            !stop.load(Ordering::Relaxed)
                && service_asio_panel(
                    &driver,
                    &shared,
                    &[AsioPanelSide::Input, AsioPanelSide::Output],
                )
        },
    );
    match wait {
        CallbackWait::Stopped => {}
        CallbackWait::Reset | CallbackWait::Failed => {
            let _ = driver.stop();
            let _ = driver.dispose_buffers();
            registration.clear();
            drop(followers);
            return Err(if matches!(wait, CallbackWait::Reset) {
                DRIVER_RESET_MESSAGE
            } else {
                CALLBACK_PANIC_MESSAGE
            }
            .to_owned());
        }
        CallbackWait::Hung => {
            let _ = driver.stop();
            let _ = driver.dispose_buffers();
            registration.abandon();
            drop(followers);
            shared.set_device_lost(
                "The ASIO driver stopped responding — the interface may have been unplugged"
                    .to_owned(),
            );
            return Ok(());
        }
    }
    drop(followers);

    let _ = driver.stop();
    let _ = driver.dispose_buffers();
    registration.clear();
    Ok(())
}
