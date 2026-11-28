//! Exclusive and REAL-style run before the shared cpal stream.  Extra
//! outputs never use this policy; they do use the device-in-use sentence.

use std::sync::Arc;

use crate::audio::output::{OpenedOutput, OutputStream, FIXED_BUFFER_REFUSED};
use crate::audio::shared::Shared;
use crate::audio::types::OutputMode;
use crate::audio::win::wasapi;

pub(crate) enum Attempt {
    Opened(OpenedOutput),
    /// What the shared stream's mode line adds, or `None` when no WASAPI
    /// attempt was made.
    Refused(Option<String>),
}

/// `exclusive` is WASAPI exclusive; `low_latency` is IAudioClient3 at the
/// driver's smallest shared step (REAL-style).  Neither is retried.  A
/// refusal carries the reason so the shared stream can say what it fell
/// back from.
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

/// `refused` is what the WASAPI attempt left behind.
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

/// `AUDCLNT_E_DEVICE_IN_USE` (0x8889000A) as cpal prints it.  With ASIO4ALL
/// holding the input, the Realtek card it drives refused every shared
/// output with this code.
const DEVICE_IN_USE: &str = "-2004287478";

/// The sentence for that code, or `None` for any other error.
pub(crate) fn explain_device_in_use(error: &str) -> Option<&'static str> {
    error.contains(DEVICE_IN_USE).then_some(
        "This output is in use: another driver or app has taken the sound card for itself",
    )
}

#[cfg(test)]
mod tests;
