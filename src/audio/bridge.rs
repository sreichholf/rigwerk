//! The bridge thread: picks the path for the chosen devices (ASIO or
//! system on either side), opens both sides and keeps them running.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use cpal::traits::StreamTrait;

use super::devices::pick_input_config;
use super::follower::FollowerOutputs;
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

pub(crate) fn run_bridge(
    config: StartConfig,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
    shared.set_target_frames(initial_target(config.buffer_size));
    #[cfg(target_os = "windows")]
    if let Some(output_clsid) = config.output_asio_clsid {
        return run_asio_output_bridge(
            &config,
            output_clsid,
            Arc::clone(&shared),
            Arc::clone(&stop),
        );
    }
    #[cfg(target_os = "windows")]
    if let Some(clsid) = config.asio_clsid {
        // The ASIO driver owns the clock.  Ask it for its real rate first and
        // let the output follow or convert from that rate.
        return run_asio_bridge(&config, clsid, Arc::clone(&shared), Arc::clone(&stop));
    }

    // A `pipewire:{node.name}` id is a real PipeWire source or sink. The
    // native client links RigWerk's own nodes to it. Mixed bridges still open
    // the other side through cpal.
    #[cfg(target_os = "linux")]
    if config
        .input_id
        .as_deref()
        .is_some_and(super::linux::native::is_native_id)
        || super::linux::native::is_native_id(&config.output_id)
    {
        return super::linux::native::run(&config, shared, stop);
    }

    // Settle the input first.  A device that cannot run at the requested rate
    // opens at its own; the output then converts from that real rate instead
    // of playing the input too fast or too slow.
    shared.set_status("Opening input…");
    let input_device = open_input_device(&config)?;
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

    // Generic followers: own cpal streams, synced live from the plan.
    // PipeWire duplex never reaches this path; ASIO / native PW return above.
    let mut followers = FollowerOutputs::new(config.output_buffer, output.rate);
    followers.sync(&shared);

    while !stop.load(Ordering::Relaxed) {
        followers.sync(&shared);
        thread::sleep(Duration::from_millis(20));
    }
    drop(followers);
    drop(input_stream);
    drop(output);
    Ok(())
}
