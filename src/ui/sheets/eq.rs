//! The EQ sheet's body (`docs/ui-spec.md` §6.1): the response curve with
//! its five band handles on the left, the band table on the right, *Flat*
//! above them.
//!
//! The header above (`sheets.rs`) already holds the EQ's *On* / *Bypass*.
//! The curve is the filter math the engine runs
//! ([`EqSettings::response_db`]); a handle sits at its band's frequency and
//! gain, so a drag moves exactly those two.  Every edit is one
//! [`RigWerkApp::set_path_eq`] after the frame, never an engine call.  The
//! selected band is view state and lives in egui's memory.

use eframe::egui;
use egui::epaint::{Mesh, Vertex, WHITE_UV};
use egui::{Align, Align2, Color32, CornerRadius, Layout, Rect, RichText, Sense, Stroke, Vec2};

use crate::app::{LogTone, RigWerkApp};
use crate::audio::{
    BandKind, EngineState, EqBand, EqSettings, EqSlot, EQ_BANDS, EQ_MAX_GAIN_DB, EQ_MAX_Q, EQ_MIN_Q,
};
use crate::ui::fonts::Face;
use crate::ui::theme::{font, palette, with_alpha, Palette, TypeRole};
use crate::ui::widgets::{chip, row_info, CONTROL_H};

use crate::ui::widgets::OutlineButton;

const CURVE_MIN_HZ: f32 = 20.0;
const CURVE_MAX_HZ: f32 = 20_000.0;
/// The curve shows a little past ±12 dB so a full boost is not on the edge.
const CURVE_RANGE_DB: f32 = EQ_MAX_GAIN_DB + 3.0;
const HANDLE_R: f32 = 9.0;
const ROW_H: f32 = 32.0;
const HEADER_H: f32 = 20.0;
/// Number, band, type, frequency, gain, Q, on.
const COLUMNS: [f32; 7] = [28.0, 72.0, 76.0, 84.0, 80.0, 64.0, 44.0];
const COLUMN_GAP: f32 = 8.0;
const ROW_GAP: f32 = 4.0;
/// Below this the table goes under the curve.
const MIN_CURVE_W: f32 = 320.0;

fn table_width() -> f32 {
    COLUMNS.iter().sum::<f32>() + COLUMN_GAP * (COLUMNS.len() - 1) as f32
}

fn table_height() -> f32 {
    HEADER_H + ROW_GAP + (ROW_H + ROW_GAP) * EQ_BANDS.len() as f32
}

pub(crate) fn hz_label(hz: f32) -> String {
    if hz >= 1_000.0 {
        format!("{:.1} kHz", hz / 1_000.0)
    } else {
        format!("{hz:.0} Hz")
    }
}

/// An EQ gain: one decimal, the real minus sign, a plus for a boost.
pub(crate) fn gain_label(db: f32) -> String {
    let tenths = (db * 10.0).round();
    if tenths == 0.0 {
        "0.0 dB".to_owned()
    } else if tenths < 0.0 {
        format!("−{:.1} dB", -tenths / 10.0)
    } else {
        format!("+{:.1} dB", tenths / 10.0)
    }
}

/// A typed number: `−4`, `+3.5 dB`, `1.2k`, `800 Hz`, `2.5 kHz`.
pub(crate) fn parse_value(text: &str) -> Option<f64> {
    let text = text.trim().replace('−', "-").to_lowercase();
    let text = text
        .trim_end_matches("db")
        .trim_end_matches("hz")
        .trim()
        .to_owned();
    let (number, scale) = match text.strip_suffix('k') {
        Some(number) => (number.trim(), 1_000.0),
        None => (text.as_str(), 1.0),
    };
    number
        .trim_start_matches('+')
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
        .map(|value| value * scale)
}

pub(crate) fn slot_title(slot: EqSlot) -> &'static str {
    match slot {
        EqSlot::Pre => "Pre EQ",
        EqSlot::Post => "Post EQ",
    }
}

