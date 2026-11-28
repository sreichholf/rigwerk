//! The pitch sheet.  Off takes the stage and its latency out; bypass plays
//! dry at the same delay and keeps the stage.  [`RigWerkApp::apply_pitch`]
//! clamps; the widgets do not.

use eframe::egui;
use egui::{Align, Layout, RichText, Vec2};

use crate::app::screen::format_ms;
use crate::app::{LogTone, RigWerkApp};
use crate::audio::{
    PitchEngine, PitchMode, PitchSettings, PITCH_MAX_CENTS, PITCH_MAX_DELAY_MS,
    PITCH_MAX_SEMITONES, PITCH_MIN_CENTS, PITCH_MIN_DELAY_MS, PITCH_MIN_SEMITONES,
};
use crate::ui::fonts::Face;
use crate::ui::theme::{font, palette, Palette, TypeRole};
use crate::ui::widgets::{segmented_hinted, styled_slider, OutlineButton, SliderStyle, CONTROL_H};

const LABEL_W: f32 = 96.0;
const VALUE_W: f32 = 72.0;
const ROW_H: f32 = 28.0;
/// The sliders' column at most; wider only stretches the tracks.
const MAX_CONTROLS_W: f32 = 640.0;

pub(crate) const RESET_HINT: &str =
    "Restore the shift, wet, delay, engine and mode to the defaults. On, Bypass and Off are kept.";

pub(crate) const USE_HINT: &str = "Attacks stay intact. Best for single notes and power chords; \
     full chords chorus, less with a longer delay.";
const SPECTRAL_USE_HINT: &str = "Keeps every chord clean. Attacks soften and arrive a little \
     later; a longer mode resolves lower notes.";

pub(crate) fn summary(latency: &str) -> String {
    format!("First in the chain · sums a stereo source to mono · the path waits {latency}")
}

const SEMITONES_HINT: &str =
    "The shift in semitones, −12 to +12. −2 tunes a guitar down a whole step.";
const CENTS_HINT: &str = "A fine shift on top of the semitones, −50 to +50 cents.";
const WET_HINT: &str = "How much of the shifted signal plays with the dry signal. \
     100 % (default) is the shifted signal only; less doubles the note, e.g. an octave down.";
const ENGINE_HINT: &str = "Tight for single notes and power chords, Spectral for full chords.";
const ENGINE_NAMES: [&str; 2] = ["Tight", "Spectral"];
const ENGINE_HINTS: [&str; 2] = [
    "Tight (default): low latency, attacks intact. Full chords chorus.",
    "Spectral: keeps every chord clean. Its latency is the mode's.",
];
const MODE_HINT: &str = "Spectral's resolution. Longer resolves lower notes and waits longer.";
const MODE_NAMES: [&str; 4] = ["Fast", "Balanced", "Quality", "Ultra"];
const MODE_HINTS: [&str; 4] = [
    "Fast: the shortest delay. Resolves partials about 31 Hz apart on a held note.",
    "Balanced (default): resolves partials about 23 Hz apart on a held note.",
    "Quality: the best octave down. Resolves partials about 23 Hz apart.",
    "Ultra: the longest delay, about 40 ms. Resolves partials about 12 Hz apart; \
     attacks smear the most.",
];
pub(crate) const DELAY_HINT: &str =
    "Shorter for single notes and power chords: tight and immediate. \
     Longer for full chords: smoother, but you hear every note that much later.";

/// The settings' engine.  Destructured, so the UI's `.engine` scan
/// (`src/tests.rs`) keeps meaning the app's engine field.
pub(crate) fn engine_of(pitch: PitchSettings) -> PitchEngine {
    let PitchSettings { engine, .. } = pitch;
    engine
}

/// What the engine is called in the UI.
pub(crate) fn engine_name(engine: PitchEngine) -> &'static str {
    ENGINE_NAMES[engine_index(engine)]
}

