//! REAL-style playback for WASAPI outputs.
//!
//! Windows mixes every shared-mode stream in fixed steps, 10 ms by default.
//! Since Windows 10 a driver may allow smaller steps; a stream opened through
//! IAudioClient3 at the smallest one pulls the whole device down to it while
//! the stream runs.  That is all the REAL tool does.  Here the same stream
//! also carries the bridge's audio, so the output side shrinks from about
//! 10-20 ms to a few milliseconds and other apps on that device benefit too.

use super::mixer::OutputMixer;
use super::output::write_frame;
use super::shared::Shared;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows::core::{w, GUID, HSTRING, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    IAudioClient3, IAudioRenderClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    WAVEFORMATEX,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW,
    WaitForSingleObject,
};

const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
const SUBTYPE_IEEE_FLOAT: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

/// What the driver actually granted.
#[derive(Clone, Debug)]
pub struct RealInfo {
    pub rate: u32,
    pub channels: usize,
    /// The step the device now runs at, in frames.
    pub step_frames: u32,
    pub min_frames: u32,
    pub default_frames: u32,
}

impl RealInfo {
    pub fn describe(&self, requested_rate: u32) -> String {
        let ms = |frames: u32| frames as f32 / self.rate.max(1) as f32 * 1000.0;
        let mut text = if self.step_frames > self.min_frames {
            format!(
                "REAL-style · {} smp step ({:.1} ms) · another app holds the device at this step (driver minimum {} smp)",
                self.step_frames,
                ms(self.step_frames),
                self.min_frames
            )
        } else if self.min_frames >= self.default_frames {
            // The driver's smallest shared-mode step is its 10 ms default.
            // REAL itself cannot go lower here either; say so plainly instead
            // of pretending the attempt failed.
            format!(
                "REAL-style · {} smp step ({:.1} ms) · no smaller step: the driver's minimum is its default",
                self.step_frames,
                ms(self.step_frames)
            )
        } else {
            format!(
                "REAL-style · {} smp step ({:.1} ms), down from {} smp ({:.1} ms)",
                self.step_frames,
                ms(self.step_frames),
                self.default_frames,
                ms(self.default_frames)
            )
        };
        if requested_rate != self.rate {
            text.push_str(&format!(" · device mixes at {} Hz", self.rate));
        }
        text
    }
}

