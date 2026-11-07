//! The headless screenshot mode used by checks (`--screenshot <file>`).

use std::path::PathBuf;

use eframe::egui;
use egui::{Vec2, ViewportCommand};

use crate::app::RigWerkApp;

/// Inner size that fits Live (and side-by-side routing) without a scrollbar.
pub(crate) const SCREENSHOT_INNER_SIZE: Vec2 = Vec2::new(1440.0, 1080.0);

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
/// the meters, the scope and the follower links have settled.
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
        if self.frames == 1 {
            ctx.send_viewport_cmd(ViewportCommand::InnerSize(SCREENSHOT_INNER_SIZE));
        }
        if self.frames >= settle && !job.requested {
            let viewport = ctx.viewport_rect();
            if (viewport.width() - SCREENSHOT_INNER_SIZE.x).abs() > 1.0
                || (viewport.height() - SCREENSHOT_INNER_SIZE.y).abs() > 1.0
            {
                eprintln!(
                    "Screenshot viewport is {:.0}x{:.0}, expected {:.0}x{:.0}. On Wayland, XWayland or Xvfb is required so the size can be fixed.",
                    viewport.width(),
                    viewport.height(),
                    SCREENSHOT_INNER_SIZE.x,
                    SCREENSHOT_INNER_SIZE.y
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
    /// `RIGWERK_ROUTING_SETUP=drywet` applies the Dry L / Wet R card;
    /// `sum3` adds a third path (a copy of the first, playing) on top of it,
    /// routed L/R on the clock device, so two paths add up there.
    /// `blend` applies Blend clean with 128 samples of (fake) plugin
    /// latency on the driven path, aligned; `dualamp-off` applies Dual-amp
    /// with 64 samples on the second amp and the clock device's alignment
    /// off (the red warning); `live3` is `blend` plus an empty third path
    /// (unmuted, routed nowhere: a Live Issues warning), for the Live page.
    /// `showcase` is the README setup: see [`Self::apply_screenshot_showcase`].
    fn apply_screenshot_routing(&mut self) {
        use crate::app::cards::Card;
        let setup = std::env::var("RIGWERK_ROUTING_SETUP").unwrap_or_default();
        match setup.as_str() {
            "drywet" | "sum3" => {
                if !self.apply_card(Card::DryWet) {
                    return;
                }
                if setup == "sum3" {
                    let first = self.path_slots()[0];
                    if let Some(copy) = self.duplicate_path(first) {
                        self.set_path_mute(copy, false);
                        self.clock_device.path_map[copy].map = crate::audio::ChannelMap::default();
                        self.select_path(first);
                    }
                }
            }
            "blend" | "live3" => {
                if !self.apply_card(Card::Blend) {
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
                if !self.apply_card(Card::DualAmp) {
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
    /// follower plus RigWerk Out, Blend clean (Clean and Room), and the
    /// inserts in `RIGWERK_SCREENSHOT_PLUGINS`, a `;` list of
    /// `<path number>=<bundle>[#<effect name>]` (path 1 is Clean).  Room
    /// stays selected.  A chain without plugin latency gets none faked.
    /// The plugin library is scanned too, so the Plugins page has a list.
    fn apply_screenshot_showcase(&mut self) {
        self.begin_plugin_scan();
        let follower = self
            .inventory
            .outputs
            .iter()
            .map(|device| device.id.clone())
            .find(|id| *id != self.selected_output().id && !id.starts_with("vbcable:"));
        if let Some(id) = follower {
            self.add_follower_device(&id);
        }
        self.add_follower_device(crate::audio::VIRTUAL_OUT_ID);
        if !self.apply_card(crate::app::cards::Card::Blend) {
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
