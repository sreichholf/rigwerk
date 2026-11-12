//! The eframe shell: the window frame, its modals, and the panic-mute chord.
//!
//! `logic` and `save` only forward into `app`.  Their bodies stay there.
//! Style is applied on the first pass here, so `app` does not import `ui`.

use std::sync::Once;

use eframe::egui;
use egui::{Align2, Margin, RichText, Vec2};

use crate::app::screenshot;
use crate::app::{Page, RigWerkApp};
use crate::ui::theme::ts;
use crate::ui::theme::{
    configure_style, with_alpha, BG, BORDER, CARD, DIM, GREEN, MUTED, SIDEBAR, TEAL, TEXT,
};

/// Command and shift, and nothing else.  Command is Ctrl on Windows and
/// Linux, and Command on macOS.  `matches_exact` rejects Alt; egui's
/// `consume_key` would treat that extra modifier as optional.
const PANIC_MUTE_MODIFIERS: egui::Modifiers = egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT);

fn ensure_style(ctx: &egui::Context) {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| configure_style(ctx));
}

impl eframe::App for RigWerkApp {
    /// Non-painting maintenance.  eframe also calls this while the window is
    /// hidden or minimized, with no egui pass, so engine watchdogs, device
    /// watching, plugin main-thread/timer servicing and the Live Issues
    /// state keep running.  The timed repaint request at the end is what
    /// keeps those hidden passes coming: 16 ms while the bridge is live or
    /// a plugin editor is open, otherwise 200 ms.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ensure_style(ctx);
        let output_muted = self.publish_engine();
        self.smooth_meters(ctx.input(|input| input.time), output_muted);
        self.run_logic(ctx);
    }

    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ensure_style(root.ctx());
        // eframe builds this root before `logic`, copying `global_style`
        // into an `Arc`.  `configure_style` then copy-on-writes the context,
        // and `logic` may already have fired the `Once`, so the root would
        // keep egui defaults for the whole first pass.  Children clone this
        // `Arc`; point it at the style now on the context before any of them
        // are created.
        root.set_style(root.ctx().global_style());
        // Once per frame, before any page.  A control that only exists on
        // Live or Mixer would miss the chord everywhere else.
        self.handle_panic_mute(root.ctx());

        let ctx = root.ctx().clone();
        // Screenshots need a painted frame, so they stay on the drawing side.
        self.handle_screenshot(&ctx);

        egui::Panel::left("rigwerk_nav")
            .exact_size(224.0)
            .resizable(false)
            .show_separator_line(false)
            .frame(
                egui::Frame::new()
                    .fill(SIDEBAR)
                    .inner_margin(Margin::symmetric(16, 22)),
            )
            .show(root, |ui| {
                self.sidebar(ui);
            });

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BG).inner_margin(Margin {
                left: 26,
                right: 26,
                top: 20,
                bottom: 18,
            }))
            .show(root, |ui| {
                // Mixer and Plugins scroll inside their own panes and fill the
                // remaining height, so they stay outside the page ScrollArea.
                if matches!(self.page, Page::Mixer | Page::Plugins) {
                    self.header(ui);
                    ui.add_space(14.0);
                    if self.page == Page::Mixer {
                        self.mixer_page(ui);
                    } else {
                        self.plugins_page(ui);
                    }
                    return;
                }
                let mut page_scroll = egui::ScrollArea::vertical().auto_shrink([false, false]);
                if let Some(offset) = self
                    .screenshot
                    .as_ref()
                    .and_then(|_| screenshot::screenshot_scroll())
                {
                    page_scroll = page_scroll.vertical_scroll_offset(offset);
                }
                page_scroll.show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    self.header(ui);
                    // The tuner skips the usual 14 px header gap and the
                    // 18 px page bottom gap (it keeps 2 px); the panel's own
                    // 18 px bottom margin is unchanged.
                    if self.page != Page::Tuner {
                        ui.add_space(14.0);
                    }
                    match self.page {
                        Page::Live => self.live_page(ui, &ctx),
                        Page::Mixer => self.mixer_page(ui),
                        Page::Tuner => self.tuner_page(ui),
                        Page::Routing => self.routing_page(ui),
                        Page::Devices => self.devices_page(ui),
                        Page::Sessions => self.sessions_page(ui),
                        Page::Plugins => self.plugins_page(ui),
                        Page::Guide => self.guide_page(ui),
                        Page::Diagnostics => self.diagnostics_page(ui),
                    }
                    ui.add_space(if self.page == Page::Tuner { 2.0 } else { 18.0 });
                });
            });

        if self.show_about {
            // Fixed outer size so the window frame (and its right border) stays
            // fully on-screen; inner content is capped below that.
            egui::Window::new("About RigWerk")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
                .fixed_size(Vec2::new(420.0, 340.0))
                .frame(
                    egui::Frame::new()
                        .fill(CARD)
                        .stroke(egui::Stroke::new(1.0, BORDER))
                        .corner_radius(egui::CornerRadius::same(12))
                        .inner_margin(Margin::same(16)),
                )
                .show(&ctx, |ui| {
                    ui.set_max_width(388.0);
                    ui.heading("RigWerk");
                    ui.label(
                        RichText::new(
                            "An independent-clock audio bridge. Capture and playback keep their own rates; a short ring absorbs the drift. Extra outputs can follow the finished mix.",
                        )
                        .color(MUTED),
                    );
                    ui.add_space(10.0);
                    ui.label(
                        RichText::new(
                            "Windows: ASIO and WASAPI (DirectSound and MME when the host lists them). Mixer can add WASAPI followers; Publish uses an installed virtual cable such as VB-Audio Cable. Linux: PipeWire is the low-latency path (own nodes linked to the source and sink you pick), with direct ALSA hw: available and PulseAudio on the system side. PipeWire clocks can add follower sinks and RigWerk Out. The ALSA period is compared with the PipeWire quantum instead of hiding a mismatch.",
                        )
                        .size(ts(10.5))
                        .color(GREEN),
                    );
                    ui.label(
                        RichText::new(
                            "Pick any listed input and output. Whether one interface can do both jobs at once depends on its driver; RigWerk reports what that driver allows instead of guessing.",
                        )
                        .size(ts(10.0))
                        .color(MUTED),
                    );
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(format!(
                            "Version {} · preview build",
                            env!("CARGO_PKG_VERSION")
                        ))
                        .color(DIM),
                    );
                    ui.add_space(8.0);
                    if ui
                        .add(
                            egui::Button::new(RichText::new("Close").color(TEAL))
                                .fill(with_alpha(TEAL, 14))
                                .stroke(egui::Stroke::new(1.0, with_alpha(TEAL, 45)))
                                .corner_radius(egui::CornerRadius::same(7)),
                        )
                        .clicked()
                    {
                        self.show_about = false;
                    }
                });
        }

        self.plugin_editor_window(&ctx);
        self.calibration_window(&ctx);

        if self.startup.is_some() {
            egui::Window::new("Starting RigWerk")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
                .fixed_size(Vec2::new(360.0, 120.0))
                .frame(
                    egui::Frame::new()
                        .fill(CARD)
                        .stroke(egui::Stroke::new(1.0, BORDER))
                        .corner_radius(egui::CornerRadius::same(12))
                        .inner_margin(Margin::same(18)),
                )
                .show(&ctx, |ui| {
                    ui.label(
                        RichText::new("Finding audio devices…")
                            .size(ts(14.0))
                            .color(TEXT),
                    );
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(
                            "The window is ready. Device lists fill in as soon as the scan finishes.",
                        )
                        .size(ts(11.0))
                        .color(MUTED),
                    );
                });
        }

        if let Some(message) = self.device_alert.clone() {
            egui::Window::new("Audio device disconnected")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
                .show(&ctx, |ui| {
                    ui.set_width(380.0);
                    ui.label(RichText::new(message).size(ts(12.5)).color(TEXT));
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(
                            "Plug the device back in (or pick another one on the Devices page), then start the bridge again.",
                        )
                        .size(ts(10.5))
                        .color(MUTED),
                    );
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if ui.button("Rescan devices").clicked() {
                            self.rescan();
                        }
                        if ui.button("Dismiss").clicked() {
                            self.device_alert = None;
                        }
                    });
                });
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        self.save_to(storage);
    }
}

impl RigWerkApp {
    /// Ctrl+Shift+M toggles the master mute, the same switch as the mute
    /// button (`self.mute`; the next `publish_engine` pass publishes it, and the
    /// mixer ramps).  On macOS the chord is Command+Shift+M.  A second press
    /// unmutes.  A text edit with focus keeps the key, and any modifier
    /// other than command and shift (Alt, for one) does not count.  A held
    /// key repeats; only the first press toggles.
    pub(crate) fn handle_panic_mute(&mut self, ctx: &egui::Context) {
        if ctx.text_edit_focused() {
            return;
        }
        let pressed = ctx.input_mut(|input| {
            let mut pressed = false;
            input.events.retain(|event| {
                let egui::Event::Key {
                    key,
                    pressed: down,
                    repeat,
                    modifiers,
                    ..
                } = event
                else {
                    return true;
                };
                if *key != egui::Key::M || !*down || !modifiers.matches_exact(PANIC_MUTE_MODIFIERS)
                {
                    return true;
                }
                if !*repeat {
                    pressed = true;
                }
                false
            });
            pressed
        });
        if pressed {
            self.mute = !self.mute;
        }
    }
}
