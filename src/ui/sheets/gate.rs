//! The gate sheet's body (`docs/ui-spec.md` §6.1): the six gate values as
//! sliders on the left; the position (*Before effects* / *After effects*),
//! what it listens to (*Gated signal* / *Dry input*) and the reduction
//! meter on the right; *Reset* above them.
//!
//! The header above (`sheets.rs`) already holds the gate's *On* /
//! *Bypass*, so *Reset* keeps it.  The classic Mixer's gate card draws the
//! same values from [`GATE_CONTROLS`] and commits through
//! [`RigWerkApp::apply_gate`]; neither copies the other's texts.  Moving
//! the gate also moves its chain row, which the console orders by the
//! gate's slot.

use eframe::egui;
use egui::{Align, CornerRadius, Layout, Rect, RichText, Sense, Vec2};

use crate::app::{LogTone, RigWerkApp};
use crate::audio::{
    GateKey, GateSettings, GateSlot, GATE_MAX_ATTACK_MS, GATE_MAX_HOLD_MS, GATE_MAX_HYSTERESIS_DB,
    GATE_MAX_RANGE_DB, GATE_MAX_RELEASE_MS, GATE_MAX_THRESHOLD_DB, GATE_MIN_ATTACK_MS,
    GATE_MIN_HOLD_MS, GATE_MIN_HYSTERESIS_DB, GATE_MIN_RANGE_DB, GATE_MIN_RELEASE_MS,
    GATE_MIN_THRESHOLD_DB,
};
use crate::ui::fonts::Face;
use crate::ui::theme::{font, palette, Palette, TypeRole};
use crate::ui::widgets::{
    format_db, quiet_heading, row_info, segmented, styled_slider, SliderStyle, CONTROL_H,
};

use crate::ui::widgets::OutlineButton;

const LABEL_W: f32 = 96.0;
const VALUE_W: f32 = 72.0;
const ROW_H: f32 = 28.0;
const COLUMN_GAP: f32 = 24.0;
/// The position and the meter.
const SIDE_W: f32 = 280.0;
/// The sliders' column at most; wider only stretches the tracks.
const MAX_CONTROLS_W: f32 = 640.0;
/// Below this the side column goes under the sliders.
const MIN_TWO_COLUMNS_W: f32 = 320.0 + COLUMN_GAP + SIDE_W;
const METER_H: f32 = 8.0;

pub(crate) const RESET_HINT: &str =
    "Restore threshold, range, timing, slot and key to the defaults. On/off is kept.";

pub(crate) const SUMMARY: &str =
    "Causal · no added latency · first transient can leak before it opens";

/// How a gate value reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GateUnit {
    Db,
    Ms,
}

impl GateUnit {
    /// `docs/ui-spec.md` §2.2: one decimal and the real minus for dB; one
    /// decimal below 10 ms, none from there.
    pub(crate) fn format(self, value: f32) -> String {
        match self {
            Self::Db => format_db(value, 1),
            Self::Ms if value < 10.0 => format!("{value:.1} ms"),
            Self::Ms => format!("{value:.0} ms"),
        }
    }
}

/// One of the gate's six values: its name, hover text, range and step.
pub(crate) struct GateControl {
    pub(crate) name: &'static str,
    pub(crate) hint: &'static str,
    pub(crate) min: f32,
    pub(crate) max: f32,
    pub(crate) step: f32,
    pub(crate) unit: GateUnit,
    /// The long times are easier to set on a log scale.
    logarithmic: bool,
    field: fn(&mut GateSettings) -> &mut f32,
}

impl GateControl {
    pub(crate) fn value<'a>(&self, gate: &'a mut GateSettings) -> &'a mut f32 {
        (self.field)(gate)
    }
}

