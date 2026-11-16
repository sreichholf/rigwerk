//! Parameter changes between the UI thread and the audio thread, one
//! queue per plugin and direction (the VST3 SDK's `ParameterChangeTransfer`
//! with an overflow that never drops a change).
//!
//! UI to audio carries panel edits, editor `performEdit` and resets; audio
//! to UI carries VST3 `outputParameterChanges` and CLAP output events.
//! Exactly one thread pushes and one drains: [`param_transfer`] hands out
//! one [`ParamSender`] and one [`ParamReceiver`], neither is `Clone`, and
//! both take `&mut self`.  Everything is allocated there; `push` and `drain`
//! never allocate or lock, so either side may be the audio thread.

use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

/// The smallest ring, in entries, whatever the parameter count.
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

/// The pushing side of a [`param_transfer`].
pub(crate) struct ParamSender {
    inner: Arc<Inner>,
}

/// The draining side of a [`param_transfer`].
pub(crate) struct ParamReceiver {
    inner: Arc<Inner>,
}

/// A queue for `params` parameters (indices `0..params`).  The ring holds
/// the next power of two of `2 * params` entries, at least 64; behind it is
/// one overflow slot per parameter.  All of it is allocated here.
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
    /// Parameters this queue was made for.
    pub(crate) fn params(&self) -> usize {
        self.inner.values.len()
    }

    /// Queues `value` for parameter `index` at sample `offset` in the
    /// consumer's next block (a negative offset is stored as 0).  `false`,
    /// and nothing queued, when `index` is out of range or `value` is not
    /// finite.
    ///
    /// A full ring does not drop the change: it goes to the parameter's
    /// overflow slot, which keeps only the newest value.  Once a parameter
    /// has a pending overflow value, its later changes go there too, even
    /// when the ring has room again, until the consumer has taken it.  That
    /// keeps each parameter in order.  A ring entry is only pushed while the
    /// slot is empty, so it is newer than every overflow value already
    /// taken and older than every one stored after it.  The consumer drains
    /// the ring before the overflow, and when a slot fills it records
    /// `head`, so a value stored while the consumer is mid-drain waits until
    /// the ring entries pushed before it have been delivered.
    ///
    /// A VST3 `ParameterValueQueue` needs each parameter's points in offset
    /// order.  Keeping the offsets of one parameter non-decreasing within a
    /// block is the producer's job: [`ParamReceiver::drain`] does not
    /// reorder.
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

    /// Changes pushed so far.
    pub(crate) fn pushed(&self) -> u64 {
        self.inner.pushed.0.load(Ordering::Relaxed)
    }

    /// The push count up to which the consumer has reported every change
    /// applied: drained and accepted by the plugin.
    pub(crate) fn applied(&self) -> u64 {
        self.inner.applied.0.load(Ordering::Acquire)
    }

    /// True until the consumer has drained every change pushed so far.
    /// False means drained, handed to a block; not that the plugin has
    /// applied it.  No allocation, no lock.
    ///
    /// This scans every overflow slot (O(params)), not the pending bits: a
    /// drain clears a word's bits before it puts back the ones it defers,
    /// so from this thread the bits can read zero while a value still
    /// waits.
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
    /// Parameters this queue was made for.
    pub(crate) fn params(&self) -> usize {
        self.inner.values.len()
    }

    /// Changes pushed so far.  Read it before a [`Self::drain`]: every
    /// change it counts is visible to that drain.
    pub(crate) fn pushed(&self) -> u64 {
        self.inner.pushed.0.load(Ordering::Acquire)
    }

    /// Reports every change up to push count `pushed` applied: a drain
    /// taken after reading it left nothing deferred, and the plugin
    /// accepted the block it went into.  Never moves back.
    pub(crate) fn mark_applied(&self, pushed: u64) {
        self.inner.applied.0.fetch_max(pushed, Ordering::Release);
    }

    /// Entries the ring holds before changes go to the overflow.
    pub(crate) fn capacity(&self) -> usize {
        self.inner.entries.len()
    }

    /// Hands every queued change to `emit` as `(index, value, offset)`:
    /// first the ring, in push order, then each pending overflow value at
    /// the highest offset the ring part emitted (0 if it emitted none).
    /// That keeps the points of one block in offset order, which a VST3
    /// `ParameterValueQueue` needs; a point at the same offset replaces the
    /// one before it.
    ///
    /// No change is lost, and each parameter's changes arrive in the order
    /// they were pushed.  Overflow values do not keep their order relative
    /// to other parameters' changes, and of several overflow values for one
    /// parameter only the newest arrives.  A value the producer stores while
    /// this runs arrives now or in a later drain, never twice: the slot is
    /// emptied with a swap.
    ///
    /// Returns whether it deferred an overflow value behind ring entries
    /// pushed after its `head` snapshot (that value arrives with a later
    /// drain).
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

    /// Drops what is queued, as after a state load (the restored state
    /// replaces any change still in flight).  It is a [`Self::drain`] that
    /// emits nothing: one `head` snapshot, the ring up to it and every
    /// overflow value whose ring entries all lie below it are dropped.  A
    /// slot the producer fills after the snapshot is kept, with the ring
    /// entries ahead of it, so each parameter loses only its oldest changes
    /// and keeps its newest whenever it keeps any.
    // For a VST3 state load (#28); only the tests call it yet.
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

    /// Keeps `value` for parameter `index`, replacing an older one.
    pub(crate) fn put(&mut self, index: usize, value: f64) {
        if let Some(slot) = self.values.get_mut(index) {
            *slot = value;
            self.pending[index / 64] |= 1 << (index % 64);
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.pending.iter().all(|&word| word == 0)
    }

    /// Hands every kept value to `offer` as `(index, value)`; it stays
    /// kept only where `offer` returns true.
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
