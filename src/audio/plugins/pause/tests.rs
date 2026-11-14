use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{BlockAction, PauseHandshake};

/// A handshake past its first block: processing.
fn running() -> PauseHandshake {
    let pause = PauseHandshake::new();
    assert_eq!(pause.begin_block(), BlockAction::Start);
    pause
}

#[test]
fn a_new_handshake_starts_processing_on_its_first_block() {
    let pause = PauseHandshake::new();
    assert!(!pause.is_paused());
    assert_eq!(pause.begin_block(), BlockAction::Start);
    assert_eq!(pause.begin_block(), BlockAction::Process);
    assert_eq!(pause.paused_blocks(), 0);
}

#[test]
fn a_request_before_the_first_block_pauses_without_a_stop_call() {
    let pause = PauseHandshake::new();
    pause.request();
    assert!(!pause.is_paused());
    assert_eq!(pause.begin_block(), BlockAction::Skip);
    assert!(pause.is_paused());
    pause.resume();
    assert_eq!(pause.begin_block(), BlockAction::Start);
}

#[test]
fn the_full_sequence_stops_and_restarts_once() {
    let pause = running();
    assert_eq!(pause.begin_block(), BlockAction::Process);
    pause.request();
    assert!(!pause.is_paused());
    assert_eq!(pause.begin_block(), BlockAction::Stop);
    // Not paused until the stop call is done.
    assert!(!pause.is_paused());
    pause.stopped();
    assert!(pause.is_paused());
    assert_eq!(pause.begin_block(), BlockAction::Skip);
    assert_eq!(pause.begin_block(), BlockAction::Skip);
    pause.resume();
    assert!(!pause.is_paused());
    assert_eq!(pause.begin_block(), BlockAction::Start);
    assert_eq!(pause.begin_block(), BlockAction::Process);
    assert_eq!(pause.paused_blocks(), 3);
}

#[test]
fn request_is_idempotent() {
    let pause = running();
    pause.request();
    pause.request();
    assert_eq!(pause.begin_block(), BlockAction::Stop);
    pause.request();
    pause.stopped();
    pause.request();
    assert!(pause.is_paused());
    assert_eq!(pause.begin_block(), BlockAction::Skip);
}

#[test]
fn resume_before_the_stop_cancels_the_request() {
    let pause = running();
    pause.request();
    pause.resume();
    assert_eq!(pause.begin_block(), BlockAction::Process);
    assert_eq!(pause.paused_blocks(), 0);
}

#[test]
fn resume_during_the_stop_call_starts_on_the_next_block() {
    let pause = running();
    pause.request();
    assert_eq!(pause.begin_block(), BlockAction::Stop);
    pause.resume();
    pause.stopped();
    assert!(!pause.is_paused());
    assert_eq!(pause.begin_block(), BlockAction::Start);
    assert_eq!(pause.begin_block(), BlockAction::Process);
}

#[test]
fn request_again_during_the_stop_call_ends_paused() {
    let pause = running();
    pause.request();
    assert_eq!(pause.begin_block(), BlockAction::Stop);
    pause.resume();
    pause.request();
    pause.stopped();
    assert!(pause.is_paused());
    assert_eq!(pause.begin_block(), BlockAction::Skip);
    pause.resume();
    assert_eq!(pause.begin_block(), BlockAction::Start);
}

#[test]
fn request_before_the_restart_cancels_the_resume() {
    let pause = running();
    pause.request();
    assert_eq!(pause.begin_block(), BlockAction::Stop);
    pause.stopped();
    pause.resume();
    pause.request();
    // Still stopped: the audio side confirms without a second stop call.
    assert!(!pause.is_paused());
    assert_eq!(pause.begin_block(), BlockAction::Skip);
    assert!(pause.is_paused());
    pause.resume();
    assert_eq!(pause.begin_block(), BlockAction::Start);
}

#[test]
fn without_a_bridge_the_ui_stops_only_a_processing_plugin() {
    // Processing (also with a request pending): the UI must stop it.
    let pause = running();
    assert!(pause.stop_without_audio());
    assert!(pause.is_paused());
    let pause = running();
    pause.request();
    assert!(pause.stop_without_audio());
    // Already stopped: paused, not yet restarted, or never started.
    assert!(!pause.stop_without_audio());
    pause.resume();
    assert!(!pause.stop_without_audio());
    assert!(!PauseHandshake::new().stop_without_audio());
    // The way out is `resume`: a later bridge's first block starts it.
    pause.resume();
    assert!(!pause.is_paused());
    assert_eq!(pause.begin_block(), BlockAction::Start);
    assert_eq!(pause.paused_blocks(), 0);
}

#[test]
fn the_audio_thread_restarts_after_the_ui_work() {
    // On x86 (TSO) this is a functional check of the handshake, not a
    // proof of its ordering: the hardware orders these accesses anyway.
    let pause = Arc::new(running());
    // Plain data the UI changes while paused; relaxed, so only the
    // handshake orders it.
    let work = Arc::new(AtomicU64::new(0));
    let audio = {
        let pause = Arc::clone(&pause);
        let work = Arc::clone(&work);
        std::thread::spawn(move || {
            let started = Instant::now();
            loop {
                assert!(started.elapsed() < Duration::from_secs(10), "no restart");
                match pause.begin_block() {
                    BlockAction::Stop => pause.stopped(),
                    BlockAction::Start => return work.load(Ordering::Relaxed),
                    BlockAction::Process | BlockAction::Skip => std::thread::yield_now(),
                }
            }
        })
    };
    pause.request();
    let started = Instant::now();
    while !pause.is_paused() {
        assert!(started.elapsed() < Duration::from_secs(10), "never paused");
        std::thread::yield_now();
    }
    work.store(42, Ordering::Relaxed);
    pause.resume();
    assert_eq!(audio.join().unwrap(), 42);
    assert!(pause.paused_blocks() >= 1);
}
