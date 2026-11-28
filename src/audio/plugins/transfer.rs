//! One SPSC parameter queue per plugin and direction.  The VST3 SDK's
//! `ParameterChangeTransfer`, with an overflow slot per parameter that
//! keeps the newest value instead of dropping a change.
//!
//! `push` and `drain` never allocate or lock.

use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

/// Floor for the ring, whatever the parameter count.
const MIN_RING: usize = 64;

/// An empty overflow slot.  A NaN, so no value [`ParamSender::push`]
/// accepts can collide with it.
const EMPTY: u64 = u64::MAX;

/// One queued change.  The producer writes the fields before it publishes
/// `head`, the consumer reads them before it publishes `tail`, so relaxed
/// stores and loads are enough.
struct Entry {
    index: AtomicU32,
    value: AtomicU64,
    offset: AtomicI32,
}

/// A ring counter on its own cache line, so the producer's `head` stores
/// and the consumer's `tail` stores do not bounce one line between cores.
#[repr(align(64))]
struct Counter(AtomicU64);

/// The counters are `u64` and count single entries, so they never wrap in
/// practice: they are compared and subtracted without wrapping arithmetic.
struct Inner {
    entries: Box<[Entry]>,
    /// `entries.len() - 1`; the length is a power of two.
    mask: u64,
    /// Entries pushed, ever.  Only the producer writes it.
    head: Counter,
    /// Entries drained, ever.  Only the consumer writes it.
    tail: Counter,
    /// The newest overflow value per parameter, as `f64` bits, or
    /// [`EMPTY`].  A non-empty slot is what makes a parameter pending.
    values: Box<[AtomicU64]>,
    /// One bit per parameter: set after every overflow store, so the
    /// consumer only visits the slots that may hold a value.
    pending: Box<[AtomicU64]>,
    /// Per parameter, `head` when the producer last found the slot empty
    /// and filled it: every ring entry of that parameter lies below it.  The
    /// consumer takes the slot only once its own `tail` has reached it.  A
    /// store over a slot the producer saw full leaves the mark alone, even
    /// when the consumer emptied the slot just after that look; the mark
    /// still holds, since no ring entry of the parameter was pushed since.
    behind: Box<[AtomicU64]>,
    /// Changes pushed, ever (ring and overflow); stored after the change
    /// itself.  Only the producer writes it.
    pushed: Counter,
    /// The `pushed` count up to which the consumer reports every change
    /// applied (see [`ParamReceiver::mark_applied`]).  Only the consumer
    /// writes it.
    applied: Counter,
}

impl Inner {
    /// True while the ring holds an entry not yet drained.
    fn ring_queued(&self) -> bool {
        self.head.0.load(Ordering::Acquire) != self.tail.0.load(Ordering::Acquire)
    }
}

pub(crate) struct ParamSender {
    inner: Arc<Inner>,
}

pub(crate) struct ParamReceiver {
    inner: Arc<Inner>,
}

/// Indices `0..params`.  The ring is the next power of two of `2 * params`
/// entries, at least 64, and every byte of it is allocated here.
pub(crate) fn param_transfer(params: usize) -> (ParamSender, ParamReceiver) {
    assert!(
        u32::try_from(params).is_ok(),
        "parameter count {params} does not fit an entry"
    );
    let capacity = params.saturating_mul(2).max(MIN_RING).next_power_of_two();
    let inner = Arc::new(Inner {
        entries: (0..capacity)
            .map(|_| Entry {
                index: AtomicU32::new(0),
                value: AtomicU64::new(0),
                offset: AtomicI32::new(0),
            })
            .collect(),
        mask: capacity as u64 - 1,
        head: Counter(AtomicU64::new(0)),
        tail: Counter(AtomicU64::new(0)),
        values: (0..params).map(|_| AtomicU64::new(EMPTY)).collect(),
        pending: (0..params.div_ceil(64))
            .map(|_| AtomicU64::new(0))
            .collect(),
        behind: (0..params).map(|_| AtomicU64::new(0)).collect(),
        pushed: Counter(AtomicU64::new(0)),
        applied: Counter(AtomicU64::new(0)),
    });
    (
        ParamSender {
            inner: Arc::clone(&inner),
        },
        ParamReceiver { inner },
    )
}