/// Threshold, range, hysteresis, attack, release and hold, in that order.
pub(crate) const GATE_CONTROLS: [GateControl; 6] = [
    GateControl {
        name: "Threshold",
        hint: "Open threshold. Signal above this opens a closed gate.",
        min: GATE_MIN_THRESHOLD_DB,
        max: GATE_MAX_THRESHOLD_DB,
        step: 0.5,
        unit: GateUnit::Db,
        logarithmic: false,
        field: |gate| &mut gate.threshold_db,
    },
    GateControl {
        name: "Range",
        hint: "Gain when fully closed. 0 dB is pass-through; −80 dB is near mute.",
        min: GATE_MIN_RANGE_DB,
        max: GATE_MAX_RANGE_DB,
        step: 1.0,
        unit: GateUnit::Db,
        logarithmic: false,
        field: |gate| &mut gate.range_db,
    },
    GateControl {
        name: "Hysteresis",
        hint: "Close threshold sits this far below the open threshold. Stops chatter.",
        min: GATE_MIN_HYSTERESIS_DB,
        max: GATE_MAX_HYSTERESIS_DB,
        step: 0.5,
        unit: GateUnit::Db,
        logarithmic: false,
        field: |gate| &mut gate.hysteresis_db,
    },
    GateControl {
        name: "Attack",
        hint: "Gain time constant while opening (~63% of the remaining gap).",
        min: GATE_MIN_ATTACK_MS,
        max: GATE_MAX_ATTACK_MS,
        step: 0.1,
        unit: GateUnit::Ms,
        logarithmic: true,
        field: |gate| &mut gate.attack_ms,
    },
    GateControl {
        name: "Release",
        hint: "Gain time constant while closing.",
        min: GATE_MIN_RELEASE_MS,
        max: GATE_MAX_RELEASE_MS,
        step: 1.0,
        unit: GateUnit::Ms,
        logarithmic: true,
        field: |gate| &mut gate.release_ms,
    },
    GateControl {
        name: "Hold",
        hint: "Keep the gate open this long after the detector drops below the close threshold.",
        min: GATE_MIN_HOLD_MS,
        max: GATE_MAX_HOLD_MS,
        step: 1.0,
        unit: GateUnit::Ms,
        logarithmic: false,
        field: |gate| &mut gate.hold_ms,
    },
];

/// What each position is for.
pub(crate) fn slot_hint(slot: GateSlot) -> &'static str {
    match slot {
        GateSlot::Pre => "Before effects: gates single-coil / interface hiss into amp sims.",
        GateSlot::Post => "After effects (default): gates idle roar / gain-stage hiss.",
    }
}

/// What each key is for.
pub(crate) fn key_hint(key: GateKey) -> &'static str {
    match key {
        GateKey::Signal => "Gated signal (default): the gate hears what it cuts.",
        GateKey::Input => {
            "Dry input: the gate hears the path's input before the chain, lined up with \
             the effects' latency, so an amp's noise cannot hold it open. Set the \
             threshold for the guitar's own level."
        }
    }
}

/// `gate` back to the defaults, keeping on/off.
pub(crate) fn reset_gate(gate: GateSettings) -> GateSettings {
    GateSettings {
        enabled: gate.enabled,
        ..GateSettings::default()
    }
}

/// How full the reduction meter is: `reduction_db` of what `range_db`
/// can take away, 0–1.
fn reduction_fraction(reduction_db: f32, range_db: f32) -> f32 {
    if !reduction_db.is_finite() {
        return 0.0;
    }
    (reduction_db / range_db.abs().max(1.0)).clamp(0.0, 1.0)
}

impl RigWerkApp {
    /// The gate sheet body for path `path`.
    pub(crate) fn gate_sheet(&mut self, ui: &mut egui::Ui, path: usize) {
        let Some(signal) = self.paths.get(path).and_then(Option::as_ref) else {
            return;
        };
        let current = signal.mix.gate;
        let reduction = if current.enabled {
            self.gate_reduction_db(path)
        } else {
            0.0
        };
        let colours = palette(ui.ctx());
        let mut gate = current;
        let mut reset = false;

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
                    gate = reset_gate(gate);
                    reset = true;
                }
            });
        });
        ui.add_space(8.0);

        let width = ui.available_width();
        if width >= MIN_TWO_COLUMNS_W {
            let controls_w = (width - COLUMN_GAP - SIDE_W).min(MAX_CONTROLS_W);
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = COLUMN_GAP;
                ui.vertical(|ui| {
                    ui.set_width(controls_w);
                    controls(ui, &mut gate, colours);
                });
                ui.vertical(|ui| {
                    ui.set_width(SIDE_W);
                    side(ui, &mut gate, reduction, colours);
                });
            });
        } else {
            controls(ui, &mut gate, colours);
            ui.add_space(12.0);
            side(ui, &mut gate, reduction, colours);
        }

        if gate != current {
            self.select_path(path);
            self.apply_gate(current, gate, reset);
        }
    }

    /// Sanitises `gate`, notes a reset or a move in the log, and sends it
    /// to the selected path and the engine.  The sheet and the classic
    /// card both commit here.
    pub(crate) fn apply_gate(&mut self, current: GateSettings, gate: GateSettings, reset: bool) {
        let gate = gate.sanitized();
        if gate == current {
            return;
        }
        if reset {
            self.push_log("Gate reset to defaults.", LogTone::Amber);
        } else if gate.enabled != current.enabled {
            self.push_log(
                if gate.enabled {
                    "Gate on."
                } else {
                    "Gate bypassed."
                },
                LogTone::Amber,
            );
        } else if gate.slot != current.slot {
            self.push_log(
                match gate.slot {
                    GateSlot::Pre => "Gate moved before effects.",
                    GateSlot::Post => "Gate moved after effects.",
                },
                LogTone::Amber,
            );
        } else if gate.key != current.key {
            self.push_log(
                match gate.key {
                    GateKey::Signal => "Gate listens to the gated signal.",
                    GateKey::Input => "Gate listens to the dry input.",
                },
                LogTone::Amber,
            );
        }
        self.set_path_gate(gate);
        self.push_paths_to_engine();
    }
}

