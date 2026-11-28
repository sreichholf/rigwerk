//! A draw pass never ends the process.  A watchdog in `main` bounds a
//! run whose repaints stop.  [`fixtures`] is built only for a screenshot run.

mod fixtures;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use eframe::egui;
use egui::{Vec2, ViewportCommand};

use crate::app::RigWerkApp;

/// Inner size that fits Live (and side-by-side routing) without a scrollbar.
const DEFAULT_INNER_SIZE: Vec2 = Vec2::new(1440.0, 1080.0);

/// The smallest and the largest size `RIGWERK_SCREENSHOT_SIZE` may ask for.
const MIN_INNER_SIZE: Vec2 = Vec2::new(640.0, 480.0);
const MAX_INNER_SIZE: Vec2 = Vec2::new(7680.0, 4320.0);

/// Test hook for screenshots at another window size:
/// `RIGWERK_SCREENSHOT_SIZE=<w>x<h>` sets the inner size in points.  A
/// blank value, one that does not parse, or one out of range is logged
/// and the default 1440×1080 is used.
pub(crate) fn screenshot_inner_size() -> Vec2 {
    static SIZE: std::sync::OnceLock<Vec2> = std::sync::OnceLock::new();
    *SIZE.get_or_init(|| inner_size_from(std::env::var("RIGWERK_SCREENSHOT_SIZE").ok().as_deref()))
}

/// The size `value` names.  `None` is an unset variable and stays the
/// default.  A present value that does not parse, including a blank one,
/// is logged and the default is used.
fn inner_size_from(value: Option<&str>) -> Vec2 {
    let Some(value) = value else {
        return DEFAULT_INNER_SIZE;
    };
    parse_inner_size(value).unwrap_or_else(|| {
        eprintln!(
            "RIGWERK_SCREENSHOT_SIZE={value:?} is not <w>x<h> from {:.0}x{:.0} to {:.0}x{:.0}; using {:.0}x{:.0}.",
            MIN_INNER_SIZE.x,
            MIN_INNER_SIZE.y,
            MAX_INNER_SIZE.x,
            MAX_INNER_SIZE.y,
            DEFAULT_INNER_SIZE.x,
            DEFAULT_INNER_SIZE.y
        );
        DEFAULT_INNER_SIZE
    })
}

/// `<w>x<h>` in whole points (`x` or `X`, spaces allowed around either
/// number), or `None` when it does not parse or is out of range.
fn parse_inner_size(value: &str) -> Option<Vec2> {
    let (width, height) = value.trim().split_once(['x', 'X'])?;
    let width: u32 = width.trim().parse().ok()?;
    let height: u32 = height.trim().parse().ok()?;
    let size = Vec2::new(width as f32, height as f32);
    (size.x >= MIN_INNER_SIZE.x
        && size.y >= MIN_INNER_SIZE.y
        && size.x <= MAX_INNER_SIZE.x
        && size.y <= MAX_INNER_SIZE.y)
        .then_some(size)
}

/// Test hook for screenshots further down a page: `RIGWERK_SCROLL=<px>`
/// scrolls the page by that much (screenshot mode only).
pub(crate) fn screenshot_scroll() -> Option<f32> {
    static SCROLL: std::sync::OnceLock<Option<f32>> = std::sync::OnceLock::new();
    *SCROLL.get_or_init(|| {
        std::env::var("RIGWERK_SCROLL")
            .ok()
            .and_then(|value| value.trim().parse().ok())
    })
}

/// The `<path number>=<file>` entries of the `;` list in `var`.
fn screenshot_files(var: &str) -> Vec<(usize, String)> {
    parse_screenshot_files(var, &std::env::var(var).unwrap_or_default())
}

/// The `<path number>=<file>` entries of `list`.  An entry without a
/// number is logged under `var` and skipped.
fn parse_screenshot_files(var: &str, list: &str) -> Vec<(usize, String)> {
    list.split(';')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .filter_map(|entry| {
            let parsed = entry.split_once('=').and_then(|(number, file)| {
                Some((number.trim().parse().ok()?, file.trim().to_owned()))
            });
            if parsed.is_none() {
                eprintln!("{var} entry without a path number: {entry}");
            }
            parsed
        })
        .collect()
}