impl ParamSender {
    pub(crate) fn params(&self) -> usize {
        self.inner.values.len()
    }

    /// False, and nothing queued, when `index` is out of range or `value`
    /// is not finite.  A negative offset is stored as 0.
    ///
    /// A full ring, or a parameter that already has an overflow value,
    /// keeps only the newest in that slot, so the parameter stays in
    /// order even after the ring has room.  [`ParamReceiver::drain`] does
    /// not reorder: offsets of one parameter must stay non-decreasing in
    /// a block (VST3 `ParameterValueQueue`).
    pub(crate) fn push(&mut self, index: usize, value: f64, offset: i32) -> bool {
        if index >= self.params() || !value.is_finite() {
            return false;
        }
        let inner = &*self.inner;
        let slot = &inner.values[index];
        let head = inner.head.0.load(Ordering::Relaxed);
        let tail = inner.tail.0.load(Ordering::Acquire);
        // Acquire: seeing the consumer empty the slot also means its ring
        // drain is over, so an entry pushed now waits for the next drain.
        let empty = slot.load(Ordering::Acquire) == EMPTY;
        if !empty || head - tail > inner.mask {
            // While the slot is full no ring entry of this parameter is
            // pushed, so the mark from when it filled still holds.
            if empty {
                inner.behind[index].store(head, Ordering::Relaxed);
            }
            slot.store(value.to_bits(), Ordering::Release);
            inner.pending[index / 64].fetch_or(1 << (index % 64), Ordering::Release);
            inner.pushed.0.fetch_add(1, Ordering::Release);
            return true;
        }
        let entry = &inner.entries[(head & inner.mask) as usize];
        entry.index.store(index as u32, Ordering::Relaxed);
        entry.value.store(value.to_bits(), Ordering::Relaxed);
        entry.offset.store(offset.max(0), Ordering::Relaxed);
        inner.head.0.store(head + 1, Ordering::Release);
        inner.pushed.0.fetch_add(1, Ordering::Release);
        true
    }

    pub(crate) fn pushed(&self) -> u64 {
        self.inner.pushed.0.load(Ordering::Relaxed)
    }

    /// Drained and accepted by the plugin, not merely queued.
    pub(crate) fn applied(&self) -> u64 {
        self.inner.applied.0.load(Ordering::Acquire)
    }

    /// False means drained into a block, not that the plugin applied it.
    /// Scans every overflow slot: a drain clears a word's bits before
    /// putting back the ones it defers, so the bits can read zero while a
    /// value still waits.
    pub(crate) fn has_queued(&self) -> bool {
        let inner = &*self.inner;
        inner.ring_queued()
            || inner
                .values
                .iter()
                .any(|slot| slot.load(Ordering::Acquire) != EMPTY)
    }
}

impl ParamReceiver {
    pub(crate) fn params(&self) -> usize {
        self.inner.values.len()
    }

    /// Read it before a [`Self::drain`]: every change it counts is visible
    /// to that drain.
    pub(crate) fn pushed(&self) -> u64 {
        self.inner.pushed.0.load(Ordering::Acquire)
    }

    /// A drain after this count left nothing deferred, and the plugin
    /// accepted that block.  Never moves back.
    pub(crate) fn mark_applied(&self, pushed: u64) {
        self.inner.applied.0.fetch_max(pushed, Ordering::Release);
    }

    pub(crate) fn capacity(&self) -> usize {
        self.inner.entries.len()
    }