/// The six sliders, one row each: name, track, value.
fn controls(ui: &mut egui::Ui, gate: &mut GateSettings, colours: &Palette) {
    let width = ui.available_width();
    let slider_w = (width - LABEL_W - VALUE_W - 16.0).max(80.0);
    ui.spacing_mut().item_spacing.y = 4.0;
    for control in &GATE_CONTROLS {
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
                            RichText::new(control.name)
                                .font(font(Face::Sans, TypeRole::Body))
                                .color(colours.text2),
                        )
                        .on_hover_text(control.hint);
                    },
                );
                let value = control.value(gate);
                let style = SliderStyle::named(colours.accent, control.name)
                    .value(control.unit.format(*value))
                    .width(slider_w);
                let style = if control.logarithmic {
                    style.logarithmic()
                } else {
                    style
                };
                styled_slider(ui, value, control.min, control.max, control.step, style)
                    .on_hover_text(control.hint);
                let text = control.unit.format(*value);
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
}

/// *Before effects* / *After effects*, the key and the reduction meter.
fn side(ui: &mut egui::Ui, gate: &mut GateSettings, reduction_db: f32, colours: &Palette) {
    quiet_heading(ui, "Position", colours);
    let mut choice = match gate.slot {
        GateSlot::Pre => 0,
        GateSlot::Post => 1,
    };
    let response = segmented(ui, &mut choice, &["Before effects", "After effects"]);
    if response.changed() {
        gate.slot = if choice == 0 {
            GateSlot::Pre
        } else {
            GateSlot::Post
        };
    }
    response.on_hover_text(format!(
        "{}\n{}",
        slot_hint(GateSlot::Pre),
        slot_hint(GateSlot::Post)
    ));
    ui.add_space(16.0);
    quiet_heading(ui, "Listens to", colours);
    let mut choice = match gate.key {
        GateKey::Signal => 0,
        GateKey::Input => 1,
    };
    let response = segmented(ui, &mut choice, &["Gated signal", "Dry input"]);
    if response.changed() {
        gate.key = if choice == 0 {
            GateKey::Signal
        } else {
            GateKey::Input
        };
    }
    response.on_hover_text(format!(
        "{}\n{}",
        key_hint(GateKey::Signal),
        key_hint(GateKey::Input)
    ));
    ui.add_space(16.0);
    quiet_heading(ui, "Reduction", colours);
    reduction_meter(ui, reduction_db, gate.range_db, colours);
}

/// A bar that fills from the left as the gate takes level away, up to the
/// whole range, with the reduction beside it.
fn reduction_meter(ui: &mut egui::Ui, reduction_db: f32, range_db: f32, colours: &Palette) {
    let reduction_db = if reduction_db.is_finite() {
        reduction_db.max(0.0)
    } else {
        0.0
    };
    let text = format_db(-reduction_db, 1);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        let bar_w = (ui.available_width() - VALUE_W - 8.0).max(40.0);
        let (rect, response) = ui.allocate_exact_size(Vec2::new(bar_w, ROW_H), Sense::hover());
        let bar = Rect::from_center_size(rect.center(), Vec2::new(bar_w, METER_H));
        ui.painter()
            .rect_filled(bar, CornerRadius::same(2), colours.well);
        let fill = reduction_fraction(reduction_db, range_db) * bar.width();
        if fill > 0.4 {
            ui.painter().rect_filled(
                Rect::from_min_size(bar.min, Vec2::new(fill, bar.height())),
                CornerRadius::same(2),
                colours.meter_hot,
            );
        }
        row_info(
            &response,
            egui::Role::Meter,
            true,
            "Reduction",
            None,
            Some(text.clone()),
        );
        response.on_hover_text("How much the gate is turning the path down right now.");
        ui.label(
            RichText::new(text)
                .font(font(Face::Mono, TypeRole::Body))
                .color(colours.text2),
        );
    });
}

#[cfg(test)]
mod tests;