/// The running REAL-style stream.  Dropping it stops playback and hands
/// the device back to its normal step.
pub struct RealOutput {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Drop for RealOutput {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Opens `endpoint_id` (a WASAPI endpoint id) at its smallest engine step.
/// Returns why not when the driver refuses, so the caller can fall back.
pub fn open(
    endpoint_id: &str,
    requested_rate: u32,
    shared: Arc<Shared>,
) -> Result<(RealOutput, RealInfo), String> {
    let (ready_tx, ready_rx) = mpsc::channel::<Result<RealInfo, String>>();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let endpoint = endpoint_id.to_owned();
    let _ = requested_rate;
    let handle = thread::Builder::new()
        .name("rigwerk-real-output".to_owned())
        .spawn(move || unsafe { run(&endpoint, shared, thread_stop, ready_tx) })
        .map_err(|error| format!("could not start its thread ({error})"))?;

    match ready_rx.recv_timeout(Duration::from_secs(4)) {
        Ok(Ok(info)) => Ok((
            RealOutput {
                stop,
                handle: Some(handle),
            },
            info,
        )),
        Ok(Err(reason)) => {
            let _ = handle.join();
            Err(reason)
        }
        Err(_) => {
            stop.store(true, Ordering::Relaxed);
            Err("the driver did not answer in time".to_owned())
        }
    }
}

/// Asks Windows which engine steps this output's driver allows, without
/// opening, initializing or starting a stream.  This is the "check again"
/// path: after a driver is installed or removed it reports the new minimum
/// straight away, so the app does not have to keep a stale result.
pub fn probe(endpoint_id: &str) -> Result<RealInfo, String> {
    unsafe {
        let com = CoInitializeEx(None, COINIT_MULTITHREADED).is_ok();
        let result = probe_session(endpoint_id);
        if com {
            CoUninitialize();
        }
        result
    }
}

unsafe fn probe_session(endpoint: &str) -> Result<RealInfo, String> {
    let enumerator: IMMDeviceEnumerator =
        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .map_err(|error| format!("no device enumerator ({error})"))?;
    let device = enumerator
        .GetDevice(&HSTRING::from(endpoint))
        .map_err(|error| format!("the output was not found ({error})"))?;
    let client: IAudioClient3 = device
        .Activate(CLSCTX_ALL, None)
        .map_err(|error| driver_message("this Windows or driver has no IAudioClient3", &error))?;
    let format = MixFormat(
        client
            .GetMixFormat()
            .map_err(|error| format!("no mix format ({error})"))?,
    );
    let (channels, rate) = float_layout(format.0)
        .ok_or_else(|| "the device mix format is not 32-bit float".to_owned())?;

    let (mut default, mut fundamental, mut min, mut max) = (0u32, 0u32, 0u32, 0u32);
    client
        .GetSharedModeEnginePeriod(format.0, &mut default, &mut fundamental, &mut min, &mut max)
        .map_err(|error| driver_message("the driver reports no engine steps", &error))?;
    if min == 0 || default == 0 {
        return Err("the driver reports no usable engine step".to_owned());
    }
    Ok(RealInfo {
        rate,
        channels,
        step_frames: min,
        min_frames: min,
        default_frames: default,
    })
}

/// Windows' "access denied" answer, which is what an endpoint whose driver
/// was removed (or never bound) gives before anything can be opened.
fn access_denied(code: i32) -> bool {
    code as u32 == 0x8007_0005
}

fn driver_message(context: &str, error: &windows::core::Error) -> String {
    if access_denied(error.code().0) {
        "Windows has no working driver for this output right now. If the old vendor driver was just removed, restart Windows (or scan for hardware changes) so the in-box \"High Definition Audio Device\" driver can take it over, then check again.".to_owned()
    } else {
        format!("{context} ({error})")
    }
}

/// The mix format block Windows allocated for us.
struct MixFormat(*mut WAVEFORMATEX);

impl Drop for MixFormat {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.0 as *const _)) };
    }
}

struct EventHandle(HANDLE);

impl Drop for EventHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

