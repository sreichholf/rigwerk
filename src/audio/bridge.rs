//! `select`'s order is the precedence: a later arm never sees a device an
//! earlier one claimed.  Every arm exists on every target.  An arm this
//! target does not build returns an error instead of falling through to
//! cpal.  An ASIO clsid is a Windows registry entry.
//!
//! The bridge buffer is set here, before dispatch.  Only the ASIO output
//! and ASIO duplex paths set it themselves.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use cpal::traits::StreamTrait;

use super::devices::{is_native_id, pick_input_config};
use super::extra_output::ExtraOutputOutputs;
use super::input::{make_input_stream, open_input_device};
use super::output::open_output;
use super::shared::{initial_target, Shared};
use super::types::StartConfig;
#[cfg(target_os = "windows")]
use super::win::asio::{run_asio_bridge, run_asio_output_bridge};

pub(crate) fn rate_path(input: u32, output: u32) -> String {
    if input == output {
        format!("{input} Hz")
    } else {
        format!("{input} → {output} Hz")
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Backend {
    AsioOutput(u128),
    AsioInput(u128),
    LinuxNative,
    Cpal,
}

fn select(config: &StartConfig) -> Backend {
    if let Some(output_clsid) = config.output_asio_clsid {
        // One driver can carry both directions, so this arm wins over input.
        return Backend::AsioOutput(output_clsid);
    }
    if let Some(clsid) = config.asio_clsid {
        // This driver owns the clock.  The output follows its real rate,
        // or converts from it.
        return Backend::AsioInput(clsid);
    }

    // A `pipewire:` id is a real node.  A mixed bridge still opens the
    // other side through cpal.
    if config.input_id.as_deref().is_some_and(is_native_id) || is_native_id(&config.output_id) {
        return Backend::LinuxNative;
    }
    Backend::Cpal
}

impl Backend {
    fn run(
        self,
        config: &StartConfig,
        shared: Arc<Shared>,
        stop: Arc<AtomicBool>,
    ) -> Result<(), String> {
        match self {
            #[cfg(target_os = "windows")]
            Self::AsioOutput(output_clsid) => {
                run_asio_output_bridge(config, output_clsid, shared, stop)
            }
            #[cfg(not(target_os = "windows"))]
            Self::AsioOutput(_) => Err(
                "ASIO output is not available on this platform. Pick a system output instead."
                    .to_owned(),
            ),
            #[cfg(target_os = "windows")]
            Self::AsioInput(clsid) => run_asio_bridge(config, clsid, shared, stop),
            #[cfg(not(target_os = "windows"))]
            Self::AsioInput(_) => Err(
                "ASIO input is not available on this platform. Pick a system input instead."
                    .to_owned(),
            ),
            #[cfg(target_os = "linux")]
            Self::LinuxNative => super::linux::native::run(config, shared, stop),
            #[cfg(not(target_os = "linux"))]
            Self::LinuxNative => Err(
                "Native PipeWire is not available on this platform. Pick a system device instead."
                    .to_owned(),
            ),
            Self::Cpal => run_cpal(config, shared, stop),
        }
    }
}

pub(crate) fn run_bridge(
    config: StartConfig,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
    shared.set_target_frames(initial_target(config.buffer_size));
    select(&config).run(&config, shared, stop)
}

fn run_cpal(
    config: &StartConfig,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
    // The input opens first.  A device that refuses the requested rate
    // opens at its own, and the output converts from that.
    shared.set_status("Opening input…");
    let input_device = open_input_device(config)?;
    let input_config = pick_input_config(&input_device, config.sample_rate)?;
    let input_rate = input_config.sample_rate();
    let input_channels = input_config.channels() as usize;
    shared.set_source_rate(input_rate);

    shared.set_status("Opening independent output…");
    let output = open_output(
        &config.output_id,
        config.output_rate.unwrap_or(input_rate),
        &shared,
        config.output_buffer,
        config.low_latency_request,
        config.exclusive_output,
    )?;
    // `open_output` already published the rate.  This thread must not load
    // or destroy plugins; the UI thread reloads them.
    shared.set_status("Waiting for input clock…");
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
        "Bridging {} ch · {} → independent output ({} ch)",
        input_channels,
        rate_path(input_rate, output.rate),
        output.channels
    ));

    // Own cpal streams, synced from the plan.  PipeWire duplex, ASIO and
    // native PipeWire never reach this path.
    let mut extra_outputs = ExtraOutputOutputs::new(config.output_buffer, output.rate);
    extra_outputs.sync(&shared);

    while !stop.load(Ordering::Relaxed) {
        extra_outputs.sync(&shared);
        thread::sleep(Duration::from_millis(20));
    }
    drop(extra_outputs);
    drop(input_stream);
    drop(output);
    Ok(())
}

#[cfg(test)]
mod tests;
