//! The master strip: *Master*, the master level and *M*.

use eframe::egui;
use egui::{Rect, RichText, Sense};

use super::{level_block, strip_name, strip_parts, LevelBlock, StripLayout};
use crate::app::settings::{OUTPUT_GAIN_MAX_DB, OUTPUT_GAIN_MIN_DB};
use crate::app::RigWerkApp;
use crate::ui::fonts::Face;
use crate::ui::theme::{font, palette, TypeRole};
use crate::ui::widgets::{horizontal_meter, FaderRange};

impl RigWerkApp {
    pub(super) fn master_strip(&mut self, ui: &mut egui::Ui, rect: Rect, layout: StripLayout) {
        let colours = palette(ui.ctx());
        let (_, mut top, mut bottom) = strip_parts(ui, rect, layout, Sense::hover());
        let marked = self.anchor_severity(crate::app::issues::Anchor::Master);
        top.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let reserve = if marked.is_some() { 18.0 } else { 0.0 };
            ui.scope(|ui| {
                ui.set_max_width((ui.available_width() - reserve).max(8.0));
                strip_name(ui, "Master", colours.text);
            });
            if let Some(severity) = marked {
                issue_marker(ui, severity, "The master has an issue");
            }
        });
        if layout.sheet_open {
            horizontal_meter(
                &mut top,
                self.meter_out_l.max(self.meter_out_r),
                "Master meter",
            );
        }
        let events = level_block(
            &mut bottom,
            layout,
            &LevelBlock {
                id: egui::Id::unique("console_master_meter"),
                levels: &[self.meter_out_l, self.meter_out_r],
                level_db: self.output_gain,
                range: FaderRange {
                    min_db: OUTPUT_GAIN_MIN_DB,
                    max_db: OUTPUT_GAIN_MAX_DB,
                },
                name: "Master",
                show_scale: true,
                mute: Some(self.mute),
                solo: None,
                dim: self.mute,
                trim_line: None,
            },
        );
        if let Some(level) = events.level {
            self.set_master_level(level);
        }
        if events.mute {
            self.mute = !self.mute;
        }
    }
}

fn issue_marker(ui: &mut egui::Ui, severity: crate::app::issues::IssueSeverity, hover: &str) {
    let colours = palette(ui.ctx());
    let colour = match severity {
        crate::app::issues::IssueSeverity::Error => colours.error,
        crate::app::issues::IssueSeverity::Warning => colours.warn,
    };
    ui.label(
        RichText::new("▲")
            .font(font(Face::Sans, TypeRole::Secondary))
            .color(colour),
    )
    .on_hover_text(hover);
}
