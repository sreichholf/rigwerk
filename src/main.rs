#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod audio;
mod ui;

/// Catches an allocation on the sample path, in a debug build and the tests.
#[cfg(debug_assertions)]
#[global_allocator]
static ALLOCATOR: audio::rt_guard::GuardAllocator = audio::rt_guard::GuardAllocator;

#[cfg(test)]
mod tests;

use std::ffi::OsStr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;
use std::time::Duration;

fn parse_screenshot() -> Option<PathBuf> {
    // `args_os`: a non-UTF-8 path is valid, including a scan child's `TEMP`.
    let mut args = std::env::args_os().skip(1);
    while let Some(argument) = args.next() {
        if argument.as_os_str() == OsStr::new("--screenshot") {
            return args.next().map(PathBuf::from);
        }
        let text = argument.to_str();
        if let Some(path) = text.and_then(|text| text.strip_prefix("--screenshot=")) {
            return Some(PathBuf::from(path));
        }
    }
    None
}

/// The scan file stays an `OsString`: the parent's temp path need not be UTF-8.
fn parse_clap_scan(
    args: impl Iterator<Item = std::ffi::OsString>,
) -> (bool, Option<(PathBuf, Duration)>) {
    let mut args = args.skip(1);
    while let Some(argument) = args.next() {
        if argument.as_os_str() != OsStr::new("--clap-scan") {
            continue;
        }
        let path = args.next().map(PathBuf::from);
        let limit = args
            .next()
            .and_then(|limit| limit.to_str()?.parse::<u64>().ok())
            .map(Duration::from_millis);
        return (true, path.zip(limit));
    }
    (false, None)
}

/// A bundle crash dialog would wait forever: the scan child has no window.
/// Call this before any plugin code.
#[cfg(windows)]
fn silence_child_crashes() {
    const SEM_FAILCRITICALERRORS: u32 = 0x0001;
    const SEM_NOGPFAULTERRORBOX: u32 = 0x0002;
    const SEM_NOOPENFILEERRORBOX: u32 = 0x8000;

    #[link(name = "kernel32")]
    extern "system" {
        fn SetErrorMode(mode: u32) -> u32;
    }

    // SAFETY: a bitmask, no pointers, on this thread before any scan thread,
    // so every bundle fault sees it.  The previous mode is unused.
    unsafe {
        SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX);
    }
}

/// A Windows release build is a GUI subsystem process and has no console.
/// `--screenshot` still writes stdout and stderr.  An existing stderr
/// handle (a debug console, or a redirect) is left alone.
#[cfg(windows)]
fn ensure_screenshot_console() {
    let requested = std::env::args_os().any(|argument| {
        argument.as_os_str() == OsStr::new("--screenshot")
            || argument
                .to_str()
                .is_some_and(|text| text.starts_with("--screenshot="))
    });
    if !requested {
        return;
    }

    use std::ffi::c_void;

    const ATTACH_PARENT_PROCESS: u32 = 0xFFFF_FFFF;
    const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5;
    const STD_ERROR_HANDLE: u32 = 0xFFFF_FFF4;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const FILE_SHARE_READ_WRITE: u32 = 0x0000_0003;
    const OPEN_EXISTING: u32 = 3;

    #[link(name = "kernel32")]
    extern "system" {
        fn AttachConsole(process_id: u32) -> i32;
        fn AllocConsole() -> i32;
        fn GetStdHandle(std_handle: u32) -> *mut c_void;
        fn SetStdHandle(std_handle: u32, handle: *mut c_void) -> i32;
        fn CreateFileA(
            file_name: *const u8,
            desired_access: u32,
            share_mode: u32,
            security_attributes: *mut c_void,
            creation_disposition: u32,
            flags_and_attributes: u32,
            template_file: *mut c_void,
        ) -> *mut c_void;
    }

    fn handle_missing(handle: *mut c_void) -> bool {
        handle.is_null() || handle as isize == -1
    }

    // SAFETY: handles are only passed back to kernel32, never dereferenced.
    // Before any Rust stdio, which reads the standard handles on each write.
    unsafe {
        if !handle_missing(GetStdHandle(STD_ERROR_HANDLE)) {
            return;
        }
        let attached = AttachConsole(ATTACH_PARENT_PROCESS) != 0;
        if !attached && AllocConsole() == 0 {
            return;
        }
        // AllocConsole installs standard handles.  AttachConsole does not, so
        // point stdout and stderr at this process's console before eprintln.
        if handle_missing(GetStdHandle(STD_ERROR_HANDLE)) {
            for std_handle in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
                let console = CreateFileA(
                    c"CONOUT$".as_ptr().cast(),
                    GENERIC_WRITE,
                    FILE_SHARE_READ_WRITE,
                    std::ptr::null_mut(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                );
                if !handle_missing(console) {
                    let _ = SetStdHandle(std_handle, console);
                }
            }
        }
    }
}

