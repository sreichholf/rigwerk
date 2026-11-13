//! Exclusive-mode WASAPI playback for the clock device.
//!
//! Takes the endpoint for RigWerk alone so the driver can run below the
//! shared-mode engine period.  Other apps go silent on that device while the
//! stream holds it.  Followers stay on shared streams.

use crate::audio::mixer::OutputMixer;
use crate::audio::output::write_frame;
use crate::audio::record::{push_device_frame, RecordSource};
use crate::audio::shared::Shared;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows::core::{w, GUID, HSTRING, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    IAudioClient, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED, AUDCLNT_E_DEVICE_INVALIDATED,
    AUDCLNT_SHAREMODE_EXCLUSIVE, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, WAVEFORMATEX,
    WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0, WAVE_FORMAT_PCM,
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
const SUBTYPE_IEEE_FLOAT: GUID = GUID::from_u128(0x0000_0003_0000_0010_8000_00aa_0038_9b71);
const SUBTYPE_PCM: GUID = GUID::from_u128(0x0000_0001_0000_0010_8000_00aa_0038_9b71);
const SPEAKER_FRONT_LEFT: u32 = 0x1;
const SPEAKER_FRONT_RIGHT: u32 = 0x2;
const SPEAKER_FRONT_CENTER: u32 = 0x4;
const HNS_PER_SEC: i64 = 10_000_000;

/// What exclusive mode actually opened.
#[derive(Clone, Debug)]
pub struct ExclusiveInfo {
    pub rate: u32,
    pub channels: usize,
    /// Frames written on each buffer-ready event (`GetBufferSize`).
    pub step_frames: u32,
    /// Driver minimum exclusive period, for diagnostics.
    pub min_period_frames: u32,
    /// Driver default exclusive period, for diagnostics.
    pub default_period_frames: u32,
}

impl ExclusiveInfo {
    pub fn describe(&self, requested_rate: u32) -> String {
        let ms = |frames: u32| frames as f32 / self.rate.max(1) as f32 * 1000.0;
        let mut text = format!(
            "Exclusive · {} smp step ({:.1} ms) · this device only",
            self.step_frames,
            ms(self.step_frames)
        );
        if self.step_frames > self.min_period_frames && self.min_period_frames > 0 {
            text.push_str(&format!(
                " · aligned from {} smp min",
                self.min_period_frames
            ));
        } else if self.default_period_frames > self.step_frames {
            text.push_str(&format!(
                ", down from {} smp ({:.1} ms) default",
                self.default_period_frames,
                ms(self.default_period_frames)
            ));
        }
        if requested_rate != self.rate {
            text.push_str(&format!(" · device at {} Hz", self.rate));
        }
        text
    }
}

/// Running exclusive stream.  Dropping it stops playback and frees the device.
pub struct ExclusiveOutput {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Drop for ExclusiveOutput {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Opens `endpoint_id` in WASAPI exclusive mode at the driver's minimum period.
pub fn open_exclusive(
    endpoint_id: &str,
    requested_rate: u32,
    shared: Arc<Shared>,
) -> Result<(ExclusiveOutput, ExclusiveInfo), String> {
    let (ready_tx, ready_rx) = mpsc::channel::<Result<ExclusiveInfo, String>>();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let endpoint = endpoint_id.to_owned();
    let _ = requested_rate;
    let handle = thread::Builder::new()
        .name("rigwerk-exclusive-output".to_owned())
        .spawn(move || unsafe { run(&endpoint, shared, thread_stop, ready_tx) })
        .map_err(|error| format!("could not start its thread ({error})"))?;

    match ready_rx.recv_timeout(Duration::from_secs(4)) {
        Ok(Ok(info)) => Ok((
            ExclusiveOutput {
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
            let _ = handle.join();
            Err("the driver did not answer in time".to_owned())
        }
    }
}

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

fn exclusive_refuse(error: &windows::core::Error) -> String {
    let code = error.code().0 as u32;
    match code {
        0x8889_000A => "another app or driver already holds this device".to_owned(),
        0x8889_000E => "this endpoint does not allow exclusive mode".to_owned(),
        0x8889_0008 => "no exclusive format the driver will accept".to_owned(),
        _ if access_denied(error.code().0) => driver_message("exclusive open refused", error),
        _ => format!("the driver refused exclusive mode ({error})"),
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

/// Device sample layout for the exclusive buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SampleKind {
    F32,
    I16,
    /// 24-bit audio in a 32-bit container (valid bits in the high 24).
    I24In32,
    I32,
}

/// Owned WAVEFORMATEXTENSIBLE kept alive for the session.
struct OwnedFormat {
    wave: WAVEFORMATEXTENSIBLE,
    kind: SampleKind,
    channels: usize,
    rate: u32,
}

impl OwnedFormat {
    fn as_wave(&self) -> *const WAVEFORMATEX {
        std::ptr::from_ref(&self.wave.Format)
    }
}

struct Session {
    client: IAudioClient,
    render: IAudioRenderClient,
    event: EventHandle,
    format: OwnedFormat,
    buffer_frames: u32,
    info: ExclusiveInfo,
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

fn channel_mask(channels: u16) -> u32 {
    match channels {
        1 => SPEAKER_FRONT_CENTER,
        2 => SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT,
        n => {
            // Contiguous front speakers up to the channel count Windows uses.
            (1u32 << n.min(18)).saturating_sub(1)
        }
    }
}

fn make_extensible(
    channels: u16,
    rate: u32,
    bits: u16,
    valid_bits: u16,
    subtype: GUID,
) -> WAVEFORMATEXTENSIBLE {
    let bytes = bits / 8;
    WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_EXTENSIBLE,
            nChannels: channels,
            nSamplesPerSec: rate,
            nAvgBytesPerSec: rate * u32::from(channels) * u32::from(bytes),
            nBlockAlign: channels * bytes,
            wBitsPerSample: bits,
            cbSize: 22,
        },
        Samples: WAVEFORMATEXTENSIBLE_0 {
            wValidBitsPerSample: valid_bits,
        },
        dwChannelMask: channel_mask(channels),
        SubFormat: subtype,
    }
}

fn make_pcm(channels: u16, rate: u32, bits: u16) -> WAVEFORMATEX {
    let bytes = bits / 8;
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM as u16,
        nChannels: channels,
        nSamplesPerSec: rate,
        nAvgBytesPerSec: rate * u32::from(channels) * u32::from(bytes),
        nBlockAlign: channels * bytes,
        wBitsPerSample: bits,
        cbSize: 0,
    }
}

fn make_float(channels: u16, rate: u32) -> WAVEFORMATEX {
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
        nChannels: channels,
        nSamplesPerSec: rate,
        nAvgBytesPerSec: rate * u32::from(channels) * 4,
        nBlockAlign: channels * 4,
        wBitsPerSample: 32,
        cbSize: 0,
    }
}

unsafe fn format_supported(client: &IAudioClient, format: *const WAVEFORMATEX) -> bool {
    // Exclusive: ppClosestMatch must be null; unsupported formats return an
    // error HRESULT (not S_FALSE with a closest match).
    client
        .IsFormatSupported(AUDCLNT_SHAREMODE_EXCLUSIVE, format, None)
        .is_ok()
}

unsafe fn pick_format(
    client: &IAudioClient,
    mix: *const WAVEFORMATEX,
) -> Result<OwnedFormat, String> {
    let header = std::ptr::read_unaligned(mix);
    let channels = header.nChannels.max(1);
    let rate = header.nSamplesPerSec.max(1);

    // Prefer float32 (mix-format variants), then common PCM exclusive layouts.
    let candidates: [(SampleKind, WAVEFORMATEXTENSIBLE); 4] = [
        (
            SampleKind::F32,
            make_extensible(channels, rate, 32, 32, SUBTYPE_IEEE_FLOAT),
        ),
        (
            SampleKind::I16,
            make_extensible(channels, rate, 16, 16, SUBTYPE_PCM),
        ),
        (
            SampleKind::I24In32,
            make_extensible(channels, rate, 32, 24, SUBTYPE_PCM),
        ),
        (
            SampleKind::I32,
            make_extensible(channels, rate, 32, 32, SUBTYPE_PCM),
        ),
    ];

    for (kind, wave) in candidates {
        if format_supported(client, std::ptr::from_ref(&wave.Format)) {
            return Ok(OwnedFormat {
                wave,
                kind,
                channels: channels as usize,
                rate,
            });
        }
    }

    // Plain (non-extensible) float / PCM tags some older drivers still want.
    let plain_float = make_float(channels, rate);
    if format_supported(client, std::ptr::from_ref(&plain_float)) {
        return Ok(OwnedFormat {
            wave: WAVEFORMATEXTENSIBLE {
                Format: plain_float,
                Samples: WAVEFORMATEXTENSIBLE_0 {
                    wValidBitsPerSample: 32,
                },
                dwChannelMask: channel_mask(channels),
                SubFormat: SUBTYPE_IEEE_FLOAT,
            },
            kind: SampleKind::F32,
            channels: channels as usize,
            rate,
        });
    }
    let plain_pcm = make_pcm(channels, rate, 16);
    if format_supported(client, std::ptr::from_ref(&plain_pcm)) {
        return Ok(OwnedFormat {
            wave: WAVEFORMATEXTENSIBLE {
                Format: plain_pcm,
                Samples: WAVEFORMATEXTENSIBLE_0 {
                    wValidBitsPerSample: 16,
                },
                dwChannelMask: channel_mask(channels),
                SubFormat: SUBTYPE_PCM,
            },
            kind: SampleKind::I16,
            channels: channels as usize,
            rate,
        });
    }

    Err("no exclusive format the driver will accept".to_owned())
}

fn frames_to_hns(frames: u32, rate: u32) -> i64 {
    let rate = rate.max(1) as i64;
    ((frames as i64) * HNS_PER_SEC + rate / 2) / rate
}

fn hns_to_frames(hns: i64, rate: u32) -> u32 {
    let rate = rate.max(1) as i64;
    ((hns * rate + HNS_PER_SEC / 2) / HNS_PER_SEC).max(1) as u32
}

unsafe fn activate_client(device: &IMMDevice) -> Result<IAudioClient, String> {
    device
        .Activate(CLSCTX_ALL, None)
        .map_err(|error| driver_message("could not open an audio client", &error))
}

/// Re-open when GetBufferSize is many times the period we asked for.
///
/// Event-driven exclusive must write a full `GetBufferSize()` buffer on every
/// wake (MSDN / OpenAL Soft).  Some USB (UAC1) drivers allocate an endpoint
/// buffer much larger than the requested period while still signaling at the
/// short period — PortAudio documents this as ~8×.  Writing that many frames
/// every short wake drains the ring many times realtime and sounds like a
/// stutter / helicopter.  Matching the period to the buffer we actually got
/// keeps the write size and the wake rate at 1:1.
unsafe fn match_period_to_buffer(
    device: &IMMDevice,
    format: &OwnedFormat,
    client: IAudioClient,
    period_hns: i64,
    buffer_frames: u32,
) -> Result<(IAudioClient, u32, i64), String> {
    let requested = hns_to_frames(period_hns, format.rate).max(1);
    if buffer_frames < requested.saturating_mul(2) {
        return Ok((client, buffer_frames, period_hns));
    }
    drop(client);
    let matched_hns = frames_to_hns(buffer_frames, format.rate).max(1);
    let client = activate_client(device)?;
    client
        .Initialize(
            AUDCLNT_SHAREMODE_EXCLUSIVE,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            matched_hns,
            matched_hns,
            format.as_wave(),
            None,
        )
        .map_err(|retry| exclusive_refuse(&retry))?;
    let frames = client
        .GetBufferSize()
        .map_err(|retry| format!("no exclusive buffer size after period match ({retry})"))?;
    Ok((client, frames, matched_hns))
}

unsafe fn initialize_exclusive(
    device: &IMMDevice,
    format: &OwnedFormat,
    period_hns: i64,
) -> Result<(IAudioClient, u32, i64), String> {
    let client = activate_client(device)?;
    match client.Initialize(
        AUDCLNT_SHAREMODE_EXCLUSIVE,
        AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
        period_hns,
        period_hns,
        format.as_wave(),
        None,
    ) {
        Ok(()) => {
            let frames = client
                .GetBufferSize()
                .map_err(|error| format!("no exclusive buffer size ({error})"))?;
            match_period_to_buffer(device, format, client, period_hns, frames)
        }
        Err(error) if error.code() == AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED => {
            // Ask for the aligned frame count, then re-activate with that period.
            let aligned = client
                .GetBufferSize()
                .map_err(|retry| format!("exclusive buffer not aligned ({error}; {retry})"))?;
            drop(client);
            let aligned_hns = frames_to_hns(aligned, format.rate);
            let client = activate_client(device)?;
            client
                .Initialize(
                    AUDCLNT_SHAREMODE_EXCLUSIVE,
                    AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                    aligned_hns,
                    aligned_hns,
                    format.as_wave(),
                    None,
                )
                .map_err(|retry| exclusive_refuse(&retry))?;
            let frames = client
                .GetBufferSize()
                .map_err(|retry| format!("no exclusive buffer size after align ({retry})"))?;
            match_period_to_buffer(device, format, client, aligned_hns, frames)
        }
        Err(error) => Err(exclusive_refuse(&error)),
    }
}

unsafe fn open_session(endpoint: &str) -> Result<Session, String> {
    let enumerator: IMMDeviceEnumerator =
        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .map_err(|error| format!("no device enumerator ({error})"))?;
    let device = enumerator
        .GetDevice(&HSTRING::from(endpoint))
        .map_err(|error| format!("the output was not found ({error})"))?;

    // Probe mix format / periods on a throwaway client, then open exclusive.
    let probe = activate_client(&device)?;
    let mix = probe
        .GetMixFormat()
        .map_err(|error| format!("no mix format ({error})"))?;
    let format = match pick_format(&probe, mix) {
        Ok(format) => {
            CoTaskMemFree(Some(mix as *const _));
            format
        }
        Err(reason) => {
            CoTaskMemFree(Some(mix as *const _));
            return Err(reason);
        }
    };

    let (mut default_hns, mut min_hns) = (0i64, 0i64);
    probe
        .GetDevicePeriod(Some(&mut default_hns), Some(&mut min_hns))
        .map_err(|error| driver_message("the driver reports no exclusive period", &error))?;
    drop(probe);
    if min_hns <= 0 {
        return Err("the driver reports no usable exclusive period".to_owned());
    }

    let period_hns = min_hns;
    let (client, buffer_frames, used_hns) = initialize_exclusive(&device, &format, period_hns)?;

    let event = EventHandle(
        CreateEventW(None, false, false, PCWSTR::null())
            .map_err(|error| format!("no wake-up event ({error})"))?,
    );
    client
        .SetEventHandle(event.0)
        .map_err(|error| format!("event setup failed ({error})"))?;
    let render: IAudioRenderClient = client
        .GetService()
        .map_err(|error| format!("no render client ({error})"))?;

    // Exclusive + event callback: each wake writes the full GetBufferSize()
    // buffer (MSDN).  That size — not the HNS period conversion — is the step.
    // Start is deferred until the mixer thread is ready and has seeded the
    // cushion; otherwise the device is already draining an empty ring.
    let step_frames = buffer_frames.max(1);
    let _ = used_hns;
    Ok(Session {
        client,
        render,
        event,
        info: ExclusiveInfo {
            rate: format.rate,
            channels: format.channels,
            step_frames,
            min_period_frames: hns_to_frames(min_hns, format.rate),
            default_period_frames: hns_to_frames(default_hns, format.rate),
        },
        format,
        buffer_frames: step_frames,
    })
}

fn report_failure(shared: &Shared, error: &windows::core::Error) {
    if error.code() == AUDCLNT_E_DEVICE_INVALIDATED {
        shared.set_device_lost("Output device disconnected".to_owned());
    } else {
        shared.set_error(format!("Exclusive output stopped: {error}"));
    }
}

unsafe fn write_exclusive_frame(
    dest: *mut u8,
    kind: SampleKind,
    channels: usize,
    frame_index: usize,
    mapped: &crate::audio::output_map::DeviceFrame,
) {
    match kind {
        SampleKind::F32 => {
            let samples = std::slice::from_raw_parts_mut(
                dest.cast::<f32>().add(frame_index * channels),
                channels,
            );
            write_frame(samples, mapped);
        }
        SampleKind::I16 => {
            let samples = std::slice::from_raw_parts_mut(
                dest.cast::<i16>().add(frame_index * channels),
                channels,
            );
            write_frame(samples, mapped);
        }
        SampleKind::I32 => {
            let samples = std::slice::from_raw_parts_mut(
                dest.cast::<i32>().add(frame_index * channels),
                channels,
            );
            write_frame(samples, mapped);
        }
        SampleKind::I24In32 => {
            let samples = std::slice::from_raw_parts_mut(
                dest.cast::<i32>().add(frame_index * channels),
                channels,
            );
            for (channel, sample) in samples.iter_mut().enumerate() {
                let f = mapped.sample(channel).clamp(-1.0, 1.0);
                // Pack 24-bit into the high bits of a 32-bit slot.
                let scaled = (f * 8_388_607.0).round() as i32;
                *sample = scaled << 8;
            }
        }
    }
}

unsafe fn run(
    endpoint: &str,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    ready: mpsc::Sender<Result<ExclusiveInfo, String>>,
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

    let mut task_index = 0u32;
    let mmcss = AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index).ok();

    let channels = session.info.channels;
    let kind = session.format.kind;
    let mut mixer = OutputMixer::new(session.info.rate, channels);
    mixer.follow_source_rate(&shared);
    mixer.publish_device_channels(&shared);
    // Exclusive event-driven: every signal requires a *full* endpoint buffer
    // (IAudioClient::GetBufferSize).  Shared REAL-style keeps one engine step
    // queued via GetCurrentPadding; that pattern underruns here and sounds
    // like a stutter / helicopter.  GetCurrentPadding is meaningless here
    // (MSDN / mpv): do not size the write from it.
    let frames = session.buffer_frames.max(1);
    mixer.prepare(frames as usize);
    // Seed the ring cushion from this step before the device starts asking
    // for audio.  set_output on the bridge thread does the same; doing it
    // here covers the window before that call lands.
    let source = match shared.source_rate.load(Ordering::Relaxed) {
        0 => session.info.rate.max(1),
        rate => rate,
    };
    let source_step = (frames as u64 * source as u64)
        .div_ceil(session.info.rate.max(1) as u64)
        .max(1) as usize;
    shared.note_output_block(source_step);

    // Prefill one silent buffer, then Start — same order as the MSDN exclusive
    // event sample and OpenAL Soft.  Only then tell the bridge we are ready.
    if session.render.GetBuffer(frames).is_ok() {
        let _ = session
            .render
            .ReleaseBuffer(frames, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32);
    }
    if let Err(error) = session.client.Start() {
        let _ = ready.send(Err(format!(
            "the exclusive stream would not start ({error})"
        )));
        if let Some(handle) = mmcss {
            let _ = AvRevertMmThreadCharacteristics(handle);
        }
        drop(session);
        if com {
            CoUninitialize();
        }
        return;
    }
    if ready.send(Ok(session.info.clone())).is_err() {
        if let Some(handle) = mmcss {
            let _ = AvRevertMmThreadCharacteristics(handle);
        }
        drop(session);
        if com {
            CoUninitialize();
        }
        return;
    }
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
        let data = match session.render.GetBuffer(frames) {
            Ok(data) => data,
            Err(error) => {
                report_failure(&shared, &error);
                break;
            }
        };
        mixer.render(&shared, frames as usize, |index, mapped| {
            if channels > 0 {
                push_device_frame(&shared.recorder, RecordSource::Clock, mapped, channels);
            }
            write_exclusive_frame(data, kind, channels, index, mapped);
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

#[cfg(test)]
mod tests;
