//! A free counts too.  Release builds count nothing, so a nonzero count is
//! debug-only; the tests at the end pin the release zeros.

use super::RtGuard;

/// A real heap call, so the optimiser cannot drop it.
fn allocate() -> Vec<u8> {
    Vec::with_capacity(64)
}

#[cfg(debug_assertions)]
#[test]
fn an_allocation_inside_the_guard_is_counted() {
    let guard = RtGuard::enter();
    let buffer = allocate();
    assert!(guard.allocations() >= 1, "the Vec's buffer was allocated");
    drop(buffer);
}

#[test]
fn an_allocation_outside_the_guard_is_not_counted() {
    let _outside = allocate();
    let guard = RtGuard::enter();
    assert_eq!(guard.allocations(), 0);
}

#[cfg(debug_assertions)]
#[test]
fn the_guard_starts_over_with_each_block() {
    let first = RtGuard::enter();
    let _buffer = allocate();
    assert!(first.allocations() >= 1);
    drop(first);

    let second = RtGuard::enter();
    assert_eq!(second.allocations(), 0, "a new block counts from zero");
}

#[cfg(debug_assertions)]
#[test]
fn a_nested_guard_counts_with_the_outer_one() {
    let outer = RtGuard::enter();
    let _buffer = allocate();
    {
        let inner = RtGuard::enter();
        assert_eq!(inner.allocations(), outer.allocations());
        let _more = allocate();
    }
    assert!(
        outer.allocations() >= 2,
        "the inner allocation still counts"
    );
    drop(outer);

    let fresh = RtGuard::enter();
    assert_eq!(fresh.allocations(), 0, "the outermost drop ended the block");
}

#[cfg(debug_assertions)]
#[test]
fn a_free_inside_the_guard_is_counted_apart_from_allocations() {
    let buffer = allocate();
    let guard = RtGuard::enter();
    drop(buffer);
    assert!(guard.frees() >= 1, "the Vec's buffer was freed");
    assert_eq!(guard.allocations(), 0, "a free is not an allocation");
}

#[cfg(debug_assertions)]
#[test]
fn the_callback_marker_is_true_only_inside_a_guard() {
    use super::in_audio_callback;

    assert!(!in_audio_callback(), "no guard on this thread");
    let outer = RtGuard::enter();
    assert!(in_audio_callback());
    {
        let _inner = RtGuard::enter();
        assert!(in_audio_callback());
    }
    assert!(
        in_audio_callback(),
        "the inner drop restored the outer guard's marker"
    );
    drop(outer);
    assert!(
        !in_audio_callback(),
        "the outermost drop cleared the marker"
    );
}

#[test]
fn a_free_outside_the_guard_is_not_counted() {
    let guard = RtGuard::enter();
    drop(guard);
    drop(allocate());
    let fresh = RtGuard::enter();
    assert_eq!(fresh.frees(), 0, "a new block counts frees from zero");
}

#[cfg(debug_assertions)]
#[test]
fn the_outermost_drop_publishes_the_frees() {
    use super::audio_thread_allocations;

    let buffer = allocate();
    let before = audio_thread_allocations();
    let guard = RtGuard::enter();
    drop(buffer);
    let own = guard.frees() as u64;
    assert!(own >= 1);
    drop(guard);
    assert!(
        audio_thread_allocations() - before >= own,
        "the block's frees reached the process-wide count"
    );
}

#[cfg(debug_assertions)]
#[test]
fn the_outermost_drop_publishes_the_count() {
    use super::audio_thread_allocations;

    // Other tests publish too, so compare against at least this block's own.
    let before = audio_thread_allocations();
    let outer = RtGuard::enter();
    let _buffer = allocate();
    {
        let _inner = RtGuard::enter();
        let _more = allocate();
    }
    let own = outer.allocations() as u64;
    assert!(own >= 2);
    drop(outer);
    assert!(
        audio_thread_allocations() - before >= own,
        "the block's allocations reached the process-wide count"
    );
}

#[cfg(not(debug_assertions))]
#[test]
fn a_release_build_counts_no_allocations_and_no_frees() {
    let guard = RtGuard::enter();
    let buffer = allocate();
    drop(buffer);
    assert_eq!(guard.allocations(), 0, "a release build counts nothing");
    assert_eq!(guard.frees(), 0, "a release build counts nothing");
}

#[cfg(not(debug_assertions))]
#[test]
fn enter_and_drop_do_not_panic_in_a_release_build() {
    assert!(
        !super::in_audio_callback(),
        "a release build has no marker outside a guard"
    );
    let outer = RtGuard::enter();
    let _buffer = allocate();
    {
        let _inner = RtGuard::enter();
        let _more = allocate();
    }
    drop(outer);
    assert!(
        !super::in_audio_callback(),
        "a release build has no marker after the outermost drop"
    );
    let fresh = RtGuard::enter();
    assert_eq!(fresh.allocations(), 0);
    assert_eq!(fresh.frees(), 0);
}
