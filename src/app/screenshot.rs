//! The headless screenshot mode used by checks (`--screenshot <file>`).

use std::path::PathBuf;

use eframe::egui;

use crate::app::RigWerkApp;

pub(crate) struct ScreenshotJob {
    pub(crate) path: PathBuf,
    pub(crate) requested: bool,
}

impl RigWerkApp {
    pub(crate) fn handle_screenshot(&mut self, ctx: &egui::Context) {
        self.frames += 1;
        let Some(job) = &mut self.screenshot else {
            return;
        };
        if self.frames >= 120 && !job.requested {
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
}
