//! Debug-only allocation guard for the audio callbacks.
//!
//! An input callback, an output callback, an ASIO `bufferSwitch` and a
//! PipeWire process callback must not allocate RigWerk's own memory (see
//! `AGENTS.md`, "Audio thread"), and must not free it either: a drop of a
//! model or a buffer there is the same heap call.  This module makes a
//! violation visible: with `debug_assertions` on, the crate's global
//! allocator counts the allocations (a `realloc` is one) and, separately,
//! the frees made on a thread while an [`RtGuard`] is held, and a test
//! asserts both are zero after a block.
//!
//! It counts and never panics, deliberately: a hosted VST3 or CLAP plugin
//! may allocate or free inside its own `process`, and aborting the callback
//! over a third party's heap call would be worse than the call.  The tests
//! are where the zero is enforced, on the chains RigWerk builds itself.
//! In a live run the outermost guard's drop adds the block's allocations
//! and frees to a process-wide atomic, [`audio_thread_allocations`], which
//! the Diagnostics sheet shows.  That add is one atomic, made only when the
//! block did make a heap call, and it lives in `Drop`, never in the
//! allocator.
//!
//! The guard is a thread-local flag, so it costs one TLS read per heap call
//! in a debug build.  Release builds compile the flag away:
//! [`RtGuard::enter`] returns a value that does nothing,
//! [`RtGuard::allocations`] and [`RtGuard::frees`] are 0, and
//! [`audio_thread_allocations`] does not exist.
//!
//! The same guard marks its thread as inside an audio callback for as long
//! as it is held, and [`in_audio_callback`] reads that marker from the guard
//! depth, so it does not depend on drop order.  A `Drop` that would take the
//! deferred-slot lock or destroy a plugin asserts against it in a debug build
//! (see `plugins::chain`), so the invariant those drops depend on is checked
//! and not only documented.
//!
//! The flags are read inside `GlobalAlloc::alloc` and `dealloc`, so they
//! must not allocate themselves: every thread-local is a `const`-initialised
//! [`Cell`] with no destructor, and every access from the allocator goes
//! through `try_with`, which is a plain load once the thread has them.

#[cfg(debug_assertions)]
use std::alloc::{GlobalAlloc, Layout, System};
#[cfg(debug_assertions)]
use std::cell::Cell;
#[cfg(debug_assertions)]
use std::sync::atomic::{AtomicU64, Ordering};

/// Allocations and frees made under any thread's outermost guard since
/// launch.
#[cfg(debug_assertions)]
static PUBLISHED: AtomicU64 = AtomicU64::new(0);

/// Heap calls the audio callbacks made since launch: allocations,
/// reallocations and frees, summed over every block that ended.  Debug
/// builds only.  A plain load: the UI reads it once a frame.
#[cfg(debug_assertions)]
pub(crate) fn audio_thread_allocations() -> u64 {
    PUBLISHED.load(Ordering::Relaxed)
}

#[cfg(debug_assertions)]
thread_local! {
    /// How many guards this thread holds.  Only the outermost one counts.
    static DEPTH: Cell<u32> = const { Cell::new(0) };
    /// Allocations this thread made while the outermost guard was held.
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
    /// Frees this thread made while the outermost guard was held.
    static FREES: Cell<usize> = const { Cell::new(0) };
}

/// The crate's global allocator in a debug build: counts what the audio
/// thread allocates and frees while a [`RtGuard`] is held, and hands every
/// call to the system allocator.
#[cfg(debug_assertions)]
pub(crate) struct GuardAllocator;

/// Adds one to `counter` when this thread holds a guard.  No allocation,
/// and nothing on a thread whose locals are gone.
#[cfg(debug_assertions)]
fn note(counter: &'static std::thread::LocalKey<Cell<usize>>) {
    let inside = DEPTH.try_with(|depth| depth.get() > 0).unwrap_or(false);
    if inside {
        let _ = counter.try_with(|count| count.set(count.get().saturating_add(1)));
    }
}

#[cfg(debug_assertions)]
unsafe impl GlobalAlloc for GuardAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note(&ALLOCATIONS);
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note(&ALLOCATIONS);
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        note(&FREES);
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        note(&ALLOCATIONS);
        unsafe { System.realloc(pointer, layout, size) }
    }
}

/// Held across one audio callback's work.  Nested guards are allowed; the
/// count covers the outermost one.
pub(crate) struct RtGuard {
    #[cfg(debug_assertions)]
    _no_send: std::marker::PhantomData<*const ()>,
}

impl RtGuard {
    /// Starts counting this thread's allocations and frees, and marks the
    /// thread as inside a callback.  Cheap: a few TLS writes.
    #[must_use]
    pub(crate) fn enter() -> Self {
        #[cfg(debug_assertions)]
        DEPTH.with(|depth| {
            if depth.get() == 0 {
                ALLOCATIONS.with(|count| count.set(0));
                FREES.with(|count| count.set(0));
            }
            depth.set(depth.get().saturating_add(1));
        });
        Self {
            #[cfg(debug_assertions)]
            _no_send: std::marker::PhantomData,
        }
    }

    /// Allocations this thread made while the outermost guard was held.
    /// Only the real-time tests read the count: the callback itself does not
    /// act on it.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn allocations(&self) -> usize {
        #[cfg(debug_assertions)]
        {
            ALLOCATIONS.with(Cell::get)
        }
        #[cfg(not(debug_assertions))]
        {
            0
        }
    }

    /// Frees this thread made while the outermost guard was held: a model
    /// or a buffer dropped on the audio thread.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn frees(&self) -> usize {
        #[cfg(debug_assertions)]
        {
            FREES.with(Cell::get)
        }
        #[cfg(not(debug_assertions))]
        {
            0
        }
    }
}

/// Whether this thread is inside an audio callback: it holds an [`RtGuard`].
/// True while any guard is held, independent of drop order, because it reads
/// the guard depth.  A release build has no guard state and says `false`.
pub(crate) fn in_audio_callback() -> bool {
    #[cfg(debug_assertions)]
    {
        DEPTH.try_with(|depth| depth.get() > 0).unwrap_or(false)
    }
    #[cfg(not(debug_assertions))]
    {
        false
    }
}

impl Drop for RtGuard {
    /// The outermost drop publishes the block's counts, if any.  Empty in a
    /// release build, but always there: the tests end a guard with `drop`,
    /// and that is a call on a type with a destructor in both builds.
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        self.publish();
    }
}

#[cfg(debug_assertions)]
impl RtGuard {
    fn publish(&self) {
        let outermost = DEPTH.with(|depth| {
            let left = depth.get().saturating_sub(1);
            depth.set(left);
            left == 0
        });
        if outermost {
            let count = ALLOCATIONS
                .with(Cell::get)
                .saturating_add(FREES.with(Cell::get));
            if count > 0 {
                PUBLISHED.fetch_add(count as u64, Ordering::Relaxed);
            }
        }
    }
}

#[cfg(test)]
mod tests;
