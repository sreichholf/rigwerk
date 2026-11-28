//! This thread creates the input driver, so its panel is opened here,
//! between callbacks, not from the UI thread.  A standalone input is
//! prepared before the output opens and started after it, so the output
//! knows the source rate before its first callback.
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use azo::driver::Driver;
use azo::dto::ChannelId;
use azo::windows_core::GUID;

use crate::audio::shared::Shared;
use crate::audio::types::StartConfig;

use super::callback::{
    wait_for_callbacks, AsioCallbackContext, AsioCallbackSlot, AsioSignal, CallbackRegistration,
    CallbackWait, ASIO_HOST, CALLBACK_PANIC_MESSAGE, DRIVER_RESET_MESSAGE,
};
use super::panel::service_asio_panel;
use super::ASIO_INPUT_CALLBACKS;
use crate::audio::bridge::rate_path;
use crate::audio::extra_output::ExtraOutputOutputs;
use crate::audio::output::open_output;
use crate::audio::AsioPanelSide;

pub(crate) fn run_asio_bridge(
    config: &StartConfig,
    clsid: u128,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
    let requested_rate = config.sample_rate;
    let requested_buffer = config.buffer_size;
    let asio_wrapper = config.asio_wrapper;

    shared.set_status("Opening ASIO driver…");
    let host = azo::Host::new();
    let driver = host
        .create_driver(&GUID::from_u128(clsid))
        .map_err(|error| format!("Opening ASIO driver: {error}"))?;
    if !driver.init(None) {
        return Err("The ASIO driver refused to initialize".to_owned());
    }

    // The driver decides the clock.  If it refuses the requested rate we
    // follow the rate it is actually running at.
    let _ = driver.set_sample_rate(requested_rate as f64);
    let asio_rate = driver
        .get_sample_rate()
        .ok()
        .map(|rate| rate.round() as i64)
        .filter(|rate| (8_000..=768_000).contains(rate))
        .map(|rate| rate as u32)
        .unwrap_or(requested_rate);
    shared.set_source_rate(asio_rate);

    shared.set_status("Opening independent output…");
    let output = match open_output(
        &config.output_id,
        config.output_rate.unwrap_or(asio_rate),
        &shared,
        config.output_buffer,
        config.low_latency_request,
        config.exclusive_output,
    ) {
        Ok(opened) => opened,
        Err(error) if asio_wrapper => {
            let _ = driver.dispose_buffers();
            return Err(format!(
                    "{error}. ASIO4ALL drives this sound card itself — choose an output on different hardware, or untick this card in the ASIO4ALL panel."
                ));
        }
        Err(error) => return Err(error),
    };
    // `open_output` already published the output rate.  Plugins reload on
    // the UI thread; this bridge thread must not load or destroy them.

    let counts = driver
        .channel_counts()
        .map_err(|error| format!("ASIO channels: {error}"))?;
    if counts.in_ < 1 {
        return Err("The ASIO driver has no input channels".to_owned());
    }

    let input_channels = (counts.in_ as usize).min(2);
    let buffer = driver
        .buffer_size()
        .map_err(|error| format!("ASIO buffer size: {error}"))?;
    let buffer_frames =
        if requested_buffer >= buffer.min as usize && requested_buffer <= buffer.max as usize {
            requested_buffer as i32
        } else {
            buffer.preferred
        };

    let mut channel_types = Vec::new();
    let mut channel_ids = Vec::new();
    for index in 0..input_channels {
        let id = ChannelId {
            input: true,
            index: index as i32,
        };
        let info = driver
            .channel_info(id)
            .map_err(|error| format!("ASIO channel info: {error}"))?;
        channel_types.push(info.sample_type);
        channel_ids.push(id);
    }

    let buffers: Vec<[*mut c_void; 2]> = unsafe {
        driver
            .create_buffers(
                channel_ids.iter().copied(),
                buffer_frames,
                &raw const ASIO_INPUT_CALLBACKS,
            )
            .map_err(|error| format!("ASIO buffers: {error}"))?
            .collect()
    };

    let signal = AsioSignal::new();
    let context = AsioCallbackContext::new(
        Arc::clone(&shared),
        Arc::clone(&signal),
        buffer_frames.max(1) as usize,
    )
    .with_input(buffers, channel_types);
    let registration = match CallbackRegistration::install(
        &ASIO_HOST,
        AsioCallbackSlot::Input,
        Box::new(context),
        signal,
    ) {
        Ok(registration) => registration,
        Err(error) => {
            let _ = driver.dispose_buffers();
            return Err(format!("ASIO callbacks: {error}"));
        }
    };

    shared.set_status("Waiting for ASIO clock…");
    if let Err(error) = driver.start() {
        let _ = driver.stop();
        let _ = driver.dispose_buffers();
        registration.clear();
        return Err(if asio_wrapper {
            format!(
                "Starting ASIO: {error}. The ASIO4ALL-style driver may be blocked by the output device — choose an output on different hardware."
            )
        } else {
            format!("Starting ASIO: {error}")
        });
    }
    shared.note_input_block(buffer_frames as usize);
    shared.running.store(true, Ordering::Relaxed);
    shared.set_status(format!(
        "ASIO linked · {} in · {} smp · {} → {} ch",
        input_channels,
        buffer_frames,
        rate_path(asio_rate, output.rate),
        output.channels
    ));

    let mut extra_outputs = ExtraOutputOutputs::new(config.output_buffer, output.rate);
    let wait = wait_for_callbacks(
        &registration,
        || stop.load(Ordering::Relaxed),
        || {
            extra_outputs.sync(&shared);
            // A stop already in flight must not open the panel on its way out.
            !stop.load(Ordering::Relaxed)
                && service_asio_panel(&driver, &shared, &[AsioPanelSide::Input])
        },
    );
    match wait {
        CallbackWait::Stopped => {}
        CallbackWait::Reset | CallbackWait::Failed => {
            let _ = driver.stop();
            let _ = driver.dispose_buffers();
            registration.clear();
            drop(extra_outputs);
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
            drop(extra_outputs);
            shared.set_device_lost(
                "The ASIO input stopped responding — the interface may have been unplugged"
                    .to_owned(),
            );
            return Ok(());
        }
    }

    let _ = driver.stop();
    let _ = driver.dispose_buffers();
    registration.clear();
    drop(extra_outputs);
    drop(output);
    Ok(())
}