struct Session {
    client: IAudioClient3,
    render: IAudioRenderClient,
    event: EventHandle,
    _format: MixFormat,
    buffer_frames: u32,
    info: RealInfo,
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

/// Channel count and rate of a 32-bit float mix format, the format
/// Windows' shared-mode engine uses on every current system.
unsafe fn float_layout(format: *const WAVEFORMATEX) -> Option<(usize, u32)> {
    let header = std::ptr::read_unaligned(format);
    let tag = header.wFormatTag;
    let bits = header.wBitsPerSample;
    let channels = header.nChannels;
    let rate = header.nSamplesPerSec;
    let extra = header.cbSize;
    let float = match tag {
        WAVE_FORMAT_IEEE_FLOAT => true,
        WAVE_FORMAT_EXTENSIBLE if extra >= 22 => {
            // SubFormat follows the 18-byte header, wValidBitsPerSample
            // and dwChannelMask.
            let sub = std::ptr::read_unaligned(format.cast::<u8>().add(24).cast::<GUID>());
            sub == SUBTYPE_IEEE_FLOAT
        }
        _ => false,
    };
    (float && bits == 32 && channels > 0 && rate > 0).then_some((channels as usize, rate))
}

unsafe fn open_session(endpoint: &str) -> Result<Session, String> {
    let enumerator: IMMDeviceEnumerator =
        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .map_err(|error| format!("no device enumerator ({error})"))?;
    let device = enumerator
        .GetDevice(&HSTRING::from(endpoint))
        .map_err(|error| format!("the output was not found ({error})"))?;
    let client: IAudioClient3 = device
        .Activate(CLSCTX_ALL, None)
        .map_err(|error| driver_message("this Windows or driver has no IAudioClient3", &error))?;
    let format = MixFormat(
        client
            .GetMixFormat()
            .map_err(|error| format!("no mix format ({error})"))?,
    );
    let (channels, rate) = float_layout(format.0)
        .ok_or_else(|| "the device mix format is not 32-bit float".to_owned())?;

    let (mut default, mut fundamental, mut min, mut max) = (0u32, 0u32, 0u32, 0u32);
    client
        .GetSharedModeEnginePeriod(format.0, &mut default, &mut fundamental, &mut min, &mut max)
        .map_err(|error| driver_message("the driver reports no engine steps", &error))?;
    if min == 0 || default == 0 {
        return Err("the driver reports no usable engine step".to_owned());
    }

    // The smallest step the driver allows, exactly what REAL asks for.  Many
    // drivers only offer their 10 ms default; that is still worth using here,
    // because this stream queues one step of audio instead of letting the host
    // fill a much larger buffer, so the output side stays at the driver's
    // real minimum rather than growing to several periods.
    if let Err(error) =
        client.InitializeSharedAudioStream(AUDCLNT_STREAMFLAGS_EVENTCALLBACK, min, format.0, None)
    {
        // The engine may already be running at a larger step because another
        // app opened this device first (AUDCLNT_E_ENGINE_PERIODICITY_LOCKED).
        // Joining that step still beats falling back to a host buffer that is
        // several periods long, so ask what step is in effect and use it.
        let mut current_format: *mut WAVEFORMATEX = std::ptr::null_mut();
        let mut current = 0u32;
        let _ = client.GetCurrentSharedModeEnginePeriod(&mut current_format, &mut current);
        if !current_format.is_null() {
            CoTaskMemFree(Some(current_format as *const _));
        }
        if current == 0 || current == min {
            return Err(format!("the driver refused a {min} smp step ({error})"));
        }
        client
            .InitializeSharedAudioStream(AUDCLNT_STREAMFLAGS_EVENTCALLBACK, current, format.0, None)
            .map_err(|retry| {
                format!(
                    "the driver refused a {min} smp step ({error}) and the {current} smp step another app holds ({retry})"
                )
            })?;
    }

    // Report the step really in effect, which may be larger if another app
    // already held this device when the stream opened.
    let mut current_format: *mut WAVEFORMATEX = std::ptr::null_mut();
    let mut current = min;
    if client
        .GetCurrentSharedModeEnginePeriod(&mut current_format, &mut current)
        .is_ok()
        && !current_format.is_null()
    {
        CoTaskMemFree(Some(current_format as *const _));
    }
    let step = if current > 0 { current } else { min };

    let event = EventHandle(
        CreateEventW(None, false, false, PCWSTR::null())
            .map_err(|error| format!("no wake-up event ({error})"))?,
    );
    client
        .SetEventHandle(event.0)
        .map_err(|error| format!("event setup failed ({error})"))?;
    let buffer_frames = client
        .GetBufferSize()
        .map_err(|error| format!("no buffer size ({error})"))?;
    let render: IAudioRenderClient = client
        .GetService()
        .map_err(|error| format!("no render client ({error})"))?;

    // Start from one step of silence so the first wake-up is not a glitch.
    let lead = step.min(buffer_frames);
    if render.GetBuffer(lead).is_ok() {
        let _ = render.ReleaseBuffer(lead, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32);
    }
    client
        .Start()
        .map_err(|error| format!("the stream would not start ({error})"))?;

    Ok(Session {
        client,
        render,
        event,
        _format: format,
        buffer_frames,
        info: RealInfo {
            rate,
            channels,
            step_frames: step,
            min_frames: min,
            default_frames: default,
        },
    })
}

fn report_failure(shared: &Shared, error: &windows::core::Error) {
    if error.code() == AUDCLNT_E_DEVICE_INVALIDATED {
        shared.set_device_lost("Output device disconnected".to_owned());
    } else {
        shared.set_error(format!("REAL-style output stopped: {error}"));
    }
}

#[cfg(test)]
mod tests {
    use super::RealInfo;

