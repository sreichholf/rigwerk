use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{drive_pause, BlockAction, PauseHandshake, PauseStep, PAUSE_STALL, PAUSE_STRANDED};

fn block(pause: &PauseHandshake) -> BlockAction {
    pause.begin_block().action()
}

fn running() -> PauseHandshake {
    let pause = PauseHandshake::new();
    let first = pause.begin_block();
    assert_eq!(first.action(), BlockAction::Start);
    pause.started();
    drop(first);
    assert!(pause.processing());
    pause
}

fn stopped() -> PauseHandshake {
    let pause = running();
    pause.request();
    let stop = pause.begin_block();
    assert_eq!(stop.action(), BlockAction::Stop);
    pause.stopped();
    drop(stop);
    pause
}

#[test]
fn a_new_handshake_starts_processing_on_its_first_block() {
    let pause = PauseHandshake::new();
    assert!(!pause.is_paused());
    assert!(!pause.processing());
    assert_eq!(pause.served(), 0);
    let first = pause.begin_block();
    assert_eq!(first.action(), BlockAction::Start);
    pause.started();
    drop(first);
    assert!(pause.processing());
    assert_eq!(block(&pause), BlockAction::Process);
    assert_eq!(pause.paused_blocks(), 0);
    assert_eq!(pause.served(), 2);
}

#[test]
fn a_request_before_the_first_block_pauses_without_a_stop_call() {
    let pause = PauseHandshake::new();
    pause.request();
    assert!(!pause.is_paused());
    assert_eq!(block(&pause), BlockAction::Skip);
    assert!(pause.quiet());
    assert!(!pause.take_processing());
    assert!(pause.resume());
    assert_eq!(block(&pause), BlockAction::Start);
}

#[test]
fn the_full_sequence_stops_and_restarts_once() {
    let pause = running();
    assert_eq!(block(&pause), BlockAction::Process);
    pause.request();
    assert!(!pause.is_paused());
    let stop = pause.begin_block();
    assert_eq!(stop.action(), BlockAction::Stop);
    // Not paused until the stop call is done.
    assert!(!pause.is_paused());
    pause.stopped();
    assert!(pause.is_paused());
    assert!(!pause.processing());
    // Not quiet until the block is over.
    assert!(!pause.quiet());
    drop(stop);
    assert!(pause.quiet());
    assert!(!pause.take_processing());
    assert_eq!(block(&pause), BlockAction::Skip);
    assert_eq!(block(&pause), BlockAction::Skip);
    assert!(pause.resume());
    assert!(!pause.is_paused());
    let start = pause.begin_block();
    assert_eq!(start.action(), BlockAction::Start);
    pause.started();
    drop(start);
    assert!(pause.processing());
    assert_eq!(block(&pause), BlockAction::Process);
    assert_eq!(pause.paused_blocks(), 3);
}

#[test]
fn request_is_idempotent() {
    let pause = running();
    pause.request();
    pause.request();
    let stop = pause.begin_block();
    assert_eq!(stop.action(), BlockAction::Stop);
    pause.request();
    pause.stopped();
    drop(stop);
    pause.request();
    assert!(pause.quiet());
    assert_eq!(block(&pause), BlockAction::Skip);
}

#[test]
fn resume_before_the_stop_cancels_the_request() {
    let pause = running();
    pause.request();
    assert!(pause.resume());
    assert_eq!(block(&pause), BlockAction::Process);
    assert_eq!(pause.paused_blocks(), 0);
    assert!(pause.processing());
}

#[test]
fn resume_during_the_stop_call_starts_on_the_next_block() {
    let pause = running();
    pause.request();
    let stop = pause.begin_block();
    assert_eq!(stop.action(), BlockAction::Stop);
    assert!(pause.resume());
    pause.stopped();
    drop(stop);
    assert!(!pause.is_paused());
    assert!(!pause.processing());
    assert_eq!(block(&pause), BlockAction::Start);
    assert_eq!(block(&pause), BlockAction::Process);
}

#[test]
fn request_again_during_the_stop_call_ends_paused() {
    let pause = running();
    pause.request();
    let stop = pause.begin_block();
    assert_eq!(stop.action(), BlockAction::Stop);
    assert!(pause.resume());
    pause.request();
    pause.stopped();
    drop(stop);
    assert!(pause.quiet());
    assert_eq!(block(&pause), BlockAction::Skip);
    assert!(pause.resume());
    assert_eq!(block(&pause), BlockAction::Start);
}