/// Open at its real rate, with buffers and a callback, but not started.
/// The output designs its resampler for that rate before its own driver
/// starts.  Drop stops and disposes the driver and clears the callback.
/// It stays on the thread that created it, as ASIO requires.  `D` is
/// azo's driver proxy, which azo does not export by name.
pub(crate) struct PreparedAsioInput<D: Driver> {
    driver: D,
    registration: Option<CallbackRegistration>,
    rate: u32,
    input_channels: usize,
    buffer_frames: usize,
    /// The driver stopped calling back, so the context is leaked, not freed.
    hung: bool,
}

impl<D: Driver> Drop for PreparedAsioInput<D> {
    fn drop(&mut self) {
        let _ = self.driver.stop();
        let _ = self.driver.dispose_buffers();
        if let Some(registration) = self.registration.take() {
            if self.hung {
                registration.abandon();
            } else {
                registration.clear();
            }
        }
    }
}

/// Nothing calls back until [`PreparedAsioInput::run`].
pub(crate) fn prepare_asio_input(
    clsid: u128,
    requested_rate: u32,
    requested_buffer: usize,
    shared: &Arc<Shared>,
) -> Result<PreparedAsioInput<impl Driver>, String> {
    let host = azo::Host::new();
    let driver = host
        .create_driver(&GUID::from_u128(clsid))
        .map_err(|error| format!("Opening ASIO input driver: {error}"))?;
    if !driver.init(None) {
        return Err("The ASIO input driver refused to initialize".to_owned());
    }

    // Two ASIO drivers keep two clocks.  If they disagree on a rate the
    // output converts instead of refusing to bridge.
    let _ = driver.set_sample_rate(requested_rate as f64);
    let rate = driver
        .get_sample_rate()
        .ok()
        .map(|rate| rate.round() as i64)
        .filter(|rate| (8_000..=768_000).contains(rate))
        .map(|rate| rate as u32)
        .unwrap_or(requested_rate);
    shared.set_source_rate(rate);

    let counts = driver
        .channel_counts()
        .map_err(|error| format!("ASIO channels: {error}"))?;
    if counts.in_ < 1 {
        return Err("The ASIO driver has no input channels".to_owned());
    }
    let input_channels = (counts.in_ as usize).min(2);

    let buffer = driver
        .buffer_size()
        .map_err(|error| format!("ASIO buffer size: {error}"))?;
    let buffer_frames =
        if requested_buffer >= buffer.min as usize && requested_buffer <= buffer.max as usize {
            requested_buffer as i32
        } else {
            buffer.preferred
        };

    let mut channel_types = Vec::new();
    let mut channel_ids = Vec::new();
    for index in 0..input_channels {
        let id = ChannelId {
            input: true,
            index: index as i32,
        };
        let info = driver
            .channel_info(id)
            .map_err(|error| format!("ASIO channel info: {error}"))?;
        channel_types.push(info.sample_type);
        channel_ids.push(id);
    }

    let buffers: Vec<[*mut c_void; 2]> = match unsafe {
        driver.create_buffers(
            channel_ids.iter().copied(),
            buffer_frames,
            &raw const ASIO_INPUT_CALLBACKS,
        )
    } {
        Ok(buffers) => buffers.collect(),
        Err(error) => return Err(format!("ASIO buffers: {error}")),
    };

    let signal = AsioSignal::new();
    let context = AsioCallbackContext::new(
        Arc::clone(shared),
        Arc::clone(&signal),
        buffer_frames.max(1) as usize,
    )
    .with_input(buffers, channel_types);
    let registration = match CallbackRegistration::install(
        &ASIO_HOST,
        AsioCallbackSlot::Input,
        Box::new(context),
        signal,
    ) {
        Ok(registration) => registration,
        Err(error) => {
            let _ = driver.dispose_buffers();
            return Err(format!("ASIO callbacks: {error}"));
        }
    };

    Ok(PreparedAsioInput {
        driver,
        registration: Some(registration),
        rate,
        input_channels,
        buffer_frames: buffer_frames.max(1) as usize,
        hung: false,
    })
}