/// `None` keeps eframe's own error: nothing was recorded and the launch failed.
fn screenshot_exit_status(recorded: i32, launch_ok: bool) -> Option<i32> {
    if recorded != app::screenshot::NO_ENDING {
        Some(recorded)
    } else if launch_ok {
        // The window closed on its own without the run saving.
        Some(app::screenshot::EXIT_RUN_TIMED_OUT)
    } else {
        None
    }
}

fn main() -> eframe::Result<()> {
    // The child half of `audio::clap::scan_library_isolated`, before the
    // screenshot console and eframe, so the child opens no window and the
    // parent reads only the exit status and the file.
    let (scan_requested, scan) = parse_clap_scan(std::env::args_os());
    if scan_requested {
        let Some((path, limit)) = scan else {
            std::process::exit(2);
        };
        #[cfg(windows)]
        silence_child_crashes();
        // Last resort if the parent's kill never arrives.  `exit` skips
        // unwinding: a bundle thread is not ours to join, and the parent
        // reads only the code.
        std::thread::spawn(move || {
            std::thread::sleep(limit);
            std::process::exit(audio::clap::CHILD_EXIT_ABANDONED);
        });
        let found = audio::clap::scan_library();
        let code = match audio::clap::write_scan_file(&path, found) {
            Ok(()) => 0,
            Err(_) => audio::clap::CHILD_EXIT_UNWRITABLE,
        };
        std::process::exit(code);
    }

    #[cfg(windows)]
    ensure_screenshot_console();

    let screenshot = parse_screenshot();
    if std::env::args_os().any(|argument| argument.as_os_str() == OsStr::new("--screenshot"))
        && screenshot.is_none()
    {
        eprintln!("Usage: rigwerk --screenshot <file.png>");
        std::process::exit(2);
    }

    // A fixed size with no central scrollbar, so the last interactive size
    // is not restored.  Wayland ignores client resize, so this mode drops
    // it and opens through X11/XWayland.
    let screenshot_mode = screenshot.is_some();
    // A sentinel no ending uses, so a recorded failure stays distinct from
    // nothing recorded.
    let screenshot_exit = Arc::new(AtomicI32::new(app::screenshot::NO_ENDING));
    if screenshot_mode {
        #[cfg(target_os = "linux")]
        {
            // SAFETY: no other threads have started, and eframe has not opened
            // a display.  winit picks Wayland when `WAYLAND_DISPLAY` is set.
            unsafe {
                std::env::remove_var("WAYLAND_DISPLAY");
                std::env::remove_var("WAYLAND_SOCKET");
            }
            if std::env::var_os("DISPLAY").is_none() {
                eprintln!(
                    "Screenshot mode needs X11 (DISPLAY). On Wayland, enable XWayland or use Xvfb."
                );
                std::process::exit(2);
            }
        }
    }
    let size = if screenshot_mode {
        app::screenshot::screenshot_inner_size()
    } else {
        eframe::egui::vec2(1240.0, 820.0)
    };
    let mut viewport = eframe::egui::ViewportBuilder::default()
        .with_inner_size([size.x, size.y])
        .with_icon(ui::brand::window_icon())
        .with_title("RigWerk - independent audio bridge");
    if screenshot_mode {
        viewport = viewport
            .with_min_inner_size([size.x, size.y])
            .with_max_inner_size([size.x, size.y]);
    } else {
        viewport = viewport.with_min_inner_size([1040.0, 700.0]);
    }
    let options = eframe::NativeOptions {
        viewport,
        persist_window: !screenshot_mode,
        ..Default::default()
    };

    // Deadlines are checked only while frames are drawn.  Teardown, the app
    // drop and the engine stop are outside that, and a driver can stall
    // there.  Sheets and drawers are root panels; plugin editors are native
    // windows the app drop closes, so closing the root is every viewport.
    if screenshot_mode {
        std::thread::spawn(|| {
            std::thread::sleep(app::screenshot::RUN_TIMEOUT);
            eprintln!(
                "Screenshot run did not finish within {} s.",
                app::screenshot::RUN_TIMEOUT.as_secs()
            );
            std::process::exit(app::screenshot::EXIT_RUN_TIMED_OUT);
        });
    }

    let app_exit = Arc::clone(&screenshot_exit);
    let result = eframe::run_native(
        "RigWerk",
        options,
        Box::new(move |cc| {
            Ok(Box::new(app::RigWerkApp::new(
                cc,
                screenshot.clone(),
                app_exit.clone(),
            )))
        }),
    );
    if screenshot_mode {
        let end = screenshot_exit.load(Ordering::Acquire);
        if let Some(status) = screenshot_exit_status(end, result.is_ok()) {
            std::process::exit(status);
        }
    }
    result
}