#[test]
fn request_before_the_restart_cancels_the_resume() {
    let pause = stopped();
    assert!(pause.resume());
    pause.request();
    // Still stopped: the audio side confirms without a second stop call.
    assert!(!pause.is_paused());
    assert_eq!(block(&pause), BlockAction::Skip);
    assert!(pause.quiet());
    assert!(!pause.take_processing());
    assert!(pause.resume());
    assert_eq!(block(&pause), BlockAction::Start);
}

#[test]
fn forcing_stops_only_a_processing_plugin_once_the_block_is_over() {
    let pause = running();
    let inside = pause.begin_block();
    assert_eq!(inside.action(), BlockAction::Process);
    pause.force();
    assert!(pause.is_paused());
    assert!(!pause.quiet());
    drop(inside);
    assert!(pause.quiet());
    assert!(pause.take_processing());
    assert!(!pause.take_processing(), "the stop call is made once");
    assert_eq!(block(&pause), BlockAction::Skip);

    let pause = running();
    pause.request();
    pause.force();
    assert!(pause.quiet());
    assert!(pause.take_processing());

    let pause = running();
    pause.request();
    let stop = pause.begin_block();
    assert_eq!(stop.action(), BlockAction::Stop);
    pause.force();
    assert!(!pause.quiet());
    pause.stopped();
    drop(stop);
    assert!(pause.quiet());
    assert!(!pause.take_processing());

    let pause = PauseHandshake::new();
    let start = pause.begin_block();
    assert_eq!(start.action(), BlockAction::Start);
    pause.force();
    pause.started();
    assert!(!pause.quiet());
    drop(start);
    assert!(pause.quiet());
    assert!(pause.take_processing());

    let pause = stopped();
    pause.force();
    assert!(pause.quiet());
    assert!(!pause.take_processing());
    assert!(pause.resume());
    pause.force();
    assert!(pause.quiet());
    assert!(!pause.take_processing());
    let new = PauseHandshake::new();
    new.force();
    assert!(new.quiet());
    assert!(!new.take_processing());

    assert!(pause.resume());
    assert!(!pause.is_paused());
    assert_eq!(block(&pause), BlockAction::Start);
}

#[test]
fn resume_after_a_force_waits_for_quiet_and_the_stop_call() {
    let pause = running();
    let inside = pause.begin_block();
    assert_eq!(inside.action(), BlockAction::Process);
    pause.force();
    assert!(!pause.resume(), "a block is still inside");
    assert!(pause.is_paused());
    drop(inside);
    assert!(!pause.resume(), "quiet has not confirmed the force");
    assert_eq!(block(&pause), BlockAction::Skip);
    assert!(pause.quiet());
    assert!(!pause.resume(), "still processing");
    assert!(pause.take_processing());
    assert!(pause.resume());
    assert_eq!(block(&pause), BlockAction::Start);

    let pause = PauseHandshake::new();
    let start = pause.begin_block();
    assert_eq!(start.action(), BlockAction::Start);
    pause.force();
    assert!(!pause.resume());
    pause.started();
    drop(start);
    assert!(pause.quiet());
    assert!(!pause.resume());
    assert!(pause.take_processing());
    assert!(pause.resume());
    assert_eq!(block(&pause), BlockAction::Start);

    let pause = running();
    pause.request();
    let stop = pause.begin_block();
    assert_eq!(stop.action(), BlockAction::Stop);
    pause.stopped();
    assert!(pause.resume());
    drop(stop);
    assert_eq!(block(&pause), BlockAction::Start);
}

#[test]
fn a_failed_start_pauses_until_resumed() {
    let pause = PauseHandshake::new();
    let start = pause.begin_block();
    assert_eq!(start.action(), BlockAction::Start);
    pause.start_failed();
    drop(start);
    assert!(pause.is_paused());
    assert!(!pause.processing());
    assert_eq!(block(&pause), BlockAction::Skip);
    assert_eq!(block(&pause), BlockAction::Skip);
    assert!(pause.quiet());
    assert!(pause.resume());
    assert_eq!(block(&pause), BlockAction::Start);

    // A request during the failed start: paused, not a Stop on it.
    let pause = PauseHandshake::new();
    let start = pause.begin_block();
    assert_eq!(start.action(), BlockAction::Start);
    pause.request();
    pause.start_failed();
    drop(start);
    assert!(pause.quiet());
    assert!(!pause.take_processing());
    assert_eq!(block(&pause), BlockAction::Skip);
}