fn engine_index(engine: PitchEngine) -> usize {
    PitchEngine::ALL
        .iter()
        .position(|&e| e == engine)
        .unwrap_or(0)
}

/// What the mode is called in the UI.
pub(crate) fn mode_name(mode: PitchMode) -> &'static str {
    MODE_NAMES[mode_index(mode)]
}

fn mode_index(mode: PitchMode) -> usize {
    PitchMode::ALL.iter().position(|&m| m == mode).unwrap_or(1)
}

/// The header's three states, in their order.
pub(crate) const STATES: [&str; 3] = ["On", "Bypass", "Off"];
const STATE_HINTS: [&str; 3] = [
    "On: shifts the pitch. The path waits for its latency.",
    "Bypass: plays the dry signal at the same delay, so the paths stay aligned.",
    "Off: out of the chain, with no latency.",
];

/// The header's choice for `pitch`: 0 on, 1 bypassed, 2 off.
pub(crate) fn state_index(pitch: PitchSettings) -> usize {
    match (pitch.enabled, pitch.bypass) {
        (true, false) => 0,
        (true, true) => 1,
        (false, _) => 2,
    }
}

/// `pitch` with the header's choice `index` applied.  Off keeps the
/// bypass, so the stage returns to it; on and bypass set both.
pub(crate) fn with_state(pitch: PitchSettings, index: usize) -> PitchSettings {
    match index {
        0 => PitchSettings {
            enabled: true,
            bypass: false,
            ..pitch
        },
        1 => PitchSettings {
            enabled: true,
            bypass: true,
            ..pitch
        },
        _ => PitchSettings {
            enabled: false,
            ..pitch
        },
    }
}

/// *On* / *Bypass* / *Off* for the sheet header.  The new settings when the
/// choice moved.
pub(crate) fn state_switch(ui: &mut egui::Ui, pitch: PitchSettings) -> Option<PitchSettings> {
    let mut choice = state_index(pitch);
    segmented_hinted(ui, &mut choice, &STATES, &STATE_HINTS)
        .changed()
        .then(|| with_state(pitch, choice))
}

/// A sign for a shift: the real minus, `+` above zero, none at zero.
fn sign(value: f32) -> &'static str {
    if value < 0.0 {
        "−"
    } else if value > 0.0 {
        "+"
    } else {
        ""
    }
}

/// `−2 st`, `0 st`, `+7 st`.
pub(crate) fn format_semitones(semitones: i8) -> String {
    format!(
        "{}{} st",
        sign(f32::from(semitones)),
        semitones.unsigned_abs()
    )
}

/// `−10 ct`, `0 ct`, `+25 ct`, whole cents.
pub(crate) fn format_cents(cents: f32) -> String {
    let cents = cents.round();
    format!("{}{:.0} ct", sign(cents), cents.abs())
}

/// `100 %`.
fn format_wet(wet: f32) -> String {
    format!("{:.0} %", wet * 100.0)
}

/// `4 ms`: the delay is set in whole ms.
fn format_delay(delay_ms: f32) -> String {
    format!("{delay_ms:.0} ms")
}

/// The chain row's mark: the shift (`−2 st`, `−1.95 st` with cents) while
/// on, *Bypass* while bypassed, *Off* while off.
pub(crate) fn row_mark(pitch: PitchSettings) -> String {
    if !pitch.enabled {
        return "Off".to_owned();
    }
    if pitch.bypass {
        return "Bypass".to_owned();
    }
    let cents = pitch.cents.round();
    if cents == 0.0 {
        return format_semitones(pitch.semitones);
    }
    let shift = f32::from(pitch.semitones) + cents / 100.0;
    format!("{}{:.2} st", sign(shift), shift.abs())
}

/// `pitch` back to the defaults, keeping on, bypass and off.
pub(crate) fn reset_pitch(pitch: PitchSettings) -> PitchSettings {
    PitchSettings {
        enabled: pitch.enabled,
        bypass: pitch.bypass,
        ..PitchSettings::default()
    }
}