pub(crate) fn slot_hint(slot: EqSlot) -> &'static str {
    match slot {
        EqSlot::Pre => {
            "Before the plugin effects: shapes what the plugins receive, e.g. tighten the low end before an amp sim."
        }
        EqSlot::Post => {
            "After the plugin effects: shapes what the plugins produce, e.g. tame fizz after an amp sim."
        }
    }
}

pub(crate) fn kind_label(kind: BandKind) -> &'static str {
    match kind {
        BandKind::LowShelf => "Low shelf",
        BandKind::HighShelf => "High shelf",
        BandKind::Bell => "Bell",
    }
}

/// What band `index` is and what it does on guitar.
pub(crate) fn band_hint(index: usize) -> String {
    let spec = &EQ_BANDS[index];
    format!("{}. {}", kind_label(spec.kind), spec.role)
}

/// How Q behaves on this kind of band.
pub(crate) fn q_hint(kind: BandKind) -> &'static str {
    match kind {
        BandKind::Bell => {
            "Width: low Q is broad and musical, high Q is narrow for taming one spot."
        }
        BandKind::LowShelf | BandKind::HighShelf => {
            "Shelf width: low Q is a gentle slope, high Q is steeper with a resonant bump before the corner and a dip after it."
        }
    }
}

/// Band `index` as *Flat* leaves it.
fn default_band(index: usize) -> EqBand {
    EqSettings::default().bands[index]
}

/// Where the curve maps frequency and gain.
#[derive(Clone, Copy, Debug)]
struct Plot {
    rect: Rect,
}

impl Plot {
    fn span() -> f32 {
        (CURVE_MAX_HZ / CURVE_MIN_HZ).ln()
    }

    fn x_of(self, hz: f32) -> f32 {
        self.rect.left() + self.rect.width() * (hz / CURVE_MIN_HZ).ln() / Self::span()
    }

    fn hz_at(self, x: f32) -> f32 {
        CURVE_MIN_HZ * ((x - self.rect.left()) / self.rect.width() * Self::span()).exp()
    }

    fn y_of(self, db: f32) -> f32 {
        let db = db.clamp(-CURVE_RANGE_DB, CURVE_RANGE_DB);
        self.rect.center().y - db / CURVE_RANGE_DB * self.rect.height() / 2.0
    }

    fn db_at(self, y: f32) -> f32 {
        (self.rect.center().y - y) / (self.rect.height() / 2.0) * CURVE_RANGE_DB
    }

    fn handle(self, band: &EqBand) -> egui::Pos2 {
        egui::pos2(self.x_of(band.hz), self.y_of(band.gain_db))
    }
}

/// Band `index` with its handle moved to `at`: whole hertz inside the
/// band's range, gain in 0.1 dB steps within ±12 dB.  Q and the switch stay.
fn dragged_band(index: usize, band: EqBand, plot: Plot, at: egui::Pos2) -> EqBand {
    let spec = &EQ_BANDS[index];
    EqBand {
        hz: plot.hz_at(at.x).round().clamp(spec.min_hz, spec.max_hz),
        gain_db: ((plot.db_at(at.y) * 10.0).round() / 10.0).clamp(-EQ_MAX_GAIN_DB, EQ_MAX_GAIN_DB),
        ..band
    }
}

/// Q after `scroll` points of wheel: up narrows, down widens, one notch
/// about a fifth, in 0.01 steps within 0.3–6.
fn scrolled_q(q: f32, scroll: f32) -> f32 {
    ((q * (scroll * 0.005).exp() * 100.0).round() / 100.0).clamp(EQ_MIN_Q, EQ_MAX_Q)
}