#[test]
fn drive_pause_resets_its_wait_for_the_next_pause() {
    let pause = running();
    let stops = AtomicU64::new(0);
    let mut wait = None;
    assert_eq!(drive(&pause, &mut wait, &stops), PauseStep::Waiting);
    let stop = pause.begin_block();
    assert_eq!(stop.action(), BlockAction::Stop);
    pause.stopped();
    drop(stop);
    assert_eq!(drive(&pause, &mut wait, &stops), PauseStep::Ready);
    assert_eq!(wait, None);
    assert!(pause.resume());
    let start = pause.begin_block();
    assert_eq!(start.action(), BlockAction::Start);
    pause.started();
    drop(start);

    assert_eq!(drive(&pause, &mut wait, &stops), PauseStep::Waiting);
    assert_eq!(block(&pause), BlockAction::Stop);
}

fn drive(
    pause: &PauseHandshake,
    wait: &mut Option<(Instant, u64)>,
    stops: &AtomicU64,
) -> PauseStep {
    drive_pause(pause, wait, || {
        stops.fetch_add(1, Ordering::Relaxed);
    })
}

#[test]
fn a_pause_before_the_first_block_is_ready_at_once() {
    let pause = PauseHandshake::new();
    let stops = AtomicU64::new(0);
    let mut wait = None;
    assert_eq!(drive(&pause, &mut wait, &stops), PauseStep::Ready);
    assert_eq!(stops.load(Ordering::Relaxed), 0, "nothing to stop");
    assert!(pause.resume());
    assert_eq!(block(&pause), BlockAction::Start);
}

#[test]
fn a_pause_while_blocks_run_waits_for_the_audio_threads_stop() {
    let pause = running();
    let stops = AtomicU64::new(0);
    let mut wait = None;
    assert_eq!(drive(&pause, &mut wait, &stops), PauseStep::Waiting);
    assert_eq!(drive(&pause, &mut wait, &stops), PauseStep::Waiting);
    let stop = pause.begin_block();
    assert_eq!(stop.action(), BlockAction::Stop);
    assert_eq!(drive(&pause, &mut wait, &stops), PauseStep::Waiting);
    pause.stopped();
    // Paused, but the block is still inside the plugin.
    assert_eq!(drive(&pause, &mut wait, &stops), PauseStep::Waiting);
    drop(stop);
    assert_eq!(drive(&pause, &mut wait, &stops), PauseStep::Ready);
    assert_eq!(
        stops.load(Ordering::Relaxed),
        0,
        "the audio thread stopped it"
    );
    assert!(pause.resume());
    assert_eq!(block(&pause), BlockAction::Start);
}

#[test]
fn a_pause_after_the_stream_stopped_is_forced_after_the_stall() {
    let pause = running();
    let stops = AtomicU64::new(0);
    let mut wait = None;
    assert_eq!(drive(&pause, &mut wait, &stops), PauseStep::Waiting);
    assert_eq!(drive(&pause, &mut wait, &stops), PauseStep::Waiting);
    let (_, seen) = wait.expect("waiting");
    let long_ago = Instant::now().checked_sub(PAUSE_STALL).expect("uptime");
    // A block served since the last frame restarts the wait.
    wait = Some((long_ago, seen.wrapping_sub(1)));
    assert_eq!(drive(&pause, &mut wait, &stops), PauseStep::Waiting);
    let (since, now_seen) = wait.expect("waiting");
    assert!(since > long_ago);
    assert_eq!(now_seen, seen);
    // No block for the stall: the stream is taken to be stopped.
    wait = Some((long_ago, seen));
    assert_eq!(drive(&pause, &mut wait, &stops), PauseStep::Ready);
    assert_eq!(
        stops.load(Ordering::Relaxed),
        1,
        "the UI made the stop call"
    );
    assert!(!pause.processing());
    assert_eq!(block(&pause), BlockAction::Skip);
    assert!(pause.resume());
    assert_eq!(block(&pause), BlockAction::Start);
}