/// Test hook for screenshots of a running bridge: `RIGWERK_SCREENSHOT_START=1`
/// starts it after the setup, so meters and the scope show real signal.
/// It opens the selected devices; use it against a virtual graph.
fn screenshot_start() -> bool {
    std::env::var("RIGWERK_SCREENSHOT_START").is_ok_and(|value| value.trim() == "1")
}

/// Frames to wait before the capture: longer once a bridge has started, so
/// the meters, the scope and the extra output links have settled.
const SETTLE_FRAMES: u64 = 120;
const SETTLE_FRAMES_RUNNING: u64 = 480;

/// How long a requested capture may take before the frame loop gives up.  A
/// capture arrives on the frame after it was asked for, so this deadline is
/// only checked while frames are still being drawn; [`RUN_TIMEOUT`] is what
/// bounds a run whose repaints stop altogether.
const CAPTURE_TIMEOUT: f64 = 30.0;

/// How long a whole run may take, the close and the teardown included,
/// before the watchdog in `main` stops the process.  It sits past
/// [`CAPTURE_TIMEOUT`], because that one needs live frames while this one
/// also bounds the window close, the app drop and the engine stop after it.
pub(crate) const RUN_TIMEOUT: Duration = Duration::from_secs(90);

/// The shared cell's value while the run has no ending yet.  It cannot be an
/// exit code, so `main` can tell "recorded a failure" from "recorded
/// nothing".
pub(crate) const NO_ENDING: i32 = i32::MIN;

/// The watchdog's exit code: no ending was recorded within [`RUN_TIMEOUT`],
/// or the window closed before the run reached one.
pub(crate) const EXIT_RUN_TIMED_OUT: i32 = 4;

/// How a screenshot run ended.  [`Self::exit_code`] is the whole decision,
/// so the exit path is a pure function and is tested headlessly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    /// The file was written.
    Saved,
    /// The window never took the requested size, so the capture would show
    /// another layout.
    ViewportMismatch,
    /// The capture was asked for but did not arrive.
    Timeout,
    /// The image could not be written.
    SaveFailed,
}

impl End {
    /// The exit code for this ending: 0 only when the file was saved, and a
    /// code of its own for each way a run can fail.  [`Self::Timeout`] is 5
    /// rather than 2, because `main` uses 2 for the pre-run failures that
    /// never reach a frame (a `--screenshot` usage error or no X11 display).
    fn exit_code(self) -> i32 {
        match self {
            Self::Saved => 0,
            Self::ViewportMismatch => 1,
            Self::Timeout => 5,
            Self::SaveFailed => 3,
        }
    }
}

/// One `--screenshot` run: the file, its progress through the frames, and the
/// ending `main` exits with.
pub(crate) struct ScreenshotJob {
    /// Where the capture is written.
    path: PathBuf,
    /// The capture was asked for; it is never asked for twice.
    requested: bool,
    /// egui's clock when the capture was asked for, for [`CAPTURE_TIMEOUT`].
    requested_at: Option<f64>,
    /// The run reached its ending; every later one is ignored.
    ended: bool,
    /// The capture's ending, sent by the callback and read by the next frame.
    outcome: mpsc::Receiver<End>,
    /// The sending half of [`Self::outcome`], cloned into the callback.
    outcome_tx: mpsc::Sender<End>,
    /// Written when the ending is in; `main` reads it after eframe returns.
    exit: Arc<AtomicI32>,
}

impl ScreenshotJob {
    /// The job for `path`.  `exit` is shared with `main`, which reads it once
    /// the window has closed.
    pub(crate) fn new(path: PathBuf, exit: Arc<AtomicI32>) -> Self {
        let (outcome_tx, outcome) = mpsc::channel();
        Self {
            path,
            requested: false,
            requested_at: None,
            ended: false,
            outcome,
            outcome_tx,
            exit,
        }
    }

    /// Ends the run: the first ending wins, becomes the exit code and closes
    /// the window after this pass.  A capture that lands later is ignored.
    /// The process exits from `main`, never from a draw pass.
    fn end(&mut self, ctx: &egui::Context, end: End) {
        if self.ended {
            return;
        }
        self.ended = true;
        self.requested = true;
        self.requested_at = None;
        self.exit.store(end.exit_code(), Ordering::Release);
        ctx.send_viewport_cmd(ViewportCommand::Close);
    }
}

