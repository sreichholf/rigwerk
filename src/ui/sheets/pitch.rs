//! The pitch sheet (`docs/ui-spec.md` §6.1): semitones, cents and wet, the
//! window, and the delay that window adds.
//!
//! The header above (`sheets.rs`) already holds *On* / *Bypass*, so *Reset*
//! keeps it.  The stage sits before the pre EQ, so a drop tuning is what
//! the amp hears.  It runs at the buffer the bridge already uses and adds
//! half the window.

use eframe::egui;
use egui::{Align, Layout, RichText, Vec2};

use crate::app::screen::format_ms;
use crate::app::{LogTone, RigWerkApp};
use crate::audio::{
    format_cents, format_semitones, PitchSettings, PitchWindow, PITCH_MAX_CENTS,
    PITCH_MAX_SEMITONES, PITCH_MIN_CENTS, PITCH_MIN_SEMITONES,
};
use crate::ui::fonts::Face;
use crate::ui::theme::{font, palette, Palette, TypeRole};
use crate::ui::widgets::{
    quiet_heading, segmented_hinted, styled_slider, OutlineButton, SliderStyle, CONTROL_H,
};

const LABEL_W: f32 = 96.0;
const VALUE_W: f32 = 88.0;
const ROW_H: f32 = 28.0;

const RESET_HINT: &str = "Restore the interval, wet and window to the defaults. On/off is kept.";

const SUMMARY: &str = "Before the amp · adds half the window · the device buffer stays as it is";

const WINDOW_HINTS: [&str; 3] = [
    "16 ms window, 8 ms of latency. The tightest, and the roughest on chords.",
    "32 ms window, 16 ms of latency.",
    "64 ms window, 32 ms of latency. Smoother on chords.",
];

/// `pitch` back to the defaults, keeping on/off.
pub(crate) fn reset_pitch(pitch: PitchSettings) -> PitchSettings {
    PitchSettings {
        enabled: pitch.enabled,
        ..PitchSettings::default()
    }
}

impl RigWerkApp {
    /// The pitch sheet body for path `path`.
    pub(crate) fn pitch_sheet(&mut self, ui: &mut egui::Ui, path: usize) {
        let Some(signal) = self.paths.get(path).and_then(Option::as_ref) else {
            return;
        };
        let current = signal.mix.pitch;
        let colours = palette(ui.ctx());
        let mut pitch = current;
        let mut reset = false;
        let rate = match self.snapshot.output_rate {
            0 => self.sample_rate.max(1),
            rate => rate,
        };

        ui.horizontal(|ui| {
            ui.set_min_height(CONTROL_H);
            ui.label(
                RichText::new(SUMMARY)
                    .font(font(Face::Sans, TypeRole::Secondary))
                    .color(colours.text3),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if OutlineButton::new("Reset")
                    .show(ui)
                    .on_hover_text(RESET_HINT)
                    .clicked()
                {
                    pitch = reset_pitch(pitch);
                    reset = true;
                }
            });
        });
        ui.add_space(8.0);

        let width = ui.available_width().min(720.0);
        slider(
            ui,
            "Semitones",
            "Interval. Negative is down, which is the drop-tuning direction.",
            &mut pitch.semitones,
            PITCH_MIN_SEMITONES,
            PITCH_MAX_SEMITONES,
            1.0,
            format_semitones,
            width,
            colours,
        );
        slider(
            ui,
            "Cents",
            "Fine interval on top of the semitones, −100 to +100.",
            &mut pitch.cents,
            PITCH_MIN_CENTS,
            PITCH_MAX_CENTS,
            1.0,
            format_cents,
            width,
            colours,
        );
        let mut wet = pitch.wet * 100.0;
        slider(
            ui,
            "Wet",
            "How much of the shifted signal replaces the dry one. The dry side is delayed by the same amount.",
            &mut wet,
            0.0,
            100.0,
            1.0,
            format_wet,
            width,
            colours,
        );
        pitch.wet = wet / 100.0;