impl RigWerkApp {
    pub(crate) fn pitch_sheet(&mut self, ui: &mut egui::Ui, path: usize) {
        let Some(signal) = self.paths.get(path).and_then(Option::as_ref) else {
            return;
        };
        let current = signal.mix.pitch;
        let latency = format_ms(self.pitch_latency_ms(current));
        let summary = summary(&latency);
        let colours = palette(ui.ctx());
        let mut pitch = current;
        let mut reset = false;

        ui.horizontal(|ui| {
            ui.set_min_height(CONTROL_H);
            ui.label(
                RichText::new(summary)
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

        ui.vertical(|ui| {
            ui.set_width(ui.available_width().min(MAX_CONTROLS_W));
            controls(ui, &mut pitch, colours, path, &latency);
            ui.add_space(6.0);
            let use_hint = match engine_of(current) {
                PitchEngine::Tight => USE_HINT,
                PitchEngine::Spectral => SPECTRAL_USE_HINT,
            };
            hint(ui, use_hint, colours);
        });

        if pitch != current {
            self.select_path(path);
            self.apply_pitch(current, pitch, reset);
        }
    }

    /// Clamps `pitch`, notes a reset or a switch in the log, and sends it
    /// to the selected path and the engine.  The sheet and its header
    /// commit here.
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
        } else if state_index(pitch) != state_index(current) {
            self.push_log(
                match state_index(pitch) {
                    0 => "Pitch on.",
                    1 => "Pitch bypassed.",
                    _ => "Pitch off.",
                },
                LogTone::Amber,
            );
        } else if engine_of(pitch) != engine_of(current) {
            self.push_log(
                format!("Pitch engine: {}.", engine_name(engine_of(pitch))),
                LogTone::Amber,
            );
        } else if pitch.mode != current.mode {
            self.push_log(
                format!("Pitch mode: {}.", mode_name(pitch.mode)),
                LogTone::Amber,
            );
        }
        self.set_path_pitch(pitch);
        self.push_paths_to_engine();
    }
}

fn row(
    ui: &mut egui::Ui,
    name: &str,
    hint: &str,
    value: &str,
    colours: &Palette,
    add: impl FnOnce(&mut egui::Ui, f32),
) {
    let width = ui.available_width();
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
            add(ui, (width - LABEL_W - VALUE_W - 16.0).max(80.0));
            ui.allocate_ui_with_layout(
                Vec2::new(VALUE_W, ROW_H),
                Layout::right_to_left(Align::Center),
                |ui| {
                    ui.set_width(VALUE_W);
                    ui.label(
                        RichText::new(value)
                            .font(font(Face::Mono, TypeRole::Body))
                            .color(colours.text),
                    );
                },
            );
        },
    );
}

