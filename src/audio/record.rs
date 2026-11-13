//! WAV capture of one armed endpoint.
//!
//! A `rigwerk-record` thread is the only place that allocates the ring,
//! creates directories, or writes the file.  The audio callback and the UI
//! never call into that write, so a slow disk cannot block them.  The
//! callback copies one frame into the ring and returns.  [`Recorder::arm`]
//! and [`Recorder::disarm`] only publish a request and return.  Neither
//! joins the thread.  Dropping a [`Recorder`] only asks it to finish, and
//! the thread holds an `Arc` of the recorder state so the ring and the file
//! outlive the handle.
//!
//! The ring is the queue between the callback and that thread: five seconds
//! at the armed rate ([`RECORD_SECONDS`]).  It is not what keeps either
//! side realtime.  Past that depth a frame is counted and left out, and the
//! callback still returns.  The ring is allocated when the writer opens the
//! file, and freed only after the phase has left [`RecordPhase::Recording`]
//! and every in-flight [`Recorder::push`] has dropped its hold.  The writer
//! drains the queue as it runs and patches the WAV sizes about once a
//! second, so a crash leaves a readable file of whatever already reached
//! disk.

use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{
    AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering,
};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use super::output_map::{DeviceFrame, FRAME_LANES};

/// How much audio the ring holds, at the rate the take was armed with.
pub(crate) const RECORD_SECONDS: u32 = 5;

/// Which endpoint's callback is allowed to feed the armed take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecordSource {
    Clock,
    Follower(usize),
}

/// Where a take is.  The audio thread may read this; it never waits on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum RecordPhase {
    Idle = 0,
    Arming = 1,
    Recording = 2,
    Finishing = 3,
    Failed = 4,
}

const PHASE_IDLE: u8 = RecordPhase::Idle as u8;
const PHASE_ARMING: u8 = RecordPhase::Arming as u8;
const PHASE_RECORDING: u8 = RecordPhase::Recording as u8;
const PHASE_FINISHING: u8 = RecordPhase::Finishing as u8;
const PHASE_FAILED: u8 = RecordPhase::Failed as u8;

/// What the UI shows.  `frames` counts frames the callback pushed, which
/// runs ahead of the file until the writer next drains.
#[derive(Clone, Debug)]
pub(crate) struct RecordStatus {
    pub(crate) phase: RecordPhase,
    pub(crate) overruns: u64,
    pub(crate) frames: u64,
    pub(crate) rate: u32,
    /// Channel count the take was armed with.
    #[cfg_attr(not(test), allow(dead_code))] // Live row; no non-test reader yet.
    pub(crate) channels: usize,
    pub(crate) error: Option<String>,
    pub(crate) path: Option<String>,
}

/// Bytes of a float WAVEFORMATEXTENSIBLE header, through the data-size field.
const HEADER_LEN: u64 = 68;
const RIFF_SIZE_OFFSET: u64 = 4;
const DATA_SIZE_OFFSET: u64 = 64;
/// `ChunkSize` counts everything after itself.  Header is 68 bytes, so an
/// empty file is `68 - 8`.
const RIFF_BODY: u64 = HEADER_LEN - 8;

/// KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, mixed-endian WAV GUID layout.
const IEEE_FLOAT_GUID: [u8; 16] = [
    0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71,
];

/// Clock encodes as 0.  A follower encodes as `index + 1`, so the two never
/// share a value for any index a device list can hold.
const SOURCE_CLOCK: u64 = 0;

/// How often the writer patches the WAV sizes.  Samples are drained on
/// every pass; this only rewrites the header.
const HEADER_INTERVAL: Duration = Duration::from_secs(1);
/// How long the writer sleeps when the queue is empty.  The audio thread
/// never sleeps; this is the writer thread only.
const IDLE_POLL: Duration = Duration::from_millis(5);

/// A take the UI asked for.  Copied out of the mutex before any file call.
struct TakeRequest {
    path: PathBuf,
    rate: u32,
    channels: usize,
    source: RecordSource,
}

/// Strings and the pending request.  The audio thread never locks this.
/// The writer holds it only long enough to move those fields.
struct Meta {
    request: Option<TakeRequest>,
    error: Option<String>,
    path: Option<String>,
}