/// Writes one capture, unless an earlier ending already owns the run.  Returns
/// the ending to report, or `None` when the run had ended before the capture
/// landed, so no file is written that the run no longer reports.
fn save_capture(exit: &AtomicI32, path: &Path, image: &egui::ColorImage) -> Option<End> {
    if exit.load(Ordering::Acquire) != NO_ENDING {
        return None;
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = std::fs::create_dir_all(parent);
        }
    }
    let width = image.width() as u32;
    let height = image.height() as u32;
    let mut rgba = Vec::with_capacity(image.pixels.len() * 4);
    for pixel in &image.pixels {
        rgba.extend_from_slice(&[pixel.r(), pixel.g(), pixel.b(), pixel.a()]);
    }
    let saved = ::image::save_buffer(path, &rgba, width, height, ::image::ColorType::Rgba8);
    Some(match saved {
        Ok(()) => {
            println!("Screenshot saved to {}", path.display());
            End::Saved
        }
        Err(error) => {
            eprintln!("Could not save screenshot {}: {error}", path.display());
            End::SaveFailed
        }
    })
}

impl RigWerkApp {
    pub(crate) fn handle_screenshot(&mut self, ctx: &egui::Context) {
        // Wait until the first device inventory is in so the capture matches
        // a normal settled window, not the boot placeholder shell.
        if self.startup.is_some() {
            ctx.request_repaint();
            return;
        }
        self.frames += 1;
        if self.frames == 1 && self.screenshot.is_some() {
            self.apply_screenshot_routing();
            if std::env::var("RIGWERK_MATCH_PATHS").is_ok_and(|value| value.trim() == "1") {
                self.screenshot_match_paths();
            }
            self.open_sheet_for_screenshot();
            if screenshot_start() {
                self.start_bridge();
            }
        }
        let settle = if screenshot_start() {
            SETTLE_FRAMES_RUNNING
        } else {
            SETTLE_FRAMES
        };
        let Some(job) = &mut self.screenshot else {
            return;
        };
        if job.ended {
            // The first ending owns the run; a capture that lands after a
            // deadline is ignored.
            return;
        }
        // The capture callback reports its ending here, on the next frame.
        // The close keeps the exit code in `main`, after eframe has returned.
        if let Ok(end) = job.outcome.try_recv() {
            job.end(ctx, end);
            return;
        }
        if job
            .requested_at
            .is_some_and(|at| ctx.input(|input| input.time) - at > CAPTURE_TIMEOUT)
        {
            eprintln!("Screenshot capture did not arrive within {CAPTURE_TIMEOUT:.0} s.");
            job.end(ctx, End::Timeout);
            return;
        }
        // Re-assert size after the first frame (X11/XWayland). Wayland itself
        // ignores this; screenshot mode already forced the X11 backend.
        let size = screenshot_inner_size();
        if self.frames == 1 {
            ctx.send_viewport_cmd(ViewportCommand::InnerSize(size));
        }
        if self.frames >= settle && !job.requested {
            let viewport = ctx.viewport_rect();
            if (viewport.width() - size.x).abs() > 1.0 || (viewport.height() - size.y).abs() > 1.0 {
                eprintln!(
                    "Screenshot viewport is {:.0}x{:.0}, expected {:.0}x{:.0}. On Wayland, XWayland or Xvfb is required so the size can be fixed.",
                    viewport.width(),
                    viewport.height(),
                    size.x,
                    size.y
                );
                job.end(ctx, End::ViewportMismatch);
                return;
            }
            job.requested = true;
            job.requested_at = Some(ctx.input(|input| input.time));
            let path = job.path.clone();
            let outcome = job.outcome_tx.clone();
            let exit = Arc::clone(&job.exit);
            ctx.request_screenshot(move |image| {
                // A deadline may have ended the run while this capture was in
                // flight; the first ending wins, so this one is dropped
                // without writing a file the run no longer reports.
                if let Some(end) = save_capture(&exit, &path, &image) {
                    // Only a closed window drops the receiving end, and then
                    // there is no run left to report to.
                    let _ = outcome.send(end);
                }
            });
        }
        ctx.request_repaint();
    }
}

#[cfg(test)]
mod tests;