/// Wheel movement this frame in points, from the raw events so one notch
/// is one step.
fn wheel_points(ui: &egui::Ui) -> f32 {
    ui.input(|input| {
        input
            .events
            .iter()
            .map(|event| match event {
                egui::Event::MouseWheel { unit, delta, .. } => match unit {
                    egui::MouseWheelUnit::Point => delta.y,
                    egui::MouseWheelUnit::Line => delta.y * 40.0,
                    egui::MouseWheelUnit::Page => delta.y * 400.0,
                },
                _ => 0.0,
            })
            .sum()
    })
}

/// Fill, ring and number colour of a band's handle and its table badge.
fn badge_colours(
    colours: &Palette,
    path: Color32,
    selected: bool,
    live: bool,
) -> (Color32, Color32, Color32) {
    match (selected, live) {
        (true, true) => (colours.accent, colours.accent, colours.sheet),
        (true, false) => (colours.slot, colours.accent, colours.text3),
        (false, true) => (colours.slot, path, colours.text),
        (false, false) => (colours.slot, colours.led_off, colours.text3),
    }
}

fn paint_badge(
    painter: &egui::Painter,
    center: egui::Pos2,
    number: usize,
    (fill, ring, ink): (Color32, Color32, Color32),
) {
    painter.circle_filled(center, HANDLE_R, fill);
    painter.circle_stroke(center, HANDLE_R - 0.75, Stroke::new(1.5, ring));
    painter.text(
        center,
        Align2::CENTER_CENTER,
        number.to_string(),
        font(Face::SansSemibold, TypeRole::GridHeader),
        ink,
    );
}

impl RigWerkApp {
    /// The EQ sheet body for path `path`: the pre EQ, or the post EQ when
    /// `post`.
    pub(crate) fn eq_sheet(&mut self, ui: &mut egui::Ui, path: usize, post: bool) {
        let slot = if post { EqSlot::Post } else { EqSlot::Pre };
        let Some(signal) = self.paths.get(path).and_then(Option::as_ref) else {
            return;
        };
        let current = signal.mix.eqs[slot.index()];
        let colours = palette(ui.ctx());
        let path_colour = colours.path[signal.colour as usize % colours.path.len()];
        let rate = if self.engine.state == EngineState::Running && self.snapshot.output_rate > 0 {
            self.snapshot.output_rate
        } else {
            48_000
        };
        let selected_id = egui::Id::unique(("eq_sheet_band", path, post));
        let mut selected = ui
            .data(|data| data.get_temp::<usize>(selected_id))
            .unwrap_or(0)
            .min(EQ_BANDS.len() - 1);
        let mut eq = current;

        let width = ui.available_width();
        let ids = egui::Id::unique(("eq_sheet", path, post));
        let mut edit = Edit {
            eq: &mut eq,
            selected: &mut selected,
            path: path_colour,
            rate,
            ids,
        };
        let top = CONTROL_H + 8.0;
        if width >= table_width() + 16.0 + MIN_CURVE_W {
            // The curve takes the body's full height; *Flat* sits over the
            // table.
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 16.0;
                edit.curve(
                    ui,
                    Vec2::new(width - table_width() - 16.0, top + table_height()),
                );
                ui.vertical(|ui| {
                    ui.set_width(table_width());
                    edit.top_row(ui, slot);
                    ui.add_space(8.0);
                    edit.table(ui);
                });
            });
        } else {
            edit.top_row(ui, slot);
            ui.add_space(8.0);
            edit.curve(ui, Vec2::new(width, table_height()));
            ui.add_space(12.0);
            edit.table(ui);
        }

        ui.data_mut(|data| data.insert_temp(selected_id, selected));
        let eq = eq.sanitized();
        if eq != current {
            if eq == current.flat() && current != current.flat() {
                self.push_log(
                    format!("{} reset to flat.", slot_title(slot)),
                    LogTone::Amber,
                );
            }
            self.select_path(path);
            self.set_path_eq(slot.index(), eq);
            self.push_paths_to_engine();
        }
    }
}