/// Shared by the handle, the callback, and the writer thread.
struct Inner {
    phase: AtomicU8,
    /// Pushes currently between their phase check and their last use of the
    /// ring.  The writer spins on this after leaving `Recording`, and only
    /// then frees the ring.
    inflight: AtomicUsize,
    /// Writer threads that have not exited.  `Failed` is published before
    /// the ring is freed, so phase alone would let a new arm start too soon.
    writers: AtomicUsize,
    overruns: AtomicU64,
    frames: AtomicU64,
    rate: AtomicU32,
    channels: AtomicUsize,
    source: AtomicU64,
    stop: AtomicBool,
    ring: AtomicPtr<RecordRing>,
    meta: Mutex<Meta>,
}

impl Inner {
    fn new() -> Self {
        Self {
            phase: AtomicU8::new(PHASE_IDLE),
            inflight: AtomicUsize::new(0),
            writers: AtomicUsize::new(0),
            overruns: AtomicU64::new(0),
            frames: AtomicU64::new(0),
            rate: AtomicU32::new(0),
            channels: AtomicUsize::new(0),
            source: AtomicU64::new(SOURCE_CLOCK),
            stop: AtomicBool::new(false),
            ring: AtomicPtr::new(std::ptr::null_mut()),
            meta: Mutex::new(Meta {
                request: None,
                error: None,
                path: None,
            }),
        }
    }
}

/// SPSC ring of interleaved `f32` frames.  One producer (the armed
/// callback), one consumer (the writer).  Slots are `f32` bits.  The
/// producer writes a frame, then release-stores `head`.  The consumer
/// acquire-loads `head`, reads, then release-stores `tail`.
struct RecordRing {
    slots: Vec<AtomicU32>,
    capacity: u64,
    channels: usize,
    head: AtomicU64,
    tail: AtomicU64,
}

impl RecordRing {
    fn new(rate: u32, channels: usize) -> Self {
        let channels = channels.clamp(1, FRAME_LANES);
        let capacity = (rate as usize)
            .saturating_mul(RECORD_SECONDS as usize)
            .max(1);
        Self {
            slots: (0..capacity.saturating_mul(channels))
                .map(|_| AtomicU32::new(0))
                .collect(),
            capacity: capacity as u64,
            channels,
            head: AtomicU64::new(0),
            tail: AtomicU64::new(0),
        }
    }

    /// `false` when the ring is full.  The caller counts an overrun and
    /// drops the frame.  Never blocks.
    fn push(&self, frame: &[f32]) -> bool {
        let capacity = self.capacity;
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        if head.wrapping_sub(tail) >= capacity {
            return false;
        }
        let index = (head % capacity) as usize * self.channels;
        for (channel, slot) in self.slots[index..index + self.channels].iter().enumerate() {
            let sample = frame.get(channel).copied().unwrap_or(0.0);
            slot.store(sample.to_bits(), Ordering::Relaxed);
        }
        self.head.store(head.wrapping_add(1), Ordering::Release);
        true
    }

    /// `false` when empty.  `frame` is one sample per armed channel.
    fn pop(&self, frame: &mut [f32]) -> bool {
        let capacity = self.capacity;
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        if tail == head {
            return false;
        }
        let index = (tail % capacity) as usize * self.channels;
        for (sample, slot) in frame
            .iter_mut()
            .zip(&self.slots[index..index + self.channels])
        {
            *sample = f32::from_bits(slot.load(Ordering::Relaxed));
        }
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        true
    }
}

/// Frees the published ring unless [`Self::release`] already did.  A panic
/// on the writer still leaves `Recording` and waits out in-flight pushes
/// before the free, so the callback cannot observe a dangling pointer.
struct RingGuard<'a> {
    inner: &'a Inner,
    live: bool,
}

impl<'a> RingGuard<'a> {
    fn publish(inner: &'a Inner, rate: u32, channels: usize) -> Self {
        let ptr = Box::into_raw(Box::new(RecordRing::new(rate, channels)));
        inner.ring.store(ptr, Ordering::Release);
        Self { inner, live: true }
    }

    /// Phase has already left `Recording` and the caller has drained.
    fn release(&mut self) {
        if !self.live {
            return;
        }
        while self.inner.inflight.load(Ordering::SeqCst) != 0 {
            std::hint::spin_loop();
        }
        free_ring(self.inner);
        self.live = false;
    }
}

impl Drop for RingGuard<'_> {
    fn drop(&mut self) {
        if !self.live {
            return;
        }
        // `Finishing`, not `Failed`: the terminal phase is published only
        // after `writers` hits zero, so a caller that sees `Failed` can arm
        // again.  `Finishing` is enough to stop pushes before the free.
        if self.inner.phase.load(Ordering::SeqCst) == PHASE_RECORDING {
            self.inner.phase.store(PHASE_FINISHING, Ordering::SeqCst);
        }
        while self.inner.inflight.load(Ordering::SeqCst) != 0 {
            std::hint::spin_loop();
        }
        free_ring(self.inner);
        self.live = false;
    }
}

