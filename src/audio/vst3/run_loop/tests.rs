use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use ::vst3::Steinberg::Linux::{
    FileDescriptor, IEventHandler, IEventHandlerTrait, IRunLoop, IRunLoopTrait, ITimerHandler,
    ITimerHandlerTrait,
};
use ::vst3::Steinberg::{kInvalidArgument, kResultFalse, kResultTrue, FUnknown};
use ::vst3::{Class, ComPtr, ComRef, ComWrapper};

use super::{counts, forget_module, module_base, service_run_loop};

/// A timer that counts its calls and, on each, unregisters `targets`.
#[derive(Default)]
struct Timer {
    fired: AtomicUsize,
    targets: Mutex<Vec<usize>>,
}

impl Class for Timer {
    type Interfaces = (ITimerHandler,);
}

impl ITimerHandlerTrait for Timer {
    unsafe fn onTimer(&self) {
        self.fired.fetch_add(1, Ordering::SeqCst);
        let targets = self.targets.lock().expect("targets").clone();
        for target in targets {
            super::unregister_timer_everywhere(target as *mut ITimerHandler);
        }
    }
}

/// An fd handler that drains its pipe and counts its calls.
#[derive(Default)]
struct Reader {
    fired: AtomicUsize,
}

impl Class for Reader {
    type Interfaces = (IEventHandler,);
}

impl IEventHandlerTrait for Reader {
    unsafe fn onFDIsSet(&self, fd: FileDescriptor) {
        self.fired.fetch_add(1, Ordering::SeqCst);
        let mut byte = 0u8;
        // SAFETY: a one-byte read into a local.
        unsafe { libc::read(fd, (&mut byte as *mut u8).cast(), 1) };
    }
}

/// The host context's run loop, the one VSTGUI 3.8 registers through.
fn host_run_loop() -> ComPtr<IRunLoop> {
    // SAFETY: the host context is a live static.
    unsafe { ComRef::<FUnknown>::from_raw(crate::audio::vst3::host::host_context()) }
        .expect("a host context")
        .cast::<IRunLoop>()
        .expect("the host context is a run loop")
}

fn timer_ptr(timer: &ComWrapper<Timer>) -> *mut ITimerHandler {
    timer.as_com_ref::<ITimerHandler>().expect("timer").as_ptr()
}

fn fired(timer: &ComWrapper<Timer>) -> usize {
    timer.fired.load(Ordering::SeqCst)
}

/// The interval of the timer that must not fire twice in one interval: long
/// enough that two services back to back stay inside it on a loaded machine.
const ONCE_MS: u64 = 500;

#[test]
fn run_loop_fires_due_timers_once_and_survives_reentrant_unregister() {
    // The registry is process-wide and serviced on the UI thread.
    let _main = crate::audio::claim_main_thread();
    let run_loop = host_run_loop();
    let once = ComWrapper::new(Timer::default());
    let slow = ComWrapper::new(Timer::default());
    let killer = ComWrapper::new(Timer::default());
    let victim = ComWrapper::new(Timer::default());
    // The killer removes the victim (due in the same pass, after it) and
    // itself from inside its own callback.
    *killer.targets.lock().expect("targets") =
        vec![timer_ptr(&victim) as usize, timer_ptr(&killer) as usize];
    // SAFETY: live handlers, and null for the refused cases.
    unsafe {
        assert_eq!(
            run_loop.registerTimer(timer_ptr(&once), ONCE_MS),
            kResultTrue
        );
        assert_eq!(
            run_loop.registerTimer(timer_ptr(&slow), 3_600_000),
            kResultTrue
        );
        assert_eq!(run_loop.registerTimer(timer_ptr(&killer), 1), kResultTrue);
        assert_eq!(run_loop.registerTimer(timer_ptr(&victim), 1), kResultTrue);
        assert_eq!(
            run_loop.registerTimer(std::ptr::null_mut(), 1),
            kInvalidArgument
        );
        assert_eq!(
            run_loop.registerTimer(timer_ptr(&once), 0),
            kInvalidArgument
        );
    }
    assert_eq!(counts().1, 4);

    // The 1 ms timers are hundreds of intervals late: each fires once.
    std::thread::sleep(Duration::from_millis(ONCE_MS + 100));
    crate::audio::vst3::service_run_loop();
    assert_eq!(fired(&once), 1);
    assert_eq!(fired(&slow), 0);
    assert_eq!(fired(&killer), 1);
    assert_eq!(fired(&victim), 0, "unregistered before its turn");
    assert_eq!(counts().1, 2);
    // SAFETY: the victim pointer, already gone from the registry.
    assert_eq!(
        unsafe { run_loop.unregisterTimer(timer_ptr(&victim)) },
        kResultFalse
    );

    // Not due again yet: the next service is far inside the interval.
    crate::audio::vst3::service_run_loop();
    assert_eq!(fired(&once), 1);
    std::thread::sleep(Duration::from_millis(ONCE_MS + 100));
    crate::audio::vst3::service_run_loop();
    assert_eq!(fired(&once), 2);
    assert_eq!(fired(&killer), 1);
    assert_eq!(fired(&victim), 0);

    // SAFETY: live handlers.
    unsafe {
        assert_eq!(run_loop.unregisterTimer(timer_ptr(&once)), kResultTrue);
        assert_eq!(run_loop.unregisterTimer(timer_ptr(&slow)), kResultTrue);
    }
    assert_eq!(counts(), (0, 0));
}

