//! Decimated peak-hold scope lanes for the Live page.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Decimated scope bins shown in the Live page. About 250 ms of audio when
/// the hop follows the stream rate.
pub(crate) const SCOPE_LEN: usize = 256;

/// Peak-hold state touched only by the lane's writer thread. Kept out of the
/// atomics so the audio callback does not RMW every sample.
#[repr(align(64))]
struct ScopeWriter {
    accum: f32,
    count: u32,
}

/// Single-writer peak-hold scope lane. Capture fills [`Shared::scope_in`]; the
/// mixer fills [`Shared::scope_out`]. The UI only reads `bins` / `head`.
///
/// Decimation state is plain data behind [`UnsafeCell`]: one producer thread
/// owns each lane, so the hot path stays lock-free and allocation-free. Only
/// publishing a finished bin hits atomics (and `hop`, which the control
/// thread may retune with the stream rate).
pub(super) struct ScopeLane {
    bins: Vec<AtomicU32>,
    head: AtomicU64,
    hop: AtomicU32,
    writer: UnsafeCell<ScopeWriter>,
}

// SAFETY: each lane has a single designated writer thread for `writer` and
// for publishing into `bins`/`head`. The UI only loads those atomics.
unsafe impl Sync for ScopeLane {}

impl ScopeLane {
    pub(super) fn new() -> Self {
        Self {
            bins: (0..SCOPE_LEN).map(|_| AtomicU32::new(0)).collect(),
            head: AtomicU64::new(0),
            // 48 kHz → ~250 ms across SCOPE_LEN until set_rate runs.
            hop: AtomicU32::new(47),
            writer: UnsafeCell::new(ScopeWriter {
                accum: 0.0,
                count: 0,
            }),
        }
    }

    pub(super) fn set_rate(&self, rate: u32) {
        let hop = ((rate as u64).max(1) * 250 / 1000 / SCOPE_LEN as u64).clamp(1, 512) as u32;
        self.hop.store(hop, Ordering::Relaxed);
    }

    pub(super) fn push(&self, sample: f32) {
        let hop = self.hop.load(Ordering::Relaxed).max(1);
        // SAFETY: sole writer for this lane (input callback or mixer).
        let writer = unsafe { &mut *self.writer.get() };
        if sample.abs() >= writer.accum.abs() {
            writer.accum = sample;
        }
        writer.count += 1;
        if writer.count < hop {
            return;
        }
        writer.count = 0;
        let value = std::mem::replace(&mut writer.accum, 0.0);
        let index = self.head.fetch_add(1, Ordering::Relaxed);
        self.bins[(index % SCOPE_LEN as u64) as usize].store(value.to_bits(), Ordering::Relaxed);
    }

    pub(super) fn copy_into(&self, out: &mut [f32]) {
        let len = out.len().min(SCOPE_LEN);
        if len == 0 {
            return;
        }
        let head = self.head.load(Ordering::Relaxed);
        let start = head.wrapping_sub(len as u64);
        for (offset, slot) in out[..len].iter_mut().enumerate() {
            let index = start.wrapping_add(offset as u64) % SCOPE_LEN as u64;
            *slot = f32::from_bits(self.bins[index as usize].load(Ordering::Relaxed));
        }
    }

    pub(super) fn clear(&self) {
        for bin in &self.bins {
            bin.store(0.0f32.to_bits(), Ordering::Relaxed);
        }
        self.head.store(0, Ordering::Relaxed);
        // SAFETY: reset runs before callbacks touch a fresh session.
        let writer = unsafe { &mut *self.writer.get() };
        writer.accum = 0.0;
        writer.count = 0;
    }
}