/// Not a real phase.  Left in place when the writer panics before deciding.
const PHASE_UNSET: u8 = 255;

/// Publishes `Idle` or `Failed` only after this thread has dropped
/// `writers`.  A caller that observes either phase can arm again; the
/// previous thread is no longer touching the ring or the file.
struct Publish<'a> {
    inner: &'a Inner,
    terminal: u8,
}

impl Drop for Publish<'_> {
    fn drop(&mut self) {
        let mut terminal = self.terminal;
        if terminal != PHASE_IDLE && terminal != PHASE_FAILED {
            if self.inner.phase.load(Ordering::SeqCst) == PHASE_RECORDING {
                self.inner.phase.store(PHASE_FINISHING, Ordering::SeqCst);
            }
            while self.inner.inflight.load(Ordering::SeqCst) != 0 {
                std::hint::spin_loop();
            }
            free_ring(self.inner);
            let mut meta = lock_meta(&self.inner.meta);
            if meta.error.is_none() {
                meta.error = Some("recording stopped unexpectedly".to_owned());
            }
            terminal = PHASE_FAILED;
        }
        self.inner.writers.fetch_sub(1, Ordering::Release);
        self.inner.phase.store(terminal, Ordering::SeqCst);
    }
}

/// Hold across a push that might touch the ring.  Drop decrements, so a
/// panic cannot leave the writer's spin waiting forever.
struct Inflight<'a>(&'a AtomicUsize);

impl Drop for Inflight<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Handle the engine keeps.  Cheap to create: no thread, no ring, no file.
pub(crate) struct Recorder {
    inner: Arc<Inner>,
}

