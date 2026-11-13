use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::{push_device_frame, RecordPhase, RecordSource, RecordStatus, Recorder, RECORD_SECONDS};
use crate::audio::output_map::DeviceFrame;

const IEEE_FLOAT_GUID: [u8; 16] = [
    0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71,
];

struct TempTake {
    dir: PathBuf,
}

impl TempTake {
    fn new(name: &str) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("rigwerk-record-{name}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Self { dir }
    }

    /// A path whose parent does not exist yet.  The writer creates it.
    fn wav(&self) -> PathBuf {
        self.dir.join("nested").join("take.wav")
    }
}

impl Drop for TempTake {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Wav {
    rate: u32,
    channels: u16,
    mask: u32,
    data_size: u32,
    samples: Vec<f32>,
}

fn read_wav(path: &std::path::Path) -> Wav {
    let bytes = std::fs::read(path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
    assert!(bytes.len() >= 68, "header truncated: {} bytes", bytes.len());
    assert_eq!(&bytes[0..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WAVE");
    assert_eq!(&bytes[12..16], b"fmt ");
    assert_eq!(u32::from_le_bytes(bytes[16..20].try_into().unwrap()), 40);
    assert_eq!(
        u16::from_le_bytes(bytes[20..22].try_into().unwrap()),
        0xFFFE
    );
    let channels = u16::from_le_bytes(bytes[22..24].try_into().unwrap());
    let rate = u32::from_le_bytes(bytes[24..28].try_into().unwrap());
    let byte_rate = u32::from_le_bytes(bytes[28..32].try_into().unwrap());
    let block_align = u16::from_le_bytes(bytes[32..34].try_into().unwrap());
    assert_eq!(block_align, channels * 4);
    assert_eq!(byte_rate, rate * u32::from(block_align));
    assert_eq!(u16::from_le_bytes(bytes[34..36].try_into().unwrap()), 32);
    assert_eq!(u16::from_le_bytes(bytes[36..38].try_into().unwrap()), 22);
    assert_eq!(u16::from_le_bytes(bytes[38..40].try_into().unwrap()), 32);
    let mask = u32::from_le_bytes(bytes[40..44].try_into().unwrap());
    assert_eq!(&bytes[44..60], &IEEE_FLOAT_GUID);
    assert_eq!(&bytes[60..64], b"data");
    let data_size = u32::from_le_bytes(bytes[64..68].try_into().unwrap());
    let riff_size = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    assert_eq!(riff_size as usize, bytes.len() - 8, "RIFF size");
    assert_eq!(data_size as usize, bytes.len() - 68, "data size");
    assert_eq!(data_size % 4, 0);
    let mut samples = Vec::with_capacity(data_size as usize / 4);
    for chunk in bytes[68..68 + data_size as usize].chunks_exact(4) {
        samples.push(f32::from_le_bytes(chunk.try_into().unwrap()));
    }
    Wav {
        rate,
        channels,
        mask,
        data_size,
        samples,
    }
}

fn wait_until(recorder: &Recorder, done: impl Fn(&RecordStatus) -> bool) -> RecordStatus {
    let start = Instant::now();
    loop {
        let status = recorder.status();
        if done(&status) {
            return status;
        }
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "timed out, last status {status:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn wait_phase(recorder: &Recorder, phase: RecordPhase) -> RecordStatus {
    let status = wait_until(recorder, |status| {
        status.phase == phase || status.phase == RecordPhase::Failed
    });
    assert_eq!(status.phase, phase, "{status:?}");
    status
}

fn finish(recorder: &Recorder) -> RecordStatus {
    recorder.disarm();
    wait_phase(recorder, RecordPhase::Idle)
}

#[test]
fn a_full_ring_records_exactly_those_frames() {
    let rate = 8u32;
    let capacity = rate as usize * RECORD_SECONDS as usize;
    assert_eq!(RECORD_SECONDS, 5);
    assert_eq!(capacity, 40);

    let temp = TempTake::new("full");
    let recorder = Recorder::new();
    assert!(recorder.arm(temp.wav(), rate, 1, RecordSource::Clock));
    wait_phase(&recorder, RecordPhase::Recording);

    let mut pushed = Vec::with_capacity(capacity);
    for index in 0..capacity {
        let sample = index as f32;
        recorder.push(RecordSource::Clock, &[sample]);
        pushed.push(sample);
    }
    // The next frame must not block, and it must not land in the file.
    recorder.push(RecordSource::Clock, &[42.0]);
    let live = recorder.status();
    assert_eq!(live.phase, RecordPhase::Recording);
    assert_eq!(live.frames, capacity as u64);
    assert_eq!(live.overruns, 1);
    assert_eq!(live.rate, rate);
    assert_eq!(live.channels, 1);

    let done = finish(&recorder);
    assert_eq!(done.frames, capacity as u64);
    assert_eq!(done.overruns, 1);
    assert!(done.error.is_none(), "{done:?}");

    let wav = read_wav(&temp.wav());
    assert_eq!(wav.rate, rate);
    assert_eq!(wav.channels, 1);
    assert_eq!(wav.mask, 0x1);
    assert_eq!(wav.data_size as usize, pushed.len() * 4);
    assert_eq!(wav.samples, pushed);
}

#[test]
fn frames_are_channel_interleaved_and_other_sources_are_ignored() {
    let temp = TempTake::new("interleaved");
    let recorder = Recorder::new();
    assert!(recorder.arm(temp.wav(), 8, 2, RecordSource::Follower(1)));
    wait_phase(&recorder, RecordPhase::Recording);

    recorder.push(RecordSource::Clock, &[9.0, 9.0]);
    recorder.push(RecordSource::Follower(0), &[8.0, 8.0]);
    recorder.push(RecordSource::Follower(1), &[0.25, -0.5]);
    recorder.push(RecordSource::Follower(2), &[7.0, 7.0]);
    recorder.push(RecordSource::Follower(1), &[0.75, 0.125]);
    let live = recorder.status();
    assert_eq!(live.frames, 2);
    assert_eq!(live.overruns, 0);

    let done = finish(&recorder);
    assert!(done.error.is_none(), "{done:?}");
    let wav = read_wav(&temp.wav());
    assert_eq!(wav.channels, 2);
    assert_eq!(wav.mask, 0x3);
    assert_eq!(wav.rate, 8);
    assert_eq!(wav.samples, vec![0.25, -0.5, 0.75, 0.125]);
    assert_eq!(wav.data_size as usize, wav.samples.len() * 4);
}

#[test]
fn arm_returns_false_while_a_take_is_recording() {
    let temp = TempTake::new("busy");
    let recorder = Recorder::new();
    let path = temp.wav();
    assert!(recorder.arm(path.clone(), 8, 1, RecordSource::Clock));
    wait_phase(&recorder, RecordPhase::Recording);
    assert!(!recorder.arm(path, 48_000, 2, RecordSource::Follower(0)));
    let status = recorder.status();
    assert_eq!(status.phase, RecordPhase::Recording);
    assert_eq!(status.rate, 8);
    assert_eq!(status.channels, 1);
    let done = finish(&recorder);
    assert!(done.error.is_none(), "{done:?}");
    assert!(recorder.arm(temp.wav(), 8, 1, RecordSource::Clock));
    wait_phase(&recorder, RecordPhase::Recording);
    finish(&recorder);
}

#[test]
fn a_push_while_idle_does_nothing() {
    let recorder = Recorder::new();
    let mapped = DeviceFrame::from_lanes(&[0.5, -0.25], 2);
    recorder.push(RecordSource::Clock, &[0.5, -0.25]);
    push_device_frame(&recorder, RecordSource::Follower(0), &mapped, 2);
    recorder.disarm();
    let status = recorder.status();
    assert_eq!(status.phase, RecordPhase::Idle);
    assert_eq!(status.frames, 0);
    assert_eq!(status.overruns, 0);
    assert_eq!(status.rate, 0);
    assert!(status.error.is_none());
    assert!(status.path.is_none());
}

#[test]
fn a_zero_rate_fails_on_the_writer_and_a_later_arm_works() {
    let temp = TempTake::new("rate");
    let recorder = Recorder::new();
    assert!(recorder.arm(temp.wav(), 0, 2, RecordSource::Clock));
    let failed = wait_until(&recorder, |status| status.phase == RecordPhase::Failed);
    assert_eq!(failed.phase, RecordPhase::Failed);
    assert!(
        failed
            .error
            .as_deref()
            .is_some_and(|message| message.contains("sample rate")),
        "{failed:?}"
    );
    assert!(!temp.wav().exists());

    assert!(recorder.arm(temp.wav(), 8, 1, RecordSource::Clock));
    wait_phase(&recorder, RecordPhase::Recording);
    recorder.push(RecordSource::Clock, &[0.5]);
    let done = finish(&recorder);
    assert!(done.error.is_none(), "{done:?}");
    assert_eq!(read_wav(&temp.wav()).samples, vec![0.5]);
}

#[test]
fn push_device_frame_writes_the_mapped_channels() {
    let temp = TempTake::new("device");
    let recorder = Recorder::new();
    assert!(recorder.arm(temp.wav(), 8, 2, RecordSource::Clock));
    wait_phase(&recorder, RecordPhase::Recording);
    let mapped = DeviceFrame::from_lanes(&[0.25, -0.5, 0.75], 2);
    push_device_frame(&recorder, RecordSource::Follower(3), &mapped, 2);
    push_device_frame(&recorder, RecordSource::Clock, &mapped, 2);
    let done = finish(&recorder);
    assert!(done.error.is_none(), "{done:?}");
    assert_eq!(done.frames, 1);
    assert_eq!(read_wav(&temp.wav()).samples, vec![0.25, -0.5]);
}

#[test]
fn channel_count_is_clamped_and_past_stereo_the_mask_is_zero() {
    let temp = TempTake::new("clamp");
    let recorder = Recorder::new();
    assert!(recorder.arm(temp.wav(), 8, 0, RecordSource::Clock));
    wait_phase(&recorder, RecordPhase::Recording);
    assert_eq!(recorder.status().channels, 1);
    recorder.push(RecordSource::Clock, &[0.5, 0.25]);
    let done = finish(&recorder);
    assert!(done.error.is_none(), "{done:?}");
    let wav = read_wav(&temp.wav());
    assert_eq!(wav.channels, 1);
    assert_eq!(wav.mask, 0x1);
    assert_eq!(wav.samples, vec![0.5]);

    assert!(recorder.arm(temp.wav(), 8, 100, RecordSource::Clock));
    wait_phase(&recorder, RecordPhase::Recording);
    assert_eq!(recorder.status().channels, 9);
    let frame = [0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0];
    recorder.push(RecordSource::Clock, &frame);
    let done = finish(&recorder);
    assert!(done.error.is_none(), "{done:?}");
    let wav = read_wav(&temp.wav());
    assert_eq!(wav.channels, 9);
    assert_eq!(wav.mask, 0);
    assert_eq!(wav.samples, &frame[..9]);
}
