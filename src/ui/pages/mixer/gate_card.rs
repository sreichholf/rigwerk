//! Noise-gate inspector: threshold, range, hysteresis, attack/release/hold, slot.

use eframe::egui;
use egui::{Align, Color32, Layout, RichText, Vec2};

use crate::app::RigWerkApp;
use crate::audio::GateSlot;
use crate::ui::sheets::gate::{
    reset_gate, slot_hint, GateUnit, GATE_CONTROLS, RESET_HINT, SUMMARY,
};
use crate::ui::theme::ts;
use crate::ui::theme::{AMBER, BLUE, DIM, GREEN, MUTED, TEAL, TEXT};
use crate::ui::widgets::{hinted_tab, mono, styled_slider};

/// Wide enough for "HYSTERESIS" in the mono micro label.
const LABEL_W: f32 = 88.0;
const READOUT_W: f32 = 56.0;
/// Gap between label / slider / readout.
const COL_GAP: f32 = 8.0;
/// Breathing room so the grip circle and readout clear the pane edge.
const RIGHT_INSET: f32 = 12.0;

impl RigWerkApp {
    pub(crate) fn gate_inspector(&mut self, ui: &mut egui::Ui) {
        let current = self.path().mix.gate;
        let mut gate = current;
        let mut reset = false;

        ui.horizontal(|ui| {
            ui.label(RichText::new("Gate").size(ts(16.0)).color(TEXT).strong());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if hinted_tab(ui, "RESET", false, MUTED, RESET_HINT) {
                    gate = reset_gate(gate);
                    reset = true;
                }
                ui.add_space(4.0);
                if hinted_tab(
                    ui,
                    if gate.enabled { "ON" } else { "BYP" },
                    gate.enabled,
                    AMBER,
                    "Switch the gate on or off. Settings are kept while bypassed.",
                ) {
                    gate.enabled = !gate.enabled;
                }
            });
        });
        ui.label(RichText::new(SUMMARY).size(ts(11.0)).color(DIM));
        ui.add_space(10.0);

        // One width for every row so tracks and readouts share a right edge,
        // with a small inset from the inspector pane border.
        let panel_w = ui.available_width();
        let row_w = (panel_w - RIGHT_INSET).max(200.0);
        let slider_w = (row_w - LABEL_W - READOUT_W - COL_GAP * 2.0).max(80.0);

        ui.allocate_ui_with_layout(Vec2::new(row_w, 0.0), Layout::top_down(Align::LEFT), |ui| {
            ui.set_width(row_w);

            ui.horizontal(|ui| {
                ui.label(RichText::new("Slot").size(ts(11.0)).color(MUTED));
                ui.add_space(8.0);
                for slot in GateSlot::ALL {
                    let label = match slot {
                        GateSlot::Pre => "PRE",
                        GateSlot::Post => "POST",
                    };
                    if hinted_tab(ui, label, gate.slot == slot, TEAL, slot_hint(slot)) {
                        gate.slot = slot;
                    }
                }
            });
            ui.add_space(10.0);

            for (control, color) in GATE_CONTROLS
                .iter()
                .zip([AMBER, AMBER, GREEN, TEAL, BLUE, MUTED])
            {
                knob_row(
                    ui,
                    row_w,
                    slider_w,
                    &control.name.to_uppercase(),
                    control.value(&mut gate),
                    control.min,
                    control.max,
                    control.step,
                    color,
                    control.unit,
                    control.hint,
                );
            }

            ui.add_space(10.0);
            ui.label(
                RichText::new(match gate.slot {
                    GateSlot::Pre => "PRE · before effects",
                    GateSlot::Post => "POST · after effects",
                })
                .size(ts(10.5))
                .color(MUTED),
            );
        });

        self.apply_gate(current, gate, reset);
    }
}

/// Fixed-width row: label | slider | readout. Every row shares `row_w` /
/// `slider_w` so tracks end on one vertical line and the readout sits just
/// inside the inspector's right inset.
#[allow(clippy::too_many_arguments)]
fn knob_row(
    ui: &mut egui::Ui,
    row_w: f32,
    slider_w: f32,
    label: &str,
    value: &mut f32,
    min: f32,
    max: f32,
    step: f32,
    color: Color32,
    unit: GateUnit,
    hint: &str,
) {
    ui.allocate_ui_with_layout(
        Vec2::new(row_w, 26.0),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.set_width(row_w);
            ui.allocate_ui_with_layout(
                Vec2::new(LABEL_W, 22.0),
                Layout::left_to_right(Align::Center),
                |ui| {
                    ui.set_width(LABEL_W);
                    ui.label(mono(label, DIM, 8.0));
                },
            );
            ui.add_space(COL_GAP);
            ui.allocate_ui_with_layout(
                Vec2::new(slider_w, 22.0),
                Layout::left_to_right(Align::Center),
                |ui| {
                    ui.set_width(slider_w);
                    let _ = styled_slider(ui, value, min, max, step, (color, label.to_owned()))
                        .on_hover_text(hint);
                },
            );
            ui.add_space(COL_GAP);
            ui.allocate_ui_with_layout(
                Vec2::new(READOUT_W, 22.0),
                Layout::right_to_left(Align::Center),
                |ui| {
                    ui.set_width(READOUT_W);
                    ui.label(mono(unit.format(*value), MUTED, 9.0));
                },
            );
        },
    );
    ui.add_space(4.0);
}