impl Recorder {
    /// Cheap.  Does not spawn a thread and does not allocate the ring.
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(Inner::new()),
        }
    }

    /// Audio thread.  Wait-free.  Copies `frame` (one sample per channel).
    /// No-op unless the phase is `Recording` and `source` is the armed
    /// source.  A full ring increments the overrun count and drops the
    /// frame.  Never allocates, never locks, never writes.
    #[inline]
    pub(crate) fn push(&self, source: RecordSource, frame: &[f32]) {
        let inner = &self.inner;
        // Stale "not recording" only drops a frame.  The slow path below
        // re-checks under `inflight` before it touches the pointer.
        if inner.phase.load(Ordering::Relaxed) != PHASE_RECORDING {
            return;
        }
        // SeqCst pairs with the writer's phase store and inflight spin.  If
        // this load still observes `Recording`, the increment is visible to
        // that spin, so the free cannot run until this push has returned.
        inner.inflight.fetch_add(1, Ordering::SeqCst);
        let _hold = Inflight(&inner.inflight);
        if inner.phase.load(Ordering::SeqCst) != PHASE_RECORDING {
            return;
        }
        if inner.source.load(Ordering::Acquire) != encode_source(source) {
            return;
        }
        let ptr = inner.ring.load(Ordering::Acquire);
        if ptr.is_null() {
            return;
        }
        // SAFETY: the writer release-stores this pointer before the SeqCst
        // store of `Recording`, and it does not free it until after a SeqCst
        // store that leaves `Recording` and a spin until `inflight` is 0.
        // This push observed `Recording` while holding `inflight`.
        let ring = unsafe { &*ptr };
        if ring.push(frame) {
            inner.frames.fetch_add(1, Ordering::Relaxed);
        } else {
            inner.overruns.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// UI or engine thread.  Returns immediately.  Does not open a file,
    /// allocate the ring, or join.  CAS phase from `Idle` or `Failed` to
    /// `Arming` and hands the request to a writer thread.  Returns false if
    /// a take is already arming, recording, or finishing (or the previous
    /// writer has not exited).  `channels` is clamped to at least 1 and at
    /// most [`FRAME_LANES`] (`output_map::SLOTS`).  A `rate` of 0 fails the
    /// arm on the writer (phase `Failed`, error set) without blocking the
    /// caller: this returns true once the request is accepted, and false
    /// only when a take is already in progress.
    pub(crate) fn arm(
        &self,
        path: PathBuf,
        rate: u32,
        channels: usize,
        source: RecordSource,
    ) -> bool {
        if !self.try_begin() {
            return false;
        }
        let channels = channels.clamp(1, FRAME_LANES);
        // A disarm from the previous take must not stick.  Anything set
        // after this store belongs to the take we just accepted.
        self.inner.stop.store(false, Ordering::Release);
        self.inner.frames.store(0, Ordering::Relaxed);
        self.inner.overruns.store(0, Ordering::Relaxed);
        self.inner.rate.store(rate, Ordering::Relaxed);
        self.inner.channels.store(channels, Ordering::Relaxed);
        {
            let mut meta = lock_meta(&self.inner.meta);
            meta.error = None;
            meta.path = Some(path.display().to_string());
            meta.request = Some(TakeRequest {
                path,
                rate,
                channels,
                source,
            });
        }
        self.inner.writers.fetch_add(1, Ordering::AcqRel);
        let inner = Arc::clone(&self.inner);
        let spawned = thread::Builder::new()
            .name("rigwerk-record".to_owned())
            .spawn(move || {
                let mut publish = Publish {
                    inner: &inner,
                    terminal: PHASE_UNSET,
                };
                record_take(&inner, &mut publish);
            });
        if let Err(err) = spawned {
            // The closure never ran, so `Publish` will not drop.
            self.inner.writers.fetch_sub(1, Ordering::Release);
            fail_take(
                &self.inner,
                &format!("failed to start the recording thread: {err}"),
            );
        }
        true
    }

    /// UI or engine thread.  Returns immediately.  Does not join and does
    /// not write.  The writer drains, patches the header, and goes `Idle`.
    pub(crate) fn disarm(&self) {
        self.inner.stop.store(true, Ordering::Release);
    }

    pub(crate) fn status(&self) -> RecordStatus {
        let (error, path) = {
            let meta = lock_meta(&self.inner.meta);
            (meta.error.clone(), meta.path.clone())
        };
        RecordStatus {
            phase: phase_from_u8(self.inner.phase.load(Ordering::Acquire)),
            overruns: self.inner.overruns.load(Ordering::Relaxed),
            frames: self.inner.frames.load(Ordering::Relaxed),
            rate: self.inner.rate.load(Ordering::Relaxed),
            channels: self.inner.channels.load(Ordering::Relaxed),
            error,
            path,
        }
    }

    /// False when a writer is still alive or the phase is not a terminal one.
    fn try_begin(&self) -> bool {
        if self.inner.writers.load(Ordering::Acquire) != 0 {
            return false;
        }
        let mut phase = self.inner.phase.load(Ordering::Acquire);
        loop {
            if phase != PHASE_IDLE && phase != PHASE_FAILED {
                return false;
            }
            match self.inner.phase.compare_exchange(
                phase,
                PHASE_ARMING,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(seen) => phase = seen,
            }
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        // Do not join.  The writer owns an `Arc<Inner>` and finishes alone.
        self.disarm();
    }
}

/// Audio thread helper.  Stack buffer only.  Copies `channels` samples via
/// [`DeviceFrame::sample`] and pushes them.  `channels` is the endpoint's
/// channel count.  Past [`FRAME_LANES`] there is no distinct slot, and a
/// take never arms more than that, so the copy stops there.
#[inline]
pub(crate) fn push_device_frame(
    recorder: &Recorder,
    source: RecordSource,
    mapped: &DeviceFrame,
    channels: usize,
) {
    let count = channels.min(FRAME_LANES);
    let mut frame = [0.0f32; FRAME_LANES];
    for (channel, slot) in frame.iter_mut().enumerate().take(count) {
        *slot = mapped.sample(channel);
    }
    recorder.push(source, &frame[..count]);
}

fn encode_source(source: RecordSource) -> u64 {
    match source {
        RecordSource::Clock => SOURCE_CLOCK,
        RecordSource::Follower(index) => (index as u64).saturating_add(1),
    }
}

fn phase_from_u8(value: u8) -> RecordPhase {
    match value {
        PHASE_ARMING => RecordPhase::Arming,
        PHASE_RECORDING => RecordPhase::Recording,
        PHASE_FINISHING => RecordPhase::Finishing,
        PHASE_FAILED => RecordPhase::Failed,
        _ => RecordPhase::Idle,
    }
}

fn lock_meta(meta: &Mutex<Meta>) -> MutexGuard<'_, Meta> {
    meta.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn note_error(inner: &Inner, message: &str) {
    let mut meta = lock_meta(&inner.meta);
    meta.error = Some(message.to_owned());
}

/// Spawn failed on the caller thread: there is no writer left to publish
/// the phase.  `writers` has already been decremented.
fn fail_take(inner: &Inner, message: &str) {
    note_error(inner, message);
    inner.phase.store(PHASE_FAILED, Ordering::SeqCst);
}

/// Remember a writer-side failure.  Pushes stop immediately (`Finishing`);
/// `Failed` itself is stored when the thread exits.
fn fail_writer(inner: &Inner, publish: &mut Publish<'_>, message: &str) {
    note_error(inner, message);
    if inner.phase.load(Ordering::SeqCst) == PHASE_RECORDING {
        inner.phase.store(PHASE_FINISHING, Ordering::SeqCst);
    }
    publish.terminal = PHASE_FAILED;
}

fn take_request(inner: &Inner) -> Option<TakeRequest> {
    let mut meta = lock_meta(&inner.meta);
    meta.request.take()
}

fn record_take(inner: &Inner, publish: &mut Publish<'_>) {
    let Some(request) = take_request(inner) else {
        publish.terminal = PHASE_IDLE;
        return;
    };
    if request.rate == 0 {
        fail_writer(inner, publish, "sample rate is zero");
        return;
    }
    if inner.stop.load(Ordering::Acquire) {
        publish.terminal = PHASE_IDLE;
        return;
    }
    if let Err(err) = ensure_parent(&request.path) {
        fail_writer(inner, publish, &err.to_string());
        return;
    }
    let mut file = match File::create(&request.path) {
        Ok(file) => file,
        Err(err) => {
            fail_writer(inner, publish, &err.to_string());
            return;
        }
    };
    let header = match wav_header(request.rate, request.channels) {
        Ok(header) => header,
        Err(err) => {
            fail_writer(inner, publish, &err.to_string());
            return;
        }
    };
    if let Err(err) = file.write_all(&header) {
        fail_writer(inner, publish, &err.to_string());
        return;
    }
    if inner.stop.load(Ordering::Acquire) {
        publish.terminal = PHASE_IDLE;
        return;
    }

    let mut guard = RingGuard::publish(inner, request.rate, request.channels);
    inner
        .source
        .store(encode_source(request.source), Ordering::Release);
    inner.rate.store(request.rate, Ordering::Relaxed);
    inner.channels.store(request.channels, Ordering::Relaxed);
    inner.frames.store(0, Ordering::Relaxed);
    inner.overruns.store(0, Ordering::Relaxed);
    // The pointer and the source are published before this store.  A push
    // that acquire/SeqCst-loads `Recording` therefore sees both.
    inner.phase.store(PHASE_RECORDING, Ordering::SeqCst);

    let mut data_bytes = 0u64;
    let mut pending = Vec::with_capacity(16 * 1024);
    let mut next_header = Instant::now() + HEADER_INTERVAL;
    let mut failed: Option<String> = None;
    loop {
        if inner.stop.load(Ordering::Acquire) {
            break;
        }
        if let Err(err) = drain_ring(inner, &mut file, &mut pending, &mut data_bytes) {
            failed = Some(err.to_string());
            break;
        }
        if Instant::now() >= next_header {
            if let Err(err) = patch_sizes(&mut file, data_bytes) {
                failed = Some(err.to_string());
                break;
            }
            next_header = Instant::now() + HEADER_INTERVAL;
        }
        thread::sleep(IDLE_POLL);
    }

    if let Some(message) = failed {
        // `Finishing` stops pushes.  The guard then frees the ring.  Samples
        // still queued are dropped; the header covers bytes already written.
        // `Failed` is published when this thread exits.
        fail_writer(inner, publish, &message);
        let _ = patch_sizes(&mut file, data_bytes);
        return;
    }

    // Stop accepting pushes before the final drain, then wait until every
    // push that observed `Recording` has stored its frame.
    inner.phase.store(PHASE_FINISHING, Ordering::SeqCst);
    while inner.inflight.load(Ordering::SeqCst) != 0 {
        std::hint::spin_loop();
    }
    if let Err(err) = drain_ring(inner, &mut file, &mut pending, &mut data_bytes) {
        fail_writer(inner, publish, &err.to_string());
        let _ = patch_sizes(&mut file, data_bytes);
        return;
    }
    guard.release();
    if let Err(err) = patch_sizes(&mut file, data_bytes) {
        fail_writer(inner, publish, &err.to_string());
        return;
    }
    drop(file);
    publish.terminal = PHASE_IDLE;
}

fn ensure_parent(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    Ok(())
}

fn channel_mask(channels: usize) -> u32 {
    match channels {
        1 => 0x1,
        2 => 0x3,
        _ => 0,
    }
}

fn wav_header(rate: u32, channels: usize) -> io::Result<[u8; HEADER_LEN as usize]> {
    let channels_u16 = u16::try_from(channels).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "too many channels for a WAV file",
        )
    })?;
    let block_align = u32::from(channels_u16)
        .checked_mul(4)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "WAV block align overflows"))?;
    let byte_rate = rate
        .checked_mul(block_align)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "WAV byte rate overflows"))?;
    let mut header = [0u8; HEADER_LEN as usize];
    header[0..4].copy_from_slice(b"RIFF");
    header[4..8].copy_from_slice(&(RIFF_BODY as u32).to_le_bytes());
    header[8..12].copy_from_slice(b"WAVE");
    header[12..16].copy_from_slice(b"fmt ");
    header[16..20].copy_from_slice(&40u32.to_le_bytes());
    header[20..22].copy_from_slice(&0xFFFEu16.to_le_bytes());
    header[22..24].copy_from_slice(&channels_u16.to_le_bytes());
    header[24..28].copy_from_slice(&rate.to_le_bytes());
    header[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    header[32..34].copy_from_slice(&(block_align as u16).to_le_bytes());
    header[34..36].copy_from_slice(&32u16.to_le_bytes());
    header[36..38].copy_from_slice(&22u16.to_le_bytes());
    header[38..40].copy_from_slice(&32u16.to_le_bytes());
    header[40..44].copy_from_slice(&channel_mask(channels).to_le_bytes());
    header[44..60].copy_from_slice(&IEEE_FLOAT_GUID);
    header[60..64].copy_from_slice(b"data");
    header[64..68].copy_from_slice(&0u32.to_le_bytes());
    Ok(header)
}

