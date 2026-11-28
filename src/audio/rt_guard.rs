//! Counts allocations and frees under an [`RtGuard`], and never panics: a
//! hosted plugin may allocate inside `process`.
//!
//! The counters are read inside the allocator, so they must not allocate.
//! Every thread-local is a `const` [`Cell`] with no destructor, and the
//! allocator uses `try_with`.  The outermost drop publishes the block's
//! count; that add is not in the allocator.  Release builds compile the
//! count away.  [`in_audio_callback`] reads the guard depth, so it does not
//! depend on drop order.

#[cfg(debug_assertions)]
use std::alloc::{GlobalAlloc, Layout, System};
#[cfg(debug_assertions)]
use std::cell::Cell;
#[cfg(debug_assertions)]
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(debug_assertions)]
static PUBLISHED: AtomicU64 = AtomicU64::new(0);

/// Heap calls under any outermost guard since launch.  Debug builds only.
#[cfg(debug_assertions)]
pub(crate) fn audio_thread_allocations() -> u64 {
    PUBLISHED.load(Ordering::Relaxed)
}

#[cfg(debug_assertions)]
thread_local! {
    /// Only the outermost guard counts.
    static DEPTH: Cell<u32> = const { Cell::new(0) };
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
    static FREES: Cell<usize> = const { Cell::new(0) };
}

#[cfg(debug_assertions)]
pub(crate) struct GuardAllocator;

/// No allocation, and nothing on a thread whose locals are gone.
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

/// Nested guards share the outermost count.
pub(crate) struct RtGuard {
    #[cfg(debug_assertions)]
    _no_send: std::marker::PhantomData<*const ()>,
}

impl RtGuard {
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

    /// The callback does not act on the count.  The real-time tests read it.
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

/// True while any guard is held, independent of drop order.  A release
/// build has no guard state and says `false`.
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
    /// Empty in a release build, but always here: tests end a guard with
    /// `drop`, and that is a destructor call in both builds.
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
