//! The headless screenshot mode used by checks (`--screenshot <file>`).

use std::path::PathBuf;

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

pub(crate) struct ScreenshotJob {
    pub(crate) path: PathBuf,
    pub(crate) requested: bool,
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
            // A setup that loads effects selects the last one; the stage a
            // run asked for wins.
            if let Some(stage) = super::initial_mixer_stage() {
                self.select_chain(stage);
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
                std::process::exit(1);
            }
            job.requested = true;
            let path = job.path.clone();
            ctx.request_screenshot(move |image| {
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
                match ::image::save_buffer(&path, &rgba, width, height, ::image::ColorType::Rgba8) {
                    Ok(()) => {
                        println!("Screenshot saved to {}", path.display());
                        std::process::exit(0);
                    }
                    Err(error) => {
                        eprintln!("Could not save screenshot {}: {error}", path.display());
                        std::process::exit(1);
                    }
                }
            });
        }
        ctx.request_repaint();
    }

    /// Test hook for screenshots of Signal Routing:
    /// `RIGWERK_ROUTING_SETUP=drywet` applies the Dry L / Wet R layout;
    /// `sum3` adds a third path (a copy of the first, playing) on top of it,
    /// routed L/R on the clock output, so two paths add up there.
    /// `blend` applies Blend clean with 128 samples of (fake) plugin
    /// latency on the driven path, aligned; `dualamp-off` applies Dual-amp
    /// with 64 samples on the second amp and the clock output's alignment
    /// off (the red warning); `live3` is `blend` plus an empty third path
    /// (unmuted, routed nowhere: a Live Issues warning), for the Live page.
    /// `showcase` is the README setup: see [`Self::apply_screenshot_showcase`].
    fn apply_screenshot_routing(&mut self) {
        use crate::app::layouts::PathLayout;
        let setup = std::env::var("RIGWERK_ROUTING_SETUP").unwrap_or_default();
        match setup.as_str() {
            "drywet" | "sum3" => {
                if !self.apply_layout(PathLayout::DryWet) {
                    return;
                }
                if setup == "sum3" {
                    let first = self.path_slots()[0];
                    if let Some(copy) = self.duplicate_path(first) {
                        self.set_path_mute(copy, false);
                        self.clock_output.path_map[copy].map = crate::audio::ChannelMap::default();
                        self.select_path(first);
                    }
                }
            }
            "blend" | "live3" => {
                if !self.apply_layout(PathLayout::Blend) {
                    return;
                }
                let slots = self.path_slots();
                self.rename_path(slots[0], "Clean");
                self.rename_path(slots[1], "Drive");
                self.engine.screenshot_path_latency(slots[1], 128);
                if setup == "live3" {
                    if let Some(added) = self.add_path() {
                        self.rename_path(added, "Stream");
                        // Unmuted: routed nowhere, so Live Issues warns.
                        self.set_path_mute(added, false);
                    }
                    self.select_path(slots[0]);
                }
            }
            "showcase" => self.apply_screenshot_showcase(),
            "dualamp-off" => {
                if !self.apply_layout(PathLayout::DualAmp) {
                    return;
                }
                let slots = self.path_slots();
                self.rename_path(slots[0], "Amp A");
                self.rename_path(slots[1], "Amp B");
                self.engine.screenshot_path_latency(slots[1], 64);
                self.set_device_align(0, false);
            }
            _ => {}
        }
    }

    /// The README setup, the same on every page: a second output sink as a
    /// extra output plus RigWerk Out, Blend clean (Clean and Room), and the
    /// effects in `RIGWERK_SCREENSHOT_PLUGINS`, a `;` list of
    /// `<path number>=<bundle>[#<effect name>]` (path 1 is Clean).  Room
    /// stays selected.  A chain without plugin latency gets none faked.
    /// The plugin library is scanned too, so the Plugins page has a list.
    /// The metronome runs at 96 BPM in 4/4, clicking on the extra output only,
    /// so the Mixer and Live show it in use.
    fn apply_screenshot_showcase(&mut self) {
        self.begin_plugin_scan();
        let extra_output = self
            .inventory
            .outputs
            .iter()
            .map(|device| device.id.clone())
            .find(|id| *id != self.selected_output().id && !id.starts_with("vbcable:"));
        if let Some(id) = extra_output {
            self.add_extra_output_device(&id);
            // Device 1 is that extra output: the click goes to the headphones.
            self.set_click_channel(1, 0, true);
            self.set_click_channel(1, 1, true);
        }
        self.set_metronome_bpm(96.0);
        self.set_metronome_rhythm(crate::audio::ClickRhythm::FourFour);
        self.set_metronome_running(true);
        self.add_extra_output_device(crate::audio::VIRTUAL_OUTPUT_ID);
        if !self.apply_layout(crate::app::layouts::PathLayout::Blend) {
            return;
        }
        let slots = self.path_slots();
        self.rename_path(slots[0], "Clean");
        self.rename_path(slots[1], "Room");
        let list = std::env::var("RIGWERK_SCREENSHOT_PLUGINS").unwrap_or_default();
        for entry in list
            .split(';')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
        {
            let Some((number, bundle)) = entry.split_once('=') else {
                eprintln!("Screenshot plugin entry without a path number: {entry}");
                continue;
            };
            let Some(&slot) = number
                .trim()
                .parse::<usize>()
                .ok()
                .and_then(|number| slots.get(number.wrapping_sub(1)))
            else {
                eprintln!("Screenshot plugin entry for a path that is not there: {entry}");
                continue;
            };
            let (bundle, name) = match bundle.split_once('#') {
                Some((bundle, name)) => (bundle, Some(name.trim())),
                None => (bundle, None),
            };
            self.select_path(slot);
            if !self.add_plugin_from_bundle(std::path::Path::new(bundle.trim()), name) {
                eprintln!("Screenshot plugin not loaded: {entry}");
            }
        }
        self.select_path(slots[1]);
    }
}

#[cfg(test)]
mod tests;