fn patch_sizes(file: &mut File, data_bytes: u64) -> io::Result<()> {
    if data_bytes > u64::from(u32::MAX) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "recording is larger than a WAV file can hold",
        ));
    }
    let riff_size = RIFF_BODY + data_bytes;
    if riff_size > u64::from(u32::MAX) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "recording is larger than a WAV file can hold",
        ));
    }
    file.seek(SeekFrom::Start(RIFF_SIZE_OFFSET))?;
    file.write_all(&(riff_size as u32).to_le_bytes())?;
    file.seek(SeekFrom::Start(DATA_SIZE_OFFSET))?;
    file.write_all(&(data_bytes as u32).to_le_bytes())?;
    file.seek(SeekFrom::Start(HEADER_LEN + data_bytes))?;
    Ok(())
}

/// Pulls every queued frame into the file.  Runs on the writer thread.
fn drain_ring(
    inner: &Inner,
    file: &mut File,
    pending: &mut Vec<u8>,
    data_bytes: &mut u64,
) -> io::Result<()> {
    let ptr = inner.ring.load(Ordering::Acquire);
    if ptr.is_null() {
        return Ok(());
    }
    // SAFETY: this thread published the pointer and has not freed it yet.
    // Pushes may run alongside this drain only while the phase is still
    // `Recording`; the final drain runs after `inflight` has hit 0.
    let ring = unsafe { &*ptr };
    let channels = ring.channels;
    let mut frame = [0.0f32; FRAME_LANES];
    pending.clear();
    while ring.pop(&mut frame[..channels]) {
        for sample in &frame[..channels] {
            pending.extend_from_slice(&sample.to_le_bytes());
        }
        if pending.len() >= 16 * 1024 {
            write_pending(file, pending, data_bytes)?;
        }
    }
    if !pending.is_empty() {
        write_pending(file, pending, data_bytes)?;
    }
    Ok(())
}

fn write_pending(file: &mut File, pending: &mut Vec<u8>, data_bytes: &mut u64) -> io::Result<()> {
    file.write_all(pending)?;
    *data_bytes += pending.len() as u64;
    pending.clear();
    Ok(())
}

fn free_ring(inner: &Inner) {
    let ptr = inner.ring.swap(std::ptr::null_mut(), Ordering::AcqRel);
    if ptr.is_null() {
        return;
    }
    // SAFETY: `inflight` is 0 and the phase is no longer `Recording`, so no
    // push is dereferencing this pointer.  This thread is the sole owner.
    drop(unsafe { Box::from_raw(ptr) });
}

#[cfg(test)]
mod tests;