#[test]
fn a_pause_that_stays_unquiet_is_stranded_after_the_deadline() {
    // A block is inside the plugin and never comes out: a plugin hung in
    // `process`.
    let pause = Arc::new(running());
    let (entered, was_entered) = std::sync::mpsc::channel();
    let release = Arc::new(AtomicBool::new(false));
    let audio = {
        let pause = Arc::clone(&pause);
        let release = Arc::clone(&release);
        std::thread::spawn(move || {
            let block = pause.begin_block();
            assert_eq!(block.action(), BlockAction::Process);
            entered.send(()).expect("the block began");
            while !release.load(Ordering::Relaxed) {
                std::thread::yield_now();
            }
            drop(block);
        })
    };
    was_entered.recv().expect("the block began");

    let stops = AtomicU64::new(0);
    let mut wait = None;
    assert_eq!(drive(&pause, &mut wait, &stops), PauseStep::Waiting);
    // The deadline is seeded, not slept: the wait runs since the request.
    let (_, served) = wait.expect("waiting");
    let long_ago = Instant::now().checked_sub(PAUSE_STRANDED).expect("uptime");
    wait = Some((long_ago, served));
    assert_eq!(drive(&pause, &mut wait, &stops), PauseStep::Stranded);
    assert!(pause.stranded());
    assert_eq!(wait, None, "the wait is dropped");
    assert_eq!(
        stops.load(Ordering::Relaxed),
        0,
        "no stop call under the running one"
    );

    // A late quiet does not resume it: the plugin is never trusted again.
    release.store(true, Ordering::Relaxed);
    audio.join().expect("audio thread");
    assert!(pause.quiet());
    assert_eq!(drive(&pause, &mut wait, &stops), PauseStep::Stranded);
    assert!(pause.stranded());
    assert_eq!(
        stops.load(Ordering::Relaxed),
        0,
        "a stranded pause drives nothing"
    );
}

#[test]
fn a_pause_unquiet_for_less_than_the_deadline_only_waits() {
    let pause = running();
    pause.request();
    let stop = pause.begin_block();
    assert_eq!(stop.action(), BlockAction::Stop);
    pause.stopped();
    let stops = AtomicU64::new(0);
    let mut wait = None;
    assert_eq!(drive(&pause, &mut wait, &stops), PauseStep::Waiting);
    let (_, served) = wait.expect("waiting");
    let recently = Instant::now()
        .checked_sub(PAUSE_STRANDED / 2)
        .expect("uptime");
    wait = Some((recently, served));
    assert_eq!(drive(&pause, &mut wait, &stops), PauseStep::Waiting);
    assert!(!pause.stranded());
    drop(stop);
    assert_eq!(drive(&pause, &mut wait, &stops), PauseStep::Ready);
    assert_eq!(stops.load(Ordering::Relaxed), 0, "the block stopped it");
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
                let block = pause.begin_block();
                match block.action() {
                    BlockAction::Stop => pause.stopped(),
                    BlockAction::Start => return work.load(Ordering::Relaxed),
                    BlockAction::Process | BlockAction::Skip => {
                        drop(block);
                        std::thread::yield_now();
                    }
                }
            }
        })
    };
    pause.request();
    let started = Instant::now();
    while !pause.quiet() {
        assert!(started.elapsed() < Duration::from_secs(10), "never paused");
        std::thread::yield_now();
    }
    work.store(42, Ordering::Relaxed);
    assert!(pause.resume());
    assert_eq!(audio.join().unwrap(), 42);
    assert!(pause.paused_blocks() >= 1);
}

/// A plugin stand-in: whoever calls into it marks `inside` for the call,
/// and it tracks whether it is processing, so an overlap or a stop/start
/// out of turn fails the test.
#[derive(Default)]
struct FakePlugin {
    inside: AtomicBool,
    processing: AtomicBool,
}

impl FakePlugin {
    fn call(&self, body: impl FnOnce(&Self)) {
        assert!(
            !self.inside.swap(true, Ordering::SeqCst),
            "two threads inside"
        );
        for _ in 0..32 {
            std::hint::spin_loop();
        }
        body(self);
        self.inside.store(false, Ordering::SeqCst);
    }

