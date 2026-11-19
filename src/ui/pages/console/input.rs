//! The input strip: the device (a click opens Setup › Devices), *In 1* /
//! *In 2*, the scope (a click opens the scope sheet), *Driver panel* for
//! an ASIO input, and the input gain over the stereo meter.

use eframe::egui;
use egui::{CornerRadius, Rect, Sense, Stroke, Vec2};

use super::{level_block, strip_detail, strip_parts, LevelBlock, StripLayout};
use crate::app::screen::{SetupSection, Sheet};
use crate::app::settings::{INPUT_GAIN_MAX_DB, INPUT_GAIN_MIN_DB};
use crate::app::RigWerkApp;
use crate::audio::{AsioPanelSide, EngineState};
use crate::ui::fonts::Face;
use crate::ui::theme::{font, palette, TypeRole};
use crate::ui::widgets::{chip, fit_text, horizontal_meter, row_info, FaderRange, OutlineButton};

const SCOPE_H: f32 = 56.0;

impl RigWerkApp {
    pub(super) fn input_strip(&mut self, ui: &mut egui::Ui, rect: Rect, layout: StripLayout) {
        let colours = palette(ui.ctx());
        let (_, mut top, mut bottom) = strip_parts(ui, rect, layout, Sense::hover());
        let device = self.active_input().clone();

        let name_font = font(Face::SansMedium, TypeRole::Control);
        let marked = self.anchor_severity(crate::app::issues::Anchor::Input);
        let name = top
            .horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                let room = ui.available_width() - if marked.is_some() { 18.0 } else { 0.0 };
                let shown = fit_text(ui, &device.name, &name_font, room.max(8.0));
                let name = ui
                    .add(
                        egui::Button::new(
                            egui::RichText::new(shown)
                                .font(name_font.clone())
                                .color(colours.text),
                        )
                        .frame(false),
                    )
                    .on_hover_text(format!("{}: open Setup › Devices", device.name));
                if let Some(severity) = marked {
                    issue_marker(ui, severity, "The input has an issue");
                }
                name
            })
            .inner;
        row_info(
            &name,
            egui::Role::Button,
            true,
            device.name.clone(),
            None,
            None,
        );
        if name.clicked() {
            self.open_setup(SetupSection::Devices);
        }
        strip_detail(
            &mut top,
            &format!("{} · {} ch", device.backend, device.channels.max(1)),
        );
        if layout.sheet_open {
            horizontal_meter(
                &mut top,
                self.meter_in_l.max(self.meter_in_r),
                "Input meter",
            );
        }

        top.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            for (index, label) in ["In 1", "In 2"].into_iter().enumerate() {
                let mut on = self.input_channels[index];
                if chip(ui, &mut on, label).changed() {
                    self.set_input_channel(index, on);
                }
            }
        });

        let scope = self.mini_scope(&mut top);
        if scope.clicked() {
            self.open_sheet(Sheet::Scope);
        }

        self.input_driver_panel(&mut top);

        let mut events = level_block(
            &mut bottom,
            layout,
            &LevelBlock {
                id: egui::Id::unique("console_input_meter"),
                levels: &[self.meter_in_l, self.meter_in_r],
                level_db: self.input_gain,
                range: FaderRange {
                    min_db: INPUT_GAIN_MIN_DB,
                    max_db: INPUT_GAIN_MAX_DB,
                },
                name: "Input gain",
                show_scale: true,
                mute: None,
                solo: None,
                dim: false,
            },
        );
        if let Some(gain) = events.level.take() {
            self.set_input_gain(gain);
        }
    }

    /// The input and the output overlaid, small.  Clicking it opens the
    /// scope sheet.
    fn mini_scope(&self, ui: &mut egui::Ui) -> egui::Response {
        let colours = palette(ui.ctx());
        let width = ui.available_width();
        let (rect, response) = ui.allocate_exact_size(Vec2::new(width, SCOPE_H), Sense::click());
        let painter = ui.painter().with_clip_rect(rect);
        painter.rect_filled(rect, CornerRadius::same(3), colours.well);
        painter.hline(
            rect.x_range(),
            rect.center().y,
            Stroke::new(1.0, colours.line),
        );
        let active = matches!(self.engine.state, EngineState::Running);
        for (samples, colour) in [
            (&self.scope_in, colours.text3),
            (&self.scope_out, colours.meter),
        ] {
            if !active || samples.is_empty() {
                continue;
            }
            let count = samples.len().max(2);
            let points: Vec<egui::Pos2> = samples
                .iter()
                .enumerate()
                .map(|(index, sample)| {
                    let x = rect.left() + rect.width() * index as f32 / (count - 1) as f32;
                    let y = rect.center().y - sample.clamp(-1.0, 1.0) * rect.height() * 0.45;
                    egui::pos2(x, y)
                })
                .collect();
            painter.add(egui::Shape::line(points, Stroke::new(1.0, colour)));
        }
        row_info(&response, egui::Role::Button, true, "Scope", None, None);
        response.on_hover_text("Scope: input and output, the peaks and the sample count")
    }

    /// *Driver panel* for an ASIO input: disabled until the bridge runs on
    /// that driver, with that reason on hover.
    fn input_driver_panel(&mut self, ui: &mut egui::Ui) {
        let Some(clsid) = self.active_input().asio_clsid else {
            return;
        };
        let ready = self.engine.open_asio_driver(AsioPanelSide::Input) == Some(clsid);
        let response = OutlineButton::new("Driver panel")
            .enabled(ready)
            .full_width()
            .show(ui);
        if !ready {
            let _ = response
                .on_disabled_hover_text("Available once the bridge runs on this ASIO driver");
        } else if response
            .on_hover_text("Open this ASIO driver's own settings window")
            .clicked()
        {
            self.engine.request_asio_panel(AsioPanelSide::Input);
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
        egui::RichText::new("▲")
            .font(font(Face::Sans, TypeRole::Secondary))
            .color(colour),
    )
    .on_hover_text(hover);
}