    /// Ring first, in push order, then each overflow value at the highest
    /// offset the ring part emitted (0 if none).  VST3
    /// `ParameterValueQueue` needs that order; a point at the same offset
    /// replaces the one before it.
    ///
    /// Each parameter stays in push order.  Overflow keeps only the newest
    /// per parameter and does not order it against other parameters.  A
    /// value stored during this drain arrives now or later, never twice.
    ///
    /// True when an overflow value was behind ring entries pushed after
    /// this drain's `head` snapshot.  That value waits for the next drain.
    pub(crate) fn drain(&mut self, mut emit: impl FnMut(usize, f64, i32)) -> bool {
        let inner = &*self.inner;
        let mut deferred = false;
        let head = inner.head.0.load(Ordering::Acquire);
        let mut tail = inner.tail.0.load(Ordering::Relaxed);
        let mut last_offset = 0;
        while tail != head {
            let entry = &inner.entries[(tail & inner.mask) as usize];
            let offset = entry.offset.load(Ordering::Relaxed);
            emit(
                entry.index.load(Ordering::Relaxed) as usize,
                f64::from_bits(entry.value.load(Ordering::Relaxed)),
                offset,
            );
            last_offset = last_offset.max(offset);
            tail += 1;
        }
        inner.tail.0.store(tail, Ordering::Release);

        for (word_index, word) in inner.pending.iter().enumerate() {
            // A stale zero only defers the word to the next drain: its bit
            // stays set.
            if word.load(Ordering::Relaxed) == 0 {
                continue;
            }
            let mut bits = word.swap(0, Ordering::AcqRel);
            while bits != 0 {
                let bit = bits & bits.wrapping_neg();
                bits &= bits - 1;
                let index = word_index * 64 + bit.trailing_zeros() as usize;
                let slot = &inner.values[index];
                // Empty when an earlier drain already took the value this
                // bit was set for.
                if slot.load(Ordering::Acquire) == EMPTY {
                    continue;
                }
                // Filled after this drain read `head`: ring entries of this
                // parameter may still be queued ahead of it, so it waits
                // for the next drain.
                if inner.behind[index].load(Ordering::Relaxed) > tail {
                    word.fetch_or(bit, Ordering::Relaxed);
                    deferred = true;
                    continue;
                }
                // Only the consumer empties a slot, so this takes a value:
                // the one just seen or a newer one stored over it.
                let value = slot.swap(EMPTY, Ordering::AcqRel);
                emit(index, f64::from_bits(value), last_offset);
            }
        }
        deferred
    }

    /// A [`Self::drain`] that emits nothing.  A slot filled after this
    /// drain's `head` snapshot is kept, so a parameter loses only a prefix.
    // Only the tests call it: a state is loaded only while its plugin
    // loads, before any value is queued for it.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn discard(&mut self) {
        self.drain(|_, _, _| {});
    }

    /// True when a drain would emit nothing, as far as this thread can see:
    /// a change the producer is pushing right now may not show yet.  An
    /// overflow value deferred behind ring entries counts as queued.
    ///
    /// Only the pending bits' slots are visited: the bits never read short
    /// here, since this thread is the one that drains.
    pub(crate) fn is_empty(&self) -> bool {
        let inner = &*self.inner;
        if inner.ring_queued() {
            return false;
        }
        !inner.pending.iter().enumerate().any(|(word_index, word)| {
            let mut bits = word.load(Ordering::Acquire);
            while bits != 0 {
                let index = word_index * 64 + bits.trailing_zeros() as usize;
                if inner.values[index].load(Ordering::Acquire) != EMPTY {
                    return true;
                }
                bits &= bits - 1;
            }
            false
        })
    }
}

/// Values a consumer drained but the plugin did not take (a refused or
/// failed block, an event list that was full): the newest per parameter,
/// kept for the next block.  Allocated for the parameter count at load;
/// putting and taking never allocate, so the audio thread owns it.
pub(crate) struct Retry {
    values: Box<[f64]>,
    /// One bit per parameter with a value kept.
    pending: Box<[u64]>,
}

impl Retry {
    pub(crate) fn new(params: usize) -> Self {
        Self {
            values: vec![0.0; params].into(),
            pending: vec![0; params.div_ceil(64)].into(),
        }
    }

    pub(crate) fn put(&mut self, index: usize, value: f64) {
        if let Some(slot) = self.values.get_mut(index) {
            *slot = value;
            self.pending[index / 64] |= 1 << (index % 64);
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.pending.iter().all(|&word| word == 0)
    }

    /// Stays kept only where `offer` returns true.
    pub(crate) fn retain(&mut self, mut offer: impl FnMut(usize, f64) -> bool) {
        for (word_index, word) in self.pending.iter_mut().enumerate() {
            let mut bits = *word;
            while bits != 0 {
                let bit = bits & bits.wrapping_neg();
                bits &= bits - 1;
                let index = word_index * 64 + bit.trailing_zeros() as usize;
                if !offer(index, self.values[index]) {
                    *word &= !bit;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
