//! The master strip: *Master*, the master level and *M*.  While *Monitor*
//! is off it reads *Monitor off*, as the title bar does.

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
        strip_name(&mut top, "Master", colours.text);
        if !self.monitor {
            top.label(
                RichText::new("Monitor off")
                    .font(font(Face::SansMedium, TypeRole::Secondary))
                    .color(colours.accent_text),
            )
            .on_hover_text("Input monitoring is off: the master plays silence");
        }
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
                dim: self.mute || !self.monitor,
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
