//! An output strip: the name and its badge, the *Plays* dots, the
//! *Metronome* toggle, the buffer line, the limiter's reduction, one meter
//! bar per channel, the output level and *M*.  A click anywhere else on the
//! strip opens its output sheet.
//!
//! The narrower steps drop the subtitle (step 2), then the dB scale, the
//! buffer line and the limiter bar, and the badge becomes an icon (step 4).
//! Hover keeps what a step hides.

use eframe::egui;
use egui::{CornerRadius, Rect, RichText, Sense, Stroke, Vec2};

use super::StripLayout;
use super::{level_block, strip_detail, strip_name, strip_parts, DensityStep, LevelBlock};
use crate::app::output_devices::OutputBadge;
use crate::app::screen::Sheet;
use crate::app::RigWerkApp;
use crate::audio::{DEVICE_LEVEL_MAX_DB, DEVICE_LEVEL_MIN_DB};
use crate::ui::fonts::Face;
use crate::ui::theme::{font, palette, TypeRole};
use crate::ui::widgets::{horizontal_meter, path_dot, row_info, FaderRange};

/// The limiter's bar reads 0 to this many dB of reduction.
const LIMITER_RANGE_DB: f32 = 12.0;

impl RigWerkApp {
    pub(super) fn output_strip(
        &mut self,
        ui: &mut egui::Ui,
        rect: Rect,
        device: usize,
        layout: StripLayout,
    ) {
        let colours = palette(ui.ctx());
        let rows = self.output_device_rows();
        let Some(row) = rows.get(device).cloned() else {
            return;
        };
        let (strip, mut top, mut bottom) = strip_parts(ui, rect, layout, Sense::click());
        let mini = matches!(layout.step, DensityStep::MiniOutputs | DensityStep::Scroll);
        let badge = self.output_badge(device);
        let detail = format!("{} · {} ch", row.backend, row.channels);
        let buffer = self.output_buffer_line(device);

        top.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            if mini {
                badge_icon(ui, badge);
            }
            strip_name(ui, &row.name, colours.text).on_hover_text(format!(
                "{} · {} · {detail} · {buffer}",
                row.name,
                badge.label()
            ));
        });
        if !mini {
            badge_chip(&mut top, badge);
        }
        if layout.step == DensityStep::Normal {
            strip_detail(&mut top, &detail);
        }
        let meters: &[f32] = match row.extra_output_index {
            None => &self.meter_out_channels,
            Some(index) => &self.meter_extra_output_channels[index],
        };
        let levels: Vec<f32> = meters.iter().take(row.channels).copied().collect();
        if layout.sheet_open {
            horizontal_meter(
                &mut top,
                crate::app::output_devices::loudest(&levels, row.channels),
                &format!("{} meter", row.name),
            );
        }

        self.plays_dots(&mut top, device, layout.step == DensityStep::Normal);
        let on = self.metronome_on(device);
        if metronome_toggle(&mut top, on, mini).clicked() {
            self.toggle_metronome(device);
        }
        if !mini {
            strip_detail(&mut top, &buffer);
            limiter_bar(&mut top, row.reduction_db);
        }

        let events = level_block(
            &mut bottom,
            layout,
            &LevelBlock {
                id: egui::Id::unique(("console_output_meter", device)),
                levels: &levels,
                level_db: row.settings.level_db,
                range: FaderRange {
                    min_db: DEVICE_LEVEL_MIN_DB,
                    max_db: DEVICE_LEVEL_MAX_DB,
                },
                name: &row.name,
                show_scale: !mini,
                mute: Some(row.settings.mute),
                solo: None,
                dim: row.settings.mute,
            },
        );
        if let Some(level) = events.level {
            self.set_output_level(device, level);
        }
        if events.mute {
            self.set_output_mute(device, !row.settings.mute);
        }
        row_info(
            &strip,
            egui::Role::Button,
            true,
            format!("{} output", row.name),
            None,
            None,
        );
        if strip.clicked() {
            self.open_sheet(Sheet::Output { output: device });
        }
    }

    /// *Plays*: a dot per path, filled when any of its channels play here.
    fn plays_dots(&mut self, ui: &mut egui::Ui, device: usize, label: bool) {
        let colours = palette(ui.ctx());
        let paths: Vec<(usize, u8, String)> = self
            .path_slots()
            .into_iter()
            .filter_map(|slot| {
                let path = self.paths[slot].as_ref()?;
                Some((slot, path.colour, path.name.clone()))
            })
            .collect();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            if label {
                ui.label(
                    RichText::new("Plays")
                        .font(font(Face::Sans, TypeRole::Secondary))
                        .color(colours.text3),
                );
                ui.add_space(4.0);
            }
            for (slot, colour, name) in paths {
                let plays = self.path_reaches_device(slot, device);
                let hover = if plays {
                    format!("{name} plays here.  A click on a plain Out 1/2 clears it; any other map opens the output sheet.")
                } else {
                    format!("{name} does not play here.  A click plays it on Out 1/2.")
                };
                if path_dot(ui, colour, plays, &format!("{name} plays"))
                    .on_hover_text(hover)
                    .clicked()
                {
                    self.press_plays(device, slot);
                }
            }
        });
    }
}

