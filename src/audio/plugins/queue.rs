//! A fixed-capacity single-producer single-consumer queue of owned values:
//! the chain commands the UI sends the audio side, and the slots the audio
//! side hands back to be destroyed (see `chain`).
//!
//! [`queue`] allocates everything and hands out one [`Producer`] and one
//! [`Consumer`].  Neither is `Clone`, and both take `&mut self` to move a
//! value, so each end has one owner at a time; an end may move between
//! threads with that owner.  `push`, `peek` and `pop` never allocate or
//! lock, so either end may be on the audio thread.
//!
//! Values still queued when the last end drops are passed to the queue's
//! `release` function, not dropped in place: a plugin slot must not be
//! destroyed on whichever thread happens to let go last.

use std::cell::{Cell, UnsafeCell};
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// A queue counter on its own cache line, so the producer's `head` stores
/// and the consumer's `tail` stores do not bounce one line between cores.
#[repr(align(64))]
struct Counter(AtomicU64);

/// The counters count single values and are `u64`, so they never wrap in
/// practice: they are compared and subtracted without wrapping arithmetic.
struct Inner<T> {
    /// A cell below `head` and at or above `tail` holds a value; every
    /// other cell is uninitialised.  The length is a power of two.
    cells: Box<[UnsafeCell<MaybeUninit<T>>]>,
    mask: u64,
    /// Values pushed, ever.  Only the producer writes it.
    head: Counter,
    /// Values popped, ever.  Only the consumer writes it.
    tail: Counter,
    release: fn(T),
}

// A cell is written only by the producer, before it publishes `head`, and
// read only by the consumer, before it publishes `tail` (Release / Acquire
// on both), so no cell is ever touched by both ends at once.
unsafe impl<T: Send> Send for Inner<T> {}
unsafe impl<T: Send> Sync for Inner<T> {}

impl<T> Drop for Inner<T> {
    fn drop(&mut self) {
        let head = *self.head.0.get_mut();
        let mut tail = *self.tail.0.get_mut();
        while tail < head {
            let cell = self.cells[(tail & self.mask) as usize].get_mut();
            // Between `tail` and `head`: initialised, and read once.
            let value = unsafe { cell.assume_init_read() };
            (self.release)(value);
            tail += 1;
        }
    }
}

/// The pushing end of a [`queue`].
pub(crate) struct Producer<T> {
    inner: Arc<Inner<T>>,
}

/// The popping end of a [`queue`].  Not `Sync`: [`Self::peek`] hands out
/// `&T`, which two threads sharing one consumer must not both hold.
pub(crate) struct Consumer<T> {
    inner: Arc<Inner<T>>,
    _not_sync: PhantomData<Cell<()>>,
}

/// A queue for at least `capacity` values (rounded up to a power of two,
/// at least one).  Values left in it when both ends are gone go to
/// `release`.
pub(crate) fn queue<T: Send>(capacity: usize, release: fn(T)) -> (Producer<T>, Consumer<T>) {
    let capacity = capacity.max(1).next_power_of_two();
    let inner = Arc::new(Inner {
        cells: (0..capacity)
            .map(|_| UnsafeCell::new(MaybeUninit::uninit()))
            .collect(),
        mask: capacity as u64 - 1,
        head: Counter(AtomicU64::new(0)),
        tail: Counter(AtomicU64::new(0)),
        release,
    });
    (
        Producer {
            inner: Arc::clone(&inner),
        },
        Consumer {
            inner,
            _not_sync: PhantomData,
        },
    )
}

impl<T> Producer<T> {
    /// Appends `value`, or hands it back when the queue is full.
    pub(crate) fn push(&mut self, value: T) -> Result<(), T> {
        let inner = &*self.inner;
        let head = inner.head.0.load(Ordering::Relaxed);
        let tail = inner.tail.0.load(Ordering::Acquire);
        if head - tail >= inner.cells.len() as u64 {
            return Err(value);
        }
        // Not between `tail` and `head`: the consumer does not touch it.
        unsafe { (*inner.cells[(head & inner.mask) as usize].get()).write(value) };
        inner.head.0.store(head + 1, Ordering::Release);
        Ok(())
    }

    /// How many more values fit right now (the consumer may free more).
    pub(crate) fn room(&self) -> usize {
        let inner = &*self.inner;
        let head = inner.head.0.load(Ordering::Relaxed);
        let tail = inner.tail.0.load(Ordering::Acquire);
        inner.cells.len() - (head - tail) as usize
    }

    /// Whether every pushed value was popped.
    pub(crate) fn is_empty(&self) -> bool {
        let inner = &*self.inner;
        inner.head.0.load(Ordering::Relaxed) == inner.tail.0.load(Ordering::Acquire)
    }
}

impl<T> Consumer<T> {
    /// The oldest value, left in the queue.
    pub(crate) fn peek(&self) -> Option<&T> {
        let inner = &*self.inner;
        let tail = inner.tail.0.load(Ordering::Relaxed);
        let head = inner.head.0.load(Ordering::Acquire);
        if tail == head {
            return None;
        }
        // Published by the producer and not popped: initialised, and the
        // producer does not write it again until `tail` passes it, which
        // needs `&mut self`.
        Some(unsafe { (*inner.cells[(tail & inner.mask) as usize].get()).assume_init_ref() })
    }

    /// Takes the oldest value.
    pub(crate) fn pop(&mut self) -> Option<T> {
        let inner = &*self.inner;
        let tail = inner.tail.0.load(Ordering::Relaxed);
        let head = inner.head.0.load(Ordering::Acquire);
        if tail == head {
            return None;
        }
        // As in `peek`; read once, then the cell is handed back.
        let value =
            unsafe { (*inner.cells[(tail & inner.mask) as usize].get()).assume_init_read() };
        inner.tail.0.store(tail + 1, Ordering::Release);
        Some(value)
    }
}

#[cfg(test)]
mod tests;