#[test]
fn run_loop_calls_an_fd_handler_when_its_fd_is_readable() {
    let _main = crate::audio::claim_main_thread();
    let run_loop = host_run_loop();
    let mut pipe = [0; 2];
    // SAFETY: `pipe` has room for both ends.
    assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
    let reader = ComWrapper::new(Reader::default());
    let handler = reader
        .as_com_ref::<IEventHandler>()
        .expect("handler")
        .as_ptr();
    // SAFETY: a live handler; one handler per descriptor.
    unsafe {
        assert_eq!(run_loop.registerEventHandler(handler, pipe[0]), kResultTrue);
        assert_eq!(
            run_loop.registerEventHandler(handler, pipe[0]),
            kInvalidArgument
        );
    }
    crate::audio::vst3::service_run_loop();
    assert_eq!(reader.fired.load(Ordering::SeqCst), 0, "nothing to read");
    // SAFETY: a one-byte write from a local.
    assert_eq!(unsafe { libc::write(pipe[1], [1u8].as_ptr().cast(), 1) }, 1);
    crate::audio::vst3::service_run_loop();
    assert_eq!(reader.fired.load(Ordering::SeqCst), 1);
    crate::audio::vst3::service_run_loop();
    assert_eq!(reader.fired.load(Ordering::SeqCst), 1, "drained");
    // SAFETY: the registered handler, then our own descriptors.
    unsafe {
        assert_eq!(run_loop.unregisterEventHandler(handler), kResultTrue);
        assert_eq!(run_loop.unregisterEventHandler(handler), kResultFalse);
        libc::close(pipe[0]);
        libc::close(pipe[1]);
    }
    assert_eq!(counts(), (0, 0));
}

#[test]
fn unregister_event_handler_removes_every_fd_of_the_handler() {
    let _main = crate::audio::claim_main_thread();
    let run_loop = host_run_loop();
    let reader = ComWrapper::new(Reader::default());
    let handler = reader
        .as_com_ref::<IEventHandler>()
        .expect("handler")
        .as_ptr();
    let mut pipes = [[0; 2]; 2];
    for pipe in &mut pipes {
        // SAFETY: each array has room for both ends.
        assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
    }
    // SAFETY: a live handler, and two live descriptors.
    unsafe {
        assert_eq!(
            run_loop.registerEventHandler(handler, pipes[0][0]),
            kResultTrue
        );
        assert_eq!(
            run_loop.registerEventHandler(handler, pipes[1][0]),
            kResultTrue
        );
    }
    assert_eq!(counts().0, 2);
    // The interface has no descriptor argument: one call unregisters the
    // handler everywhere, as JUCE relies on when its editor closes.
    // SAFETY: the registered handler.
    assert_eq!(
        unsafe { run_loop.unregisterEventHandler(handler) },
        kResultTrue
    );
    assert_eq!(counts().0, 0, "both descriptors went with one unregister");
    // SAFETY: the handler is gone; close our own descriptors.
    unsafe {
        assert_eq!(run_loop.unregisterEventHandler(handler), kResultFalse);
        for pipe in pipes {
            libc::close(pipe[0]);
            libc::close(pipe[1]);
        }
    }
}