        ui.add_space(12.0);
        quiet_heading(ui, "Window", colours);
        let mut choice = match pitch.window {
            PitchWindow::Tight => 0,
            PitchWindow::Normal => 1,
            PitchWindow::Smooth => 2,
        };
        if segmented_hinted(
            ui,
            &mut choice,
            &["Tight", "Normal", "Smooth"],
            &WINDOW_HINTS,
        )
        .changed()
        {
            pitch.window = match choice {
                0 => PitchWindow::Tight,
                2 => PitchWindow::Smooth,
                _ => PitchWindow::Normal,
            };
        }
        ui.add_space(8.0);
        let latency = pitch.latency_samples(rate);
        let latency = if latency == 0 {
            "Bypassed · no added latency".to_owned()
        } else {
            format!("{} added", format_ms(latency as f32 * 1000.0 / rate as f32))
        };
        ui.label(
            RichText::new(latency)
                .font(font(Face::Mono, TypeRole::Secondary))
                .color(colours.text2),
        );

        if pitch != current {
            self.select_path(path);
            self.apply_pitch(current, pitch, reset);
        }
    }

    /// Sanitises `pitch`, notes a reset or on/off in the log, and sends it
    /// to the selected path and the engine.
    pub(crate) fn apply_pitch(
        &mut self,
        current: PitchSettings,
        pitch: PitchSettings,
        reset: bool,
    ) {
        let pitch = pitch.sanitized();
        if pitch == current {
            return;
        }
        if reset {
            self.push_log("Pitch reset to defaults.", LogTone::Amber);
        } else if pitch.enabled != current.enabled {
            self.push_log(
                if pitch.enabled {
                    "Pitch on."
                } else {
                    "Pitch bypassed."
                },
                LogTone::Amber,
            );
        } else if pitch.window != current.window {
            self.push_log(
                match pitch.window {
                    PitchWindow::Tight => "Pitch window set to tight.",
                    PitchWindow::Normal => "Pitch window set to normal.",
                    PitchWindow::Smooth => "Pitch window set to smooth.",
                },
                LogTone::Amber,
            );
        }
        self.set_path_pitch(pitch);
        self.push_paths_to_engine();
    }
}

fn format_wet(percent: f32) -> String {
    format!("{percent:.0} %")
}

#[allow(clippy::too_many_arguments)]
fn slider(
    ui: &mut egui::Ui,
    name: &str,
    hint: &str,
    value: &mut f32,
    min: f32,
    max: f32,
    step: f32,
    format: fn(f32) -> String,
    width: f32,
    colours: &Palette,
) {
    let slider_w = (width - LABEL_W - VALUE_W - 16.0).max(80.0);
    ui.allocate_ui_with_layout(
        Vec2::new(width, ROW_H),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.set_height(ROW_H);
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.allocate_ui_with_layout(
                Vec2::new(LABEL_W, ROW_H),
                Layout::left_to_right(Align::Center),
                |ui| {
                    ui.set_width(LABEL_W);
                    ui.label(
                        RichText::new(name)
                            .font(font(Face::Sans, TypeRole::Body))
                            .color(colours.text2),
                    )
                    .on_hover_text(hint);
                },
            );
            styled_slider(
                ui,
                value,
                min,
                max,
                step,
                SliderStyle::named(colours.accent, name)
                    .format(format)
                    .width(slider_w),
            )
            .on_hover_text(hint);
            let text = format(*value);
            ui.allocate_ui_with_layout(
                Vec2::new(VALUE_W, ROW_H),
                Layout::right_to_left(Align::Center),
                |ui| {
                    ui.set_width(VALUE_W);
                    ui.label(
                        RichText::new(text)
                            .font(font(Face::Mono, TypeRole::Body))
                            .color(colours.text),
                    );
                },
            );
        },
    );
}