fn controls(
    ui: &mut egui::Ui,
    pitch: &mut PitchSettings,
    colours: &Palette,
    path: usize,
    latency: &str,
) {
    ui.spacing_mut().item_spacing.y = 4.0;
    let semitones_text = format_semitones(pitch.semitones);
    row(
        ui,
        "Semitones",
        SEMITONES_HINT,
        &semitones_text,
        colours,
        |ui, width| {
            let buttons_w = 2.0 * (CONTROL_H + 8.0);
            let mut value = f32::from(pitch.semitones);
            let style = SliderStyle::named(colours.accent, "Semitones")
                .value(semitones_text.clone())
                .width((width - buttons_w).max(80.0));
            styled_slider(
                ui,
                &mut value,
                f32::from(PITCH_MIN_SEMITONES),
                f32::from(PITCH_MAX_SEMITONES),
                1.0,
                style,
            )
            .on_hover_text(SEMITONES_HINT);
            let mut semitones = value.round() as i8;
            if OutlineButton::new("−")
                .name("Semitone down")
                .enabled(semitones > PITCH_MIN_SEMITONES)
                .show(ui)
                .on_hover_text("Semitone down")
                .clicked()
            {
                semitones -= 1;
            }
            if OutlineButton::new("+")
                .name("Semitone up")
                .enabled(semitones < PITCH_MAX_SEMITONES)
                .show(ui)
                .on_hover_text("Semitone up")
                .clicked()
            {
                semitones += 1;
            }
            pitch.semitones = semitones;
        },
    );
    let cents_text = format_cents(pitch.cents);
    row(
        ui,
        "Cents",
        CENTS_HINT,
        &cents_text,
        colours,
        |ui, width| {
            let style = SliderStyle::named(colours.accent, "Cents")
                .value(cents_text.clone())
                .width(width);
            styled_slider(
                ui,
                &mut pitch.cents,
                PITCH_MIN_CENTS,
                PITCH_MAX_CENTS,
                1.0,
                style,
            )
            .on_hover_text(CENTS_HINT);
        },
    );
    let wet_text = format_wet(pitch.wet);
    row(ui, "Wet", WET_HINT, &wet_text, colours, |ui, width| {
        let style = SliderStyle::named(colours.accent, "Wet")
            .value(wet_text.clone())
            .width(width);
        styled_slider(ui, &mut pitch.wet, 0.0, 1.0, 0.01, style).on_hover_text(WET_HINT);
    });
    let engine = engine_of(*pitch);
    let mut choice = engine_index(engine);
    let tight = engine == PitchEngine::Tight;
    let engine_value = if tight { "" } else { latency };
    row(ui, "Engine", ENGINE_HINT, engine_value, colours, |ui, _| {
        if segmented_hinted(ui, &mut choice, &ENGINE_NAMES, &ENGINE_HINTS).changed() {
            *pitch = PitchSettings {
                engine: PitchEngine::ALL[choice],
                ..*pitch
            };
        }
    });
    if !tight {
        let mut choice = mode_index(pitch.mode);
        row(ui, "Mode", MODE_HINT, "", colours, |ui, _| {
            if segmented_hinted(ui, &mut choice, &MODE_NAMES, &MODE_HINTS).changed() {
                pitch.mode = PitchMode::ALL[choice];
            }
        });
        return;
    }
    // Every delay change dips the path, so a drag applies only on release.
    // The draft is per path, and only the release of a drag (or a click or
    // key on the slider) commits: a sheet closed mid-drag leaves a draft
    // that is dropped, not committed, when the sheet opens again.
    let draft_id = egui::Id::unique(("pitch_delay_draft", path));
    let live = ui.ctx().dragged_id().is_some() || ui.input(|input| input.pointer.any_released());
    let mut delay_ms = ui
        .data(|data| data.get_temp::<f32>(draft_id))
        .filter(|_| live)
        .unwrap_or(pitch.delay_ms);
    let delay_text = format_delay(delay_ms);
    row(
        ui,
        "Delay",
        DELAY_HINT,
        &delay_text,
        colours,
        |ui, width| {
            let style = SliderStyle::named(colours.accent, "Delay")
                .value(delay_text.clone())
                .width(width);
            let response = styled_slider(
                ui,
                &mut delay_ms,
                PITCH_MIN_DELAY_MS,
                PITCH_MAX_DELAY_MS,
                1.0,
                style,
            )
            .on_hover_text(DELAY_HINT);
            if response.dragged() {
                ui.data_mut(|data| data.insert_temp(draft_id, delay_ms));
            } else {
                ui.data_mut(|data| data.remove::<f32>(draft_id));
                if response.drag_stopped() || response.changed() {
                    pitch.delay_ms = delay_ms;
                }
            }
        },
    );
}

fn hint(ui: &mut egui::Ui, text: &str, colours: &Palette) {
    ui.add(
        egui::Label::new(
            RichText::new(text)
                .font(font(Face::Sans, TypeRole::Secondary))
                .color(colours.text3),
        )
        .wrap(),
    );
}

#[cfg(test)]
mod tests;