#[test]
fn one_run_loop_does_not_unregister_anothers_handler() {
    let _main = crate::audio::claim_main_thread();
    let host = host_run_loop();
    let frame = ComWrapper::new(crate::audio::vst3::editor::PlugFrame::new());
    let frame_loop = frame
        .as_com_ref::<IRunLoop>()
        .expect("the frame is a run loop");
    let reader = ComWrapper::new(Reader::default());
    let handler = reader
        .as_com_ref::<IEventHandler>()
        .expect("handler")
        .as_ptr();
    let mut pipes = [[0; 2]; 2];
    for pipe in &mut pipes {
        // SAFETY: each array has room for both ends.
        assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
    }
    // SAFETY: a live handler and two live descriptors on two run loops.
    unsafe {
        assert_eq!(host.registerEventHandler(handler, pipes[0][0]), kResultTrue);
        assert_eq!(
            frame_loop.registerEventHandler(handler, pipes[1][0]),
            kResultTrue
        );
    }
    assert_eq!(counts().0, 2);
    // SAFETY: the registered handler.
    assert_eq!(unsafe { host.unregisterEventHandler(handler) }, kResultTrue);
    assert_eq!(counts().0, 1, "the frame's registration stays");
    // SAFETY: the registered handler.
    assert_eq!(
        unsafe { frame_loop.unregisterEventHandler(handler) },
        kResultTrue
    );
    assert_eq!(counts().0, 0);
    // SAFETY: our own descriptors.
    unsafe {
        for pipe in pipes {
            libc::close(pipe[0]);
            libc::close(pipe[1]);
        }
    }
}

#[test]
fn unregister_timer_removes_every_registration_of_the_handler() {
    let _main = crate::audio::claim_main_thread();
    let run_loop = host_run_loop();
    let timer = ComWrapper::new(Timer::default());
    // SAFETY: a live handler; the interval is irrelevant here.
    unsafe {
        assert_eq!(
            run_loop.registerTimer(timer_ptr(&timer), 1_000),
            kResultTrue
        );
        assert_eq!(
            run_loop.registerTimer(timer_ptr(&timer), 2_000),
            kResultTrue
        );
    }
    assert_eq!(counts().1, 2);
    // SAFETY: the registered handler.
    assert_eq!(
        unsafe { run_loop.unregisterTimer(timer_ptr(&timer)) },
        kResultTrue
    );
    assert_eq!(counts().1, 0, "one unregister removes every interval");
}

#[test]
fn run_loop_forgets_the_handlers_of_an_unloaded_module() {
    let _main = crate::audio::claim_main_thread();
    let timer = ComWrapper::new(Timer::default());
    // Registered through an editor frame: the same registry.
    let frame = ComWrapper::new(crate::audio::vst3::editor::PlugFrame::new());
    let run_loop = frame
        .as_com_ref::<IRunLoop>()
        .expect("the frame is a run loop");
    // SAFETY: a live handler.
    assert_eq!(
        unsafe { run_loop.registerTimer(timer_ptr(&timer), 1) },
        kResultTrue
    );
    assert_eq!(counts(), (0, 1));
    // The handler's vtable is in this test binary, so its base is ours.
    let base = module_base(service_run_loop as *const c_void);
    assert_ne!(base, 0);
    forget_module(base + 1);
    assert_eq!(counts(), (0, 1), "another module keeps it");
    forget_module(base);
    assert_eq!(counts(), (0, 0));
    std::thread::sleep(Duration::from_millis(5));
    crate::audio::vst3::service_run_loop();
    assert_eq!(fired(&timer), 0);
}