    fn start(&self) {
        self.call(|plugin| {
            assert!(
                !plugin.processing.swap(true, Ordering::SeqCst),
                "started twice"
            )
        });
    }

    fn stop(&self) {
        self.call(|plugin| {
            assert!(
                plugin.processing.swap(false, Ordering::SeqCst),
                "stopped twice"
            )
        });
    }

    fn process(&self) {
        self.call(|plugin| assert!(plugin.processing.load(Ordering::SeqCst), "not started"));
    }

    fn work(&self) {
        self.call(|plugin| {
            assert!(
                !plugin.processing.load(Ordering::SeqCst),
                "still processing"
            )
        });
    }
}

/// On x86_64 (TSO) this cannot catch a weakened ordering in the handshake
/// (the hardware only lets a load pass an earlier store): that needs
/// aarch64 or loom.
#[test]
fn a_forced_pause_never_overlaps_a_block() {
    let pause = Arc::new(PauseHandshake::new());
    let plugin = Arc::new(FakePlugin::default());
    let done = Arc::new(AtomicBool::new(false));
    let audio = {
        let pause = Arc::clone(&pause);
        let plugin = Arc::clone(&plugin);
        let done = Arc::clone(&done);
        std::thread::spawn(move || {
            let mut blocks = 0u64;
            while !done.load(Ordering::Relaxed) {
                let block = pause.begin_block();
                match block.action() {
                    BlockAction::Stop => {
                        plugin.stop();
                        pause.stopped();
                    }
                    BlockAction::Start => {
                        plugin.start();
                        pause.started();
                        plugin.process();
                    }
                    BlockAction::Process => plugin.process(),
                    BlockAction::Skip => {}
                }
                drop(block);
                blocks += 1;
                if blocks.is_multiple_of(64) {
                    std::thread::yield_now();
                }
            }
            blocks
        })
    };

    let started = Instant::now();
    let mut pauses = 0u64;
    let mut ui_stops = 0u64;
    while started.elapsed() < Duration::from_millis(200) {
        let deadline = Instant::now() + Duration::from_secs(10);
        if pauses.is_multiple_of(2) {
            let mut wait = None;
            while drive_pause(&pause, &mut wait, || {
                ui_stops += 1;
                plugin.stop();
            }) == PauseStep::Waiting
            {
                assert!(Instant::now() < deadline, "never ready");
                std::thread::yield_now();
            }
        } else {
            // Forced at once, whatever the audio thread is doing.
            pause.request();
            pause.force();
            while !pause.quiet() {
                assert!(Instant::now() < deadline, "never quiet");
                std::hint::spin_loop();
            }
            if pause.take_processing() {
                ui_stops += 1;
                plugin.stop();
            }
        }
        plugin.work();
        assert!(pause.resume());
        pauses += 1;
        for _ in 0..(pauses % 7) * 50 {
            std::hint::spin_loop();
        }
    }
    done.store(true, Ordering::Relaxed);
    let blocks = audio.join().expect("audio thread");
    assert!(pauses > 0 && blocks > 0, "{pauses} pauses, {blocks} blocks");
    eprintln!("{pauses} pauses, {ui_stops} stopped by the UI, {blocks} blocks");
}

#[test]
fn in_pause_runs_from_the_request_to_the_resume() {
    let pause = running();
    assert!(!pause.in_pause());
    pause.request();
    assert!(pause.in_pause(), "requested");
    let stop = pause.begin_block();
    assert_eq!(stop.action(), BlockAction::Stop);
    assert!(pause.in_pause(), "stopping");
    pause.stopped();
    drop(stop);
    assert!(pause.in_pause(), "paused");
    assert!(pause.quiet());
    assert!(pause.resume());
    assert!(!pause.in_pause(), "resuming");
    assert_eq!(block(&pause), BlockAction::Start);
    assert!(!pause.in_pause());
    // A new plugin waiting for its first block is not in a pause, and a
    // forced one is.
    let fresh = PauseHandshake::new();
    assert!(!fresh.in_pause());
    fresh.force();
    assert!(fresh.in_pause());
}
