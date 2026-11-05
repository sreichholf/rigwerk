#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod audio;
mod ui;

use std::path::PathBuf;

fn parse_screenshot() -> Option<PathBuf> {
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        if argument == "--screenshot" {
            return args.next().map(PathBuf::from);
        }
        if let Some(path) = argument.strip_prefix("--screenshot=") {
            return Some(PathBuf::from(path));
        }
    }
    None
}

/// A Windows release build is a GUI subsystem process, so it has no console.
/// `--screenshot` still reports usage and save results on stdout/stderr.
/// Attach the parent console (started from a terminal) or allocate one
/// (double-click) before those lines run. Debug builds already have a
/// console, and a redirected stderr handle is left alone.
#[cfg(windows)]
fn ensure_screenshot_console() {
    let requested = std::env::args()
        .any(|argument| argument == "--screenshot" || argument.starts_with("--screenshot="));
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

    // SAFETY: kernel32 console calls. Handles are only passed back to
    // kernel32 and are never dereferenced. Called before any Rust stdio use,
    // which reads the standard handles on each write.
    unsafe {
        if !handle_missing(GetStdHandle(STD_ERROR_HANDLE)) {
            return;
        }
        let attached = AttachConsole(ATTACH_PARENT_PROCESS) != 0;
        if !attached && AllocConsole() == 0 {
            return;
        }
        // AllocConsole installs standard handles. AttachConsole does not, so
        // point stdout and stderr at this process's console before eprintln.
        if handle_missing(GetStdHandle(STD_ERROR_HANDLE)) {
            for std_handle in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
                let console = CreateFileA(
                    b"CONOUT$\0".as_ptr(),
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

fn main() -> eframe::Result<()> {
    #[cfg(windows)]
    ensure_screenshot_console();

    let screenshot = parse_screenshot();
    if std::env::args().any(|argument| argument == "--screenshot") && screenshot.is_none() {
        eprintln!("Usage: rigwerk --screenshot <file.png>");
        std::process::exit(2);
    }

    // Screenshots need the Live page (and routing side-by-side) without a
    // central scrollbar. Do not restore the last interactive window size.
    // Wayland compositors ignore client resize requests, so screenshot mode
    // drops Wayland and opens through X11/XWayland where the size sticks.
    let screenshot_mode = screenshot.is_some();
    if screenshot_mode {
        #[cfg(target_os = "linux")]
        {
            // SAFETY: no other threads have started; eframe has not opened a
            // display yet. winit picks Wayland whenever WAYLAND_DISPLAY is set.
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
        app::screenshot::SCREENSHOT_INNER_SIZE
    } else {
        eframe::egui::vec2(1240.0, 820.0)
    };
    let mut viewport = eframe::egui::ViewportBuilder::default()
        .with_inner_size([size.x, size.y])
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

    eframe::run_native(
        "RigWerk",
        options,
        Box::new(move |cc| Ok(Box::new(app::RigWerkApp::new(cc, screenshot.clone())))),
    )
}