    fn info(step: u32, min: u32, default: u32) -> RealInfo {
        RealInfo {
            rate: 48_000,
            channels: 2,
            step_frames: step,
            min_frames: min,
            default_frames: default,
        }
    }

    #[test]
    fn a_driver_with_only_its_default_says_so() {
        let text = info(480, 480, 480).describe(48_000);
        assert!(text.contains("no smaller step"), "{text}");
        assert!(!text.contains("down from"), "{text}");
    }

    #[test]
    fn a_genuinely_smaller_step_is_reported() {
        let text = info(128, 128, 480).describe(48_000);
        assert!(text.contains("down from 480 smp"), "{text}");
        assert!(!text.contains("no smaller step"), "{text}");
    }

    #[test]
    fn a_step_held_by_another_app_is_reported() {
        let text = info(480, 128, 480).describe(48_000);
        assert!(text.contains("another app holds"), "{text}");
    }

    #[test]
    fn access_denied_is_recognised() {
        assert!(super::access_denied(0x8007_0005u32 as i32));
        assert!(!super::access_denied(0x8889_000Au32 as i32));
    }
}

unsafe fn run(
    endpoint: &str,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    ready: mpsc::Sender<Result<RealInfo, String>>,
) {
    let com = CoInitializeEx(None, COINIT_MULTITHREADED).is_ok();
    let session = match open_session(endpoint) {
        Ok(session) => session,
        Err(reason) => {
            let _ = ready.send(Err(reason));
            if com {
                CoUninitialize();
            }
            return;
        }
    };
    if ready.send(Ok(session.info.clone())).is_err() {
        drop(session);
        if com {
            CoUninitialize();
        }
        return;
    }

    // The same scheduling class pro audio apps use, so a busy UI cannot
    // make the small step late.
    let mut task_index = 0u32;
    let mmcss = AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index).ok();

    let channels = session.info.channels;
    let mut mixer = OutputMixer::new(session.info.rate);
    // Keep exactly one step queued.  The host hands us a much larger buffer
    // (about two steps), but filling it would add its whole size to the
    // latency and throw away the low-latency stream's advantage.  One step is
    // what the driver asks for each wake-up, so the step - not the buffer -
    // sets the output delay.  MMCSS "Pro Audio" keeps the wake-up on time.
    let queue = session
        .info
        .step_frames
        .clamp(1, session.buffer_frames.max(1));
    let mut last_event = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        if WaitForSingleObject(session.event.0, 200) != WAIT_OBJECT_0 {
            if last_event.elapsed() > Duration::from_secs(2) {
                shared.set_device_lost(
                    "The output stopped asking for audio — the device may have been unplugged"
                        .to_owned(),
                );
                break;
            }
            continue;
        }
        last_event = Instant::now();
        let padding = match session.client.GetCurrentPadding() {
            Ok(padding) => padding,
            Err(error) => {
                report_failure(&shared, &error);
                break;
            }
        };
        let frames = queue.saturating_sub(padding);
        if frames == 0 {
            continue;
        }
        let data = match session.render.GetBuffer(frames) {
            Ok(data) => data,
            Err(error) => {
                report_failure(&shared, &error);
                break;
            }
        };
        let samples =
            std::slice::from_raw_parts_mut(data.cast::<f32>(), frames as usize * channels);
        mixer.render(&shared, frames as usize, |index, out| {
            write_frame(&mut samples[index * channels..(index + 1) * channels], out);
        });
        if let Err(error) = session.render.ReleaseBuffer(frames, 0) {
            report_failure(&shared, &error);
            break;
        }
    }

    if let Some(handle) = mmcss {
        let _ = AvRevertMmThreadCharacteristics(handle);
    }
    drop(session);
    if com {
        CoUninitialize();
    }
}