/// The badge as a word, amber with its word while it warns.
fn badge_chip(ui: &mut egui::Ui, badge: OutputBadge) {
    let colours = palette(ui.ctx());
    let text_font = font(Face::SansMedium, TypeRole::GridHeader);
    let label = badge.label();
    let ink = if badge.warns() {
        colours.warn
    } else {
        colours.text2
    };
    let width = ui
        .painter()
        .layout_no_wrap(label.to_owned(), text_font.clone(), ink)
        .size()
        .x
        + 12.0;
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 18.0), Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::same(3), colours.slot);
    if badge.warns() {
        ui.painter().rect_stroke(
            rect,
            CornerRadius::same(3),
            Stroke::new(1.0, colours.warn),
            egui::StrokeKind::Inside,
        );
    }
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        label,
        text_font,
        ink,
    );
    row_info(&response, egui::Role::Label, true, label, None, None);
}

/// Step 4: the badge as a letter in a box, the word on hover.
fn badge_icon(ui: &mut egui::Ui, badge: OutputBadge) {
    let colours = palette(ui.ctx());
    let letter = match badge {
        OutputBadge::Clock => "C",
        OutputBadge::Extra => "E",
        OutputBadge::InGraph => "G",
        OutputBadge::Virtual => "V",
        OutputBadge::Missing | OutputBadge::Starting | OutputBadge::Error => "!",
    };
    let ink = if badge.warns() {
        colours.warn
    } else {
        colours.text2
    };
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::hover());
    ui.painter().rect_stroke(
        rect.shrink(1.0),
        CornerRadius::same(3),
        Stroke::new(1.0, ink),
        egui::StrokeKind::Inside,
    );
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        letter,
        font(Face::MonoMedium, TypeRole::GridHeader),
        ink,
    );
    row_info(
        &response,
        egui::Role::Label,
        true,
        badge.label(),
        None,
        None,
    );
    let _ = response.on_hover_text(badge.label());
}

/// *Metronome*: a chip, or at the mini step a painted metronome.
fn metronome_toggle(ui: &mut egui::Ui, on: bool, mini: bool) -> egui::Response {
    let colours = palette(ui.ctx());
    let text_font = font(Face::Sans, TypeRole::Secondary);
    let width = if mini { 28.0 } else { ui.available_width() };
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 24.0), Sense::click());
    let (fill, stroke, ink) = if on {
        (colours.accent_bg, colours.accent, colours.accent_text)
    } else {
        (colours.slot, colours.line2, colours.text2)
    };
    ui.painter().rect_filled(rect, CornerRadius::same(3), fill);
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(3),
        Stroke::new(1.0, stroke),
        egui::StrokeKind::Inside,
    );
    if mini {
        let centre = rect.center();
        let shape = vec![
            centre + Vec2::new(-4.0, 6.0),
            centre + Vec2::new(4.0, 6.0),
            centre + Vec2::new(1.5, -6.0),
            centre + Vec2::new(-1.5, -6.0),
        ];
        ui.painter()
            .add(egui::Shape::closed_line(shape, Stroke::new(1.5, ink)));
        ui.painter().line_segment(
            [centre + Vec2::new(0.0, 3.0), centre + Vec2::new(4.0, -5.0)],
            Stroke::new(1.5, ink),
        );
    } else {
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "Metronome",
            text_font,
            ink,
        );
    }
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    row_info(
        &response,
        egui::Role::Button,
        true,
        "Metronome",
        Some(on),
        None,
    );
    crate::ui::widgets::palette_focus(ui, &response, rect);
    response.on_hover_text(if on {
        "The metronome plays here.  Off keeps its channels for next time."
    } else {
        "Play the metronome here"
    })
}

/// The limiter's gain reduction, 0 to 12 dB, in `meter_hot`.
fn limiter_bar(ui: &mut egui::Ui, reduction_db: f32) {
    let colours = palette(ui.ctx());
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 4.0), Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::same(1), colours.well);
    let reduction = reduction_db.abs().min(LIMITER_RANGE_DB);
    if reduction > 0.05 {
        let filled = Rect::from_min_size(
            rect.min,
            Vec2::new(rect.width() * reduction / LIMITER_RANGE_DB, rect.height()),
        );
        ui.painter()
            .rect_filled(filled, CornerRadius::same(1), colours.meter_hot);
    }
    let value = format!("{reduction:.1} dB");
    row_info(
        &response,
        egui::Role::Meter,
        true,
        "Limiter reduction",
        None,
        Some(value.clone()),
    );
    let _ = response.on_hover_text(format!("Limiter reduction: {value}"));
}
