//! The headless screenshot mode used by checks (`--screenshot <file>`).

use std::path::PathBuf;

use eframe::egui;
use egui::{Vec2, ViewportCommand};

use crate::app::RigWerkApp;

/// Inner size that fits Live (and side-by-side routing) without a scrollbar.
pub(crate) const SCREENSHOT_INNER_SIZE: Vec2 = Vec2::new(1440.0, 1080.0);

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
        }
        let Some(job) = &mut self.screenshot else {
            return;
        };
        // Re-assert size after the first frame (X11/XWayland). Wayland itself
        // ignores this; screenshot mode already forced the X11 backend.
        if self.frames == 1 {
            ctx.send_viewport_cmd(ViewportCommand::InnerSize(SCREENSHOT_INNER_SIZE));
        }
        if self.frames >= 120 && !job.requested {
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
    fn apply_screenshot_routing(&mut self) {
        let setup = std::env::var("RIGWERK_ROUTING_SETUP").unwrap_or_default();
        if !matches!(setup.as_str(), "drywet" | "sum3")
            || !self.apply_card(crate::app::cards::Card::DryWet)
        {
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
}
