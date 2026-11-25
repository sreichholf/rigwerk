//! The bridge thread: picks the path for the chosen devices (ASIO or
//! system on either side), opens both sides and keeps them running.
//!
//! `select` decides which backend runs and nothing else: the order it asks
//! in is the precedence, so a backend never gets a device an earlier one
//! claimed.  Each arm of `Backend::run` only calls its backend.
//!
//! The clock target is still set in `run_bridge`, before dispatch, on
//! purpose: only the ASIO output and ASIO duplex paths set it themselves, so
//! the native PipeWire path, the ASIO input path and cpal rely on that line.
//!
//! Every arm exists on every target, so the table is whole everywhere; an arm
//! whose backend is not built on this target returns an error instead of
//! opening anything.  So a config that carries an ASIO clsid where there is no
//! ASIO errors instead of falling through to cpal: a clsid comes from a Windows
//! registry entry, so it cannot legitimately appear anywhere else.

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

/// "48000 Hz" when both sides share a rate, "44100 → 48000 Hz" when the
/// output converts.
pub(crate) fn rate_path(input: u32, output: u32) -> String {
    if input == output {
        format!("{input} Hz")
    } else {
        format!("{input} → {output} Hz")
    }
}

/// Which backend a start goes to.  The variant carries the selection's
/// result, so `run` dispatches without reading `config` again.  Every variant
/// exists on every target; an arm whose backend is not built there returns an
/// error.
#[derive(Debug, PartialEq, Eq)]
enum Backend {
    AsioOutput(u128),
    AsioInput(u128),
    LinuxNative,
    Cpal,
}

/// The backend `config` selects.  Its order is the precedence: ASIO output
/// first (one driver can carry both directions), then ASIO input, then a
/// native `pipewire:` id on either side, then cpal.  It reads nothing but
/// `config`, so it is the same table on every target.
fn select(config: &StartConfig) -> Backend {
    if let Some(output_clsid) = config.output_asio_clsid {
        return Backend::AsioOutput(output_clsid);
    }
    if let Some(clsid) = config.asio_clsid {
        // The ASIO driver owns the clock.  Ask it for its real rate first and
        // let the output follow or convert from that rate.
        return Backend::AsioInput(clsid);
    }

    // A `pipewire:{node.name}` id is a real PipeWire source or sink. The
    // native client links RigWerk's own nodes to it. Mixed bridges still open
    // the other side through cpal.
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

/// cpal capture and playback as two independent streams.
fn run_cpal(
    config: &StartConfig,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
    // Settle the input first.  A device that cannot run at the requested rate
    // opens at its own; the output then converts from that real rate instead
    // of playing the input too fast or too slow.
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
        config.real_style_output,
        config.exclusive_output,
    )?;
    // `open_output` already published `output.rate`.  The UI thread reloads
    // plugins from that rate; this thread must not load or destroy them.
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

    // Generic extra outputs: own cpal streams, synced live from the plan.
    // PipeWire duplex never reaches this path; ASIO / native PW never do.
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
