//! The Windows open policy for the clock output: WASAPI exclusive and
//! REAL-style first, the shared cpal stream behind them.
//!
//! `audio/output` owns the cpal stream, the config and `OpenedOutput`; this
//! file owns what Windows tries before that stream, what it says when the
//! attempt is refused, and the one Windows error that has a meaning of its
//! own.  Extra outputs never use this open policy; they do use the
//! device-in-use sentence.

use std::sync::Arc;

use crate::audio::output::{OpenedOutput, OutputStream, FIXED_BUFFER_REFUSED};
use crate::audio::shared::Shared;
use crate::audio::types::OutputMode;
use crate::audio::win::wasapi;

/// What the WASAPI attempt before the shared stream decided.
pub(crate) enum Attempt {
    /// WASAPI opened the clock output.
    Opened(OpenedOutput),
    /// Shared cpal next; what its mode line adds, when an attempt was made.
    Refused(Option<String>),
}

/// Tries the WASAPI clock output `exclusive` or `low_latency` asked for, and
/// returns it when it opened.
///
/// `exclusive` asks for WASAPI exclusive mode and `low_latency` for
/// IAudioClient3 at the driver's smallest shared engine step (Windows'
/// REAL-style attempt), in that order; neither is retried.  A refusal is a
/// `Refused` carrying the reason, so the shared stream that follows can say
/// what it fell back from.
pub(crate) fn attempt(
    id: &cpal::DeviceId,
    sample_rate: u32,
    shared: &Arc<Shared>,
    low_latency: bool,
    exclusive: bool,
) -> Attempt {
    if exclusive {
        if id.host() != cpal::HostId::Wasapi {
            return Attempt::Refused(Some(format!(
                " · Exclusive refused: {} has no exclusive mode",
                id.host()
            )));
        }
        match wasapi::open_exclusive(id.id(), sample_rate, Arc::clone(shared)) {
            Ok((stream, info)) => {
                shared.set_output(
                    info.rate,
                    info.step_frames,
                    OutputMode::Exclusive,
                    info.describe(sample_rate),
                );
                Attempt::Opened(OpenedOutput {
                    _stream: OutputStream::Exclusive(stream),
                    rate: info.rate,
                    channels: info.channels,
                })
            }
            Err(reason) => Attempt::Refused(Some(format!(" · Exclusive refused: {reason}"))),
        }
    } else if low_latency {
        if id.host() != cpal::HostId::Wasapi {
            return Attempt::Refused(Some(format!(
                " · Shared, smallest buffer refused: {} has no adjustable engine step",
                id.host()
            )));
        }
        match wasapi::open(id.id(), sample_rate, Arc::clone(shared)) {
            Ok((stream, info)) => {
                shared.set_output(
                    info.rate,
                    info.step_frames,
                    OutputMode::RealStyle,
                    info.describe(sample_rate),
                );
                Attempt::Opened(OpenedOutput {
                    _stream: OutputStream::Real(stream),
                    rate: info.rate,
                    channels: info.channels,
                })
            }
            Err(reason) => Attempt::Refused(Some(format!(
                " · Shared, smallest buffer refused: {reason}"
            ))),
        }
    } else {
        Attempt::Refused(None)
    }
}

/// The mode and the line the bridge publishes once the shared cpal stream is
/// the one that runs; `refused` is what the WASAPI attempt left behind.
pub(crate) fn describe(refused: Option<&str>, fixed_buffer: bool) -> (OutputMode, String) {
    let note = if fixed_buffer {
        ""
    } else {
        FIXED_BUFFER_REFUSED
    };
    (
        OutputMode::Shared,
        format!("Standard output{note}{}", refused.unwrap_or("")),
    )
}

/// Windows' "device in use" answer (AUDCLNT_E_DEVICE_IN_USE, 0x8889000A) as
/// cpal prints it.  Measured: with ASIO4ALL as the input, the Realtek card it
/// drives refused every shared output stream with exactly this code.
const DEVICE_IN_USE: &str = "-2004287478";

/// The sentence to show instead of that code, or `None` for any other error.
pub(crate) fn explain_device_in_use(error: &str) -> Option<&'static str> {
    error.contains(DEVICE_IN_USE).then_some(
        "This output is in use: another driver or app has taken the sound card for itself",
    )
}

#[cfg(test)]
mod tests;