impl<D: Driver> PreparedAsioInput<D> {
    /// Call once the output has opened.
    pub(crate) fn run(
        mut self,
        output_rate: u32,
        shared: &Arc<Shared>,
        stop: &Arc<AtomicBool>,
        mut tick: impl FnMut(),
    ) -> Result<(), String> {
        let Some(registration) = self.registration.as_ref() else {
            return Ok(());
        };
        // The output side already set its target; these blocks feed the
        // bridge buffer's floor on top of it.
        shared.note_input_block(self.buffer_frames);

        if let Err(error) = self.driver.start() {
            return Err(format!("Starting ASIO: {error}"));
        }

        shared.running.store(true, Ordering::Relaxed);
        shared.set_status(format!(
            "ASIO input linked · {} in · {} smp · {} → ASIO output",
            self.input_channels,
            self.buffer_frames,
            rate_path(self.rate, output_rate)
        ));

        let wait = wait_for_callbacks(
            registration,
            || stop.load(Ordering::Relaxed),
            || {
                tick();
                !stop.load(Ordering::Relaxed)
                    && service_asio_panel(&self.driver, shared, &[AsioPanelSide::Input])
            },
        );
        match wait {
            CallbackWait::Stopped => Ok(()),
            CallbackWait::Reset => Err(DRIVER_RESET_MESSAGE.to_owned()),
            CallbackWait::Failed => Err(CALLBACK_PANIC_MESSAGE.to_owned()),
            CallbackWait::Hung => {
                self.hung = true;
                drop(self);
                shared.set_device_lost(
                    "The ASIO input stopped responding — the interface may have been unplugged"
                        .to_owned(),
                );
                Ok(())
            }
        }
    }
}