/// One frame's edit of the sheet's EQ copy.
struct Edit<'a> {
    eq: &'a mut EqSettings,
    selected: &'a mut usize,
    path: Color32,
    rate: u32,
    ids: egui::Id,
}

impl Edit<'_> {
    /// What this EQ is for, on hover, and *Flat*.
    fn top_row(&mut self, ui: &mut egui::Ui, slot: EqSlot) {
        let colours = palette(ui.ctx());
        ui.horizontal(|ui| {
            ui.set_min_height(CONTROL_H);
            ui.label(
                RichText::new("5 bands · no added latency")
                    .font(font(Face::Sans, TypeRole::Secondary))
                    .color(colours.text3),
            )
            .on_hover_text(slot_hint(slot));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if OutlineButton::new("Flat")
                    .show(ui)
                    .on_hover_text("Reset every band to its default, 0 dB.")
                    .clicked()
                {
                    *self.eq = self.eq.flat();
                }
            });
        });
    }

    /// The response curve with the band handles.
    fn curve(&mut self, ui: &mut egui::Ui, size: Vec2) {
        let colours = palette(ui.ctx());
        let (rect, area) = ui.allocate_exact_size(size, Sense::hover());
        row_info(
            &area,
            egui::Role::Image,
            true,
            "EQ curve",
            None,
            Some(format!("{} bands", EQ_BANDS.len())),
        );
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, CornerRadius::same(4), colours.bg);
        let plot = Plot {
            rect: Rect::from_min_max(
                rect.min + Vec2::new(34.0, 12.0),
                rect.max - Vec2::new(12.0, 22.0),
            ),
        };
        paint_grid(&painter, plot, colours);

        // Handles take their input before the curve is drawn, so the curve
        // already shows this frame's drag.
        let mut order: Vec<usize> = (0..EQ_BANDS.len())
            .filter(|&index| index != *self.selected)
            .collect();
        order.push(*self.selected);
        let mut hovered = None;
        for &index in &order {
            if self.handle(ui, plot, index) {
                hovered = Some(index);
            }
        }
        let scroll = if area.contains_pointer() || hovered.is_some() {
            wheel_points(ui)
        } else {
            0.0
        };
        if scroll != 0.0 {
            let index = hovered.unwrap_or(*self.selected);
            let band = &mut self.eq.bands[index];
            band.q = scrolled_q(band.q, scroll);
            *self.selected = index;
            ui.input_mut(|input| input.smooth_scroll_delta = Vec2::ZERO);
        }

        self.paint_curve(&painter, plot, colours);
        for &index in &order {
            let band = &self.eq.bands[index];
            let live = self.eq.enabled && band.on;
            let center = plot.handle(band);
            if live {
                painter.line_segment(
                    [egui::pos2(center.x, plot.y_of(0.0)), center],
                    Stroke::new(1.0, with_alpha(self.path, 70)),
                );
            }
            paint_badge(
                &painter,
                center,
                index + 1,
                badge_colours(colours, self.path, index == *self.selected, live),
            );
        }
    }

    /// Band `index`'s handle: drag for frequency and gain, double-click to
    /// reset.  True while the pointer is over it.
    fn handle(&mut self, ui: &mut egui::Ui, plot: Plot, index: usize) -> bool {
        let spec = &EQ_BANDS[index];
        let center = plot.handle(&self.eq.bands[index]);
        let id = self.ids.with(("handle", index));
        let response = ui.interact(
            Rect::from_center_size(center, Vec2::splat(HANDLE_R * 2.0 + 4.0)),
            id,
            Sense::click_and_drag(),
        );
        if response.drag_started() {
            let origin = ui.input(|input| input.pointer.press_origin());
            if let Some(origin) = origin {
                ui.data_mut(|data| data.insert_temp(id, center - origin));
            }
        }
        if response.dragged() {
            if let Some(pointer) = response.interact_pointer_pos() {
                let offset = ui
                    .data(|data| data.get_temp::<Vec2>(id))
                    .unwrap_or_default();
                self.eq.bands[index] =
                    dragged_band(index, self.eq.bands[index], plot, pointer + offset);
            }
        }
        if response.clicked() || response.drag_started() {
            *self.selected = index;
        }
        if response.double_clicked() {
            self.eq.bands[index] = default_band(index);
        }
        if response.hovered() || response.dragged() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
        }
        let band = self.eq.bands[index];
        row_info(
            &response,
            egui::Role::Slider,
            true,
            format!("Band {}", index + 1),
            Some(index == *self.selected),
            Some(format!(
                "{}, {}, {}, Q {:.2}",
                spec.name,
                hz_label(band.hz),
                gain_label(band.gain_db),
                band.q
            )),
        );
        let hovered = response.hovered();
        response.on_hover_text(format!(
            "{}\nDrag for frequency and gain, scroll for Q, double-click to reset.",
            band_hint(index)
        ));
        hovered
    }

    fn paint_curve(&self, painter: &egui::Painter, plot: Plot, colours: &Palette) {
        const STEPS: usize = 240;
        let sample = |db: &dyn Fn(f32) -> f32| -> Vec<egui::Pos2> {
            (0..=STEPS)
                .map(|step| {
                    let hz = CURVE_MIN_HZ * (Plot::span() * step as f32 / STEPS as f32).exp();
                    egui::pos2(plot.x_of(hz), plot.y_of(db(hz)))
                })
                .collect()
        };
        let clipped = painter.with_clip_rect(plot.rect.expand(2.0));
        let eq = *self.eq;
        let points = sample(&|hz| eq.response_db(hz, self.rate));
        let colour = if eq.enabled {
            self.path
        } else {
            colours.led_off
        };
        if eq.enabled {
            clipped.add(fill_to_zero(&points, plot.y_of(0.0), colour));
            for (index, band) in eq.bands.iter().enumerate() {
                if !band.on || band.gain_db.abs() < 0.01 {
                    continue;
                }
                let trace = sample(&|hz| eq.band_response_db(index, hz, self.rate));
                let stroke = if index == *self.selected {
                    Stroke::new(1.5, with_alpha(colours.accent, 170))
                } else {
                    Stroke::new(1.0, with_alpha(self.path, 90))
                };
                clipped.add(egui::Shape::line(trace, stroke));
            }
        }
        clipped.add(egui::Shape::line(points, Stroke::new(2.0, colour)));
    }

    /// The band table: number, band, type, frequency, gain, Q, on.
    fn table(&mut self, ui: &mut egui::Ui) {
        let colours = palette(ui.ctx());
        ui.allocate_ui_with_layout(
            Vec2::new(table_width(), table_height()),
            Layout::top_down(Align::LEFT),
            |ui| {
                ui.set_width(table_width());
                ui.spacing_mut().item_spacing = Vec2::new(COLUMN_GAP, ROW_GAP);
                ui.horizontal(|ui| {
                    for (label, width) in ["", "Band", "Type", "Frequency", "Gain", "Q", "On"]
                        .into_iter()
                        .zip(COLUMNS)
                    {
                        cell(ui, width, HEADER_H, |ui| {
                            ui.label(
                                RichText::new(label)
                                    .font(font(Face::Sans, TypeRole::GridHeader))
                                    .color(colours.text3),
                            );
                        });
                    }
                });
                for index in 0..EQ_BANDS.len() {
                    self.row(ui, index, colours);
                }
            },
        );
    }

    fn row(&mut self, ui: &mut egui::Ui, index: usize, colours: &Palette) {
        let spec = &EQ_BANDS[index];
        let selected = index == *self.selected;
        let live = self.eq.enabled && self.eq.bands[index].on;
        let ink = if live { colours.text } else { colours.text3 };
        let background = ui.painter().add(egui::Shape::Noop);
        let row = ui.horizontal(|ui| {
            ui.style_mut().override_font_id = Some(font(Face::Mono, TypeRole::Body));
            ui.spacing_mut().button_padding = Vec2::new(8.0, 4.0);
            let widgets = &mut ui.visuals_mut().widgets;
            for widget in [
                &mut widgets.inactive,
                &mut widgets.hovered,
                &mut widgets.active,
            ] {
                widget.corner_radius = CornerRadius::same(4);
            }
            ui.visuals_mut().override_text_color = Some(ink);
            let mut pick = false;
            let band = &mut self.eq.bands[index];

            cell(ui, COLUMNS[0], ROW_H, |ui| {
                let (rect, badge) = ui.allocate_exact_size(Vec2::splat(ROW_H), Sense::click());
                paint_badge(
                    ui.painter(),
                    rect.center(),
                    index + 1,
                    badge_colours(colours, self.path, selected, live),
                );
                row_info(
                    &badge,
                    egui::Role::Button,
                    true,
                    format!("Select band {}", index + 1),
                    Some(selected),
                    None,
                );
                pick |= badge.clicked();
            });
            cell(ui, COLUMNS[1], ROW_H, |ui| {
                let name = ui
                    .add(
                        egui::Label::new(
                            RichText::new(spec.name)
                                .font(font(Face::SansMedium, TypeRole::Body))
                                .color(if selected { colours.accent_text } else { ink }),
                        )
                        .sense(Sense::click()),
                    )
                    .on_hover_text(band_hint(index));
                pick |= name.clicked();
            });
            cell(ui, COLUMNS[2], ROW_H, |ui| {
                ui.label(
                    RichText::new(kind_label(spec.kind))
                        .font(font(Face::Sans, TypeRole::Body))
                        .color(ink),
                )
                .on_hover_text(band_hint(index));
            });
            cell(ui, COLUMNS[3], ROW_H, |ui| {
                let mut hz = band.hz;
                let response = ui
                    .add_sized(
                        [COLUMNS[3], 26.0],
                        egui::DragValue::new(&mut hz)
                            .range(spec.min_hz..=spec.max_hz)
                            .speed(f64::from(band.hz) * 0.005)
                            .fixed_decimals(0)
                            .custom_formatter(|value, _| hz_label(value as f32))
                            .custom_parser(parse_value),
                    )
                    .on_hover_text(format!(
                        "{} to {}.",
                        hz_label(spec.min_hz),
                        hz_label(spec.max_hz)
                    ));
                row_info(
                    &response,
                    egui::Role::SpinButton,
                    true,
                    format!("{} frequency", spec.name),
                    None,
                    Some(hz_label(hz)),
                );
                if response.changed() {
                    band.hz = hz.round();
                    pick = true;
                }
            });
            cell(ui, COLUMNS[4], ROW_H, |ui| {
                let mut gain = band.gain_db;
                let response = ui
                    .add_sized(
                        [COLUMNS[4], 26.0],
                        egui::DragValue::new(&mut gain)
                            .range(-EQ_MAX_GAIN_DB..=EQ_MAX_GAIN_DB)
                            .speed(0.05)
                            .fixed_decimals(1)
                            .custom_formatter(|value, _| gain_label(value as f32))
                            .custom_parser(parse_value),
                    )
                    .on_hover_text("Boost or cut, ±12 dB in 0.1 dB steps.");
                row_info(
                    &response,
                    egui::Role::SpinButton,
                    true,
                    format!("{} gain", spec.name),
                    None,
                    Some(gain_label(gain)),
                );
                if response.changed() {
                    band.gain_db = gain;
                    pick = true;
                }
            });
            cell(ui, COLUMNS[5], ROW_H, |ui| {
                let mut q = band.q;
                let response = ui
                    .add_sized(
                        [COLUMNS[5], 26.0],
                        egui::DragValue::new(&mut q)
                            .range(EQ_MIN_Q..=EQ_MAX_Q)
                            .speed(0.01)
                            .fixed_decimals(2)
                            .custom_parser(parse_value),
                    )
                    .on_hover_text(q_hint(spec.kind));
                row_info(
                    &response,
                    egui::Role::SpinButton,
                    true,
                    format!("{} Q", spec.name),
                    None,
                    Some(format!("{q:.2}")),
                );
                if response.changed() {
                    band.q = q;
                    pick = true;
                }
            });
            cell(ui, COLUMNS[6], ROW_H, |ui| {
                let mut on = band.on;
                let response = chip(ui, &mut on, "On").on_hover_text("Switch this band on or off.");
                row_info(
                    &response,
                    egui::Role::CheckBox,
                    true,
                    format!("{} on", spec.name),
                    Some(on),
                    None,
                );
                if response.changed() {
                    band.on = on;
                    pick = true;
                }
            });
            pick
        });
        if row.inner {
            *self.selected = index;
        }
        if selected {
            ui.painter().set(
                background,
                egui::Shape::rect_filled(
                    row.response.rect.expand2(Vec2::new(4.0, 2.0)),
                    CornerRadius::same(3),
                    colours.accent_bg,
                ),
            );
        }
    }
}

/// A fixed-size cell of the band table, contents centred vertically.
fn cell(ui: &mut egui::Ui, width: f32, height: f32, add: impl FnOnce(&mut egui::Ui)) {
    ui.allocate_ui_with_layout(
        Vec2::new(width, height),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.set_width(width);
            ui.set_height(height);
            add(ui);
        },
    );
}

fn paint_grid(painter: &egui::Painter, plot: Plot, colours: &Palette) {
    let label = font(Face::Mono, TypeRole::GridHeader);
    for (db, text) in [
        (12.0, "+12"),
        (6.0, "+6"),
        (0.0, "0"),
        (-6.0, "−6"),
        (-12.0, "−12"),
    ] {
        let y = plot.y_of(db);
        let stroke = if db == 0.0 {
            Stroke::new(1.0, colours.line2)
        } else {
            Stroke::new(1.0, colours.line)
        };
        painter.hline(plot.rect.x_range(), y, stroke);
        painter.text(
            egui::pos2(plot.rect.left() - 6.0, y),
            Align2::RIGHT_CENTER,
            text,
            label.clone(),
            colours.text3,
        );
    }
    for (hz, text) in [
        (50.0, "50"),
        (100.0, "100"),
        (200.0, "200"),
        (500.0, "500"),
        (1_000.0, "1k"),
        (2_000.0, "2k"),
        (5_000.0, "5k"),
        (10_000.0, "10k"),
    ] {
        let x = plot.x_of(hz);
        painter.vline(x, plot.rect.y_range(), Stroke::new(1.0, colours.line));
        painter.text(
            egui::pos2(x, plot.rect.bottom() + 4.0),
            Align2::CENTER_TOP,
            text,
            label.clone(),
            colours.text3,
        );
    }
}

/// A fill from the curve to 0 dB that fades towards the line.
fn fill_to_zero(points: &[egui::Pos2], zero_y: f32, colour: Color32) -> egui::Shape {
    let top = with_alpha(colour, 56);
    let bottom = with_alpha(colour, 0);
    let mut mesh = Mesh::default();
    for pair in points.windows(2) {
        let (left, right) = (pair[0], pair[1]);
        let base = mesh.vertices.len() as u32;
        for (pos, color) in [
            (left, top),
            (egui::pos2(left.x, zero_y), bottom),
            (right, top),
            (egui::pos2(right.x, zero_y), bottom),
        ] {
            mesh.vertices.push(Vertex {
                pos,
                uv: WHITE_UV,
                color,
            });
        }
        mesh.indices
            .extend([base, base + 1, base + 2, base + 1, base + 3, base + 2]);
    }
    egui::Shape::mesh(mesh)
}

#[cfg(test)]
mod tests;
