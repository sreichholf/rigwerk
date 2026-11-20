//! Console: the home screen (spec §3.3).  Input, the paths, Master and the
//! outputs side by side, each group under a small label, strips inside a
//! group 1 pt apart, and a dashed add column after Paths and Outputs.
//!
//! Every strip ends in the same row (meter and fader, value, *M* / *S*), so
//! the faders line up across the screen: [`StripLayout`] gives every strip
//! the same bottom block.  The density step (`density.rs`) sets the strip
//! widths for the window width; at step 5 only the paths group scrolls
//! sideways, between Input on the left and Master and Outputs on the right.
//! While a sheet is open it covers the lower part of the strips, so each
//! strip shows a 4 pt meter under its name.  Every change goes through a
//! `RigWerkApp` method in `app/`; this module only draws.

mod add_output;
mod chain;
mod density;
mod input;
mod master;
mod output;
mod path;

use eframe::egui;
use egui::{Align, CornerRadius, Layout, Rect, RichText, Sense, Stroke, UiBuilder, Vec2};

use crate::app::RigWerkApp;
use crate::audio::MAX_PATHS;
use crate::ui::fonts::Face;
use crate::ui::theme::{font, palette, TypeRole};
use crate::ui::widgets::{
    fader, fader_height, fit_text, path_dot, popover, row_info, vertical_meter, FaderRange,
    MeterChannel,
};

use density::{density, DensityStep};

/// A ring around each strip a visible issue is anchored to, for as long
/// as that issue is visible.
fn issue_rings(app: &RigWerkApp, ui: &egui::Ui, rects: &[(Rect, crate::app::issues::Anchor)]) {
    use crate::app::issues::IssueSeverity;
    let colours = palette(ui.ctx());
    for (rect, anchor) in rects {
        let Some(severity) = app.anchor_severity(*anchor) else {
            continue;
        };
        let colour = match severity {
            IssueSeverity::Error => colours.error,
            IssueSeverity::Warning => colours.warn,
        };
        ui.painter().rect_stroke(
            rect.expand(1.5),
            CornerRadius::same(4),
            Stroke::new(2.0, colour),
            egui::StrokeKind::Inside,
        );
    }
}

/// The group labels' row.
const LABEL_H: f32 = 20.0;
/// Between groups, and between a group and its add column.
const GROUP_GAP: f32 = 12.0;
const ADD_GAP: f32 = 8.0;
const ADD_W: f32 = 32.0;
/// *M* and *S*.
const TOGGLE_H: f32 = 24.0;
const TOGGLE_W: f32 = 28.0;
/// Inside a strip, left and right.
const PAD: f32 = 8.0;
/// The fader's own width at least, readout included.
const FADER_MIN_W: f32 = 56.0;

/// What every strip shares: its height and the bottom block that holds the
/// meter, the fader, the value and the toggles.
#[derive(Clone, Copy, Debug)]
pub(crate) struct StripLayout {
    pub(crate) height: f32,
    pub(crate) fader_h: f32,
    pub(crate) step: DensityStep,
    pub(crate) sheet_open: bool,
}

impl StripLayout {
    fn new(height: f32, step: DensityStep, sheet_open: bool) -> Self {
        Self {
            height,
            fader_h: fader_height(height),
            step,
            sheet_open,
        }
    }

    /// The bottom block: fader, a gap, the toggles and the bottom padding.
    pub(crate) fn bottom_h(self) -> f32 {
        self.fader_h + 6.0 + TOGGLE_H + 8.0
    }
}

/// The x of each strip of one group, `count` strips of `width` 1 pt apart.
fn strip_rects(left: f32, top: f32, count: usize, width: f32, height: f32) -> Vec<Rect> {
    (0..count)
        .map(|index| {
            Rect::from_min_size(
                egui::pos2(left + index as f32 * (width + 1.0), top),
                Vec2::new(width, height),
            )
        })
        .collect()
}

/// `count` strips of `width` with the 1 pt gaps.
fn strips_width(count: usize, width: f32) -> f32 {
    if count == 0 {
        0.0
    } else {
        count as f32 * width + (count - 1) as f32
    }
}

impl RigWerkApp {
    /// The console, filling `ui`.
    pub(crate) fn console_page(&mut self, ui: &mut egui::Ui) {
        self.open_browser_for_screenshot();
        let colours = palette(ui.ctx());
        let full = ui.available_rect_before_wrap();
        let window_w = ui.ctx().content_rect().width();
        let slots = self.path_slots();
        let outputs = self.output_device_rows().len();
        let layout_density = density(window_w, slots.len(), outputs);
        let widths = layout_density.widths;
        let strip_h = (full.height() - LABEL_H).max(240.0);
        let layout = StripLayout::new(strip_h, layout_density.step, self.chrome.sheet.is_some());
        let top = full.top() + LABEL_H;

        let mut x = full.left();
        let input_rect = Rect::from_min_size(egui::pos2(x, top), Vec2::new(widths.input, strip_h));
        x += widths.input + GROUP_GAP;
        let paths_content = strips_width(slots.len(), widths.path);
        let paths_visible = layout_density
            .scroll
            .map_or(paths_content, |scroll| scroll.visible.min(paths_content));
        let paths_left = x;
        x += paths_visible + ADD_GAP;
        let add_path_rect = Rect::from_min_size(egui::pos2(x, top), Vec2::new(ADD_W, strip_h));
        x += ADD_W + GROUP_GAP;
        let master_rect =
            Rect::from_min_size(egui::pos2(x, top), Vec2::new(widths.master, strip_h));
        x += widths.master + GROUP_GAP;
        let outputs_left = x;
        x += strips_width(outputs, widths.output) + ADD_GAP;
        let add_output_rect = Rect::from_min_size(egui::pos2(x, top), Vec2::new(ADD_W, strip_h));

        // Group labels.
        group_label(ui, egui::pos2(input_rect.left(), full.top()), "Input");
        let paths_label = format!("Paths · {} of {MAX_PATHS}", slots.len());
        let label_end = group_label(ui, egui::pos2(paths_left, full.top()), &paths_label);
        if layout_density.scroll.is_some() {
            self.path_label_dots(ui, &slots, egui::pos2(label_end + 8.0, full.top()));
        }
        group_label(ui, egui::pos2(master_rect.left(), full.top()), "Master");
        group_label(ui, egui::pos2(outputs_left, full.top()), "Outputs");

        self.input_strip(ui, input_rect, layout);

        // Paths, scrolling sideways at step 5.
        let paths_rect = Rect::from_min_size(
            egui::pos2(paths_left, top),
            Vec2::new(paths_visible, strip_h),
        );
        let mut paths_ui = ui.new_child(
            UiBuilder::new()
                .max_rect(paths_rect)
                .layout(Layout::left_to_right(Align::Min)),
        );
        if layout_density.scroll.is_some() {
            let id = egui::Id::unique("console_paths_scroll");
            let offset: f32 = ui.data(|data| data.get_temp(id)).unwrap_or(0.0);
            let mut scroll = egui::ScrollArea::horizontal()
                .id_salt("console_paths")
                .max_width(paths_visible)
                .auto_shrink([false, false]);
            if let Some(index) = self
                .chrome
                .reveal_path
                .and_then(|slot| slots.iter().position(|have| *have == slot))
            {
                let left = index as f32 * (widths.path + 1.0);
                scroll = scroll.horizontal_scroll_offset(chain::offset_to_show(
                    left,
                    left + widths.path,
                    offset,
                    paths_visible,
                ));
            }
            let output = scroll.show(&mut paths_ui, |ui| {
                let (content, _) =
                    ui.allocate_exact_size(Vec2::new(paths_content, strip_h), Sense::hover());
                self.path_strips(ui, &slots, content.left(), top, widths.path, layout);
            });
            ui.data_mut(|data| data.insert_temp(id, output.state.clamped_offset().x));
        } else {
            self.path_strips(&mut paths_ui, &slots, paths_left, top, widths.path, layout);
        }
        self.chrome.reveal_path = None;
        self.add_path_column(ui, add_path_rect);

        self.master_strip(ui, master_rect, layout);
        for (device, rect) in strip_rects(outputs_left, top, outputs, widths.output, strip_h)
            .into_iter()
            .enumerate()
        {
            self.output_strip(ui, rect, device, layout);
        }
        separators(
            ui,
            outputs_left,
            top,
            outputs,
            widths.output,
            strip_h,
            colours.line,
        );
        self.add_output_column(ui, add_output_rect);

        self.chrome.follow_stage = false;
        let mut rings = vec![
            (input_rect, crate::app::issues::Anchor::Input),
            (master_rect, crate::app::issues::Anchor::Master),
        ];
        for (device, rect) in strip_rects(outputs_left, top, outputs, widths.output, strip_h)
            .into_iter()
            .enumerate()
        {
            rings.push((rect, crate::app::issues::Anchor::Output(device)));
        }
        issue_rings(self, ui, &rings);
        ui.allocate_rect(full, Sense::hover());
    }

    fn path_strips(
        &mut self,
        ui: &mut egui::Ui,
        slots: &[usize],
        left: f32,
        top: f32,
        width: f32,
        layout: StripLayout,
    ) {
        let colours = palette(ui.ctx());
        for (&slot, rect) in
            slots
                .iter()
                .zip(strip_rects(left, top, slots.len(), width, layout.height))
        {
            self.path_strip(ui, rect, slot, layout);
        }
        separators(
            ui,
            left,
            top,
            slots.len(),
            width,
            layout.height,
            colours.line,
        );
    }

    /// Step 5: a dot per path after the *Paths* label; a click scrolls that
    /// path into view.
    fn path_label_dots(&mut self, ui: &mut egui::Ui, slots: &[usize], at: egui::Pos2) {
        let rect = Rect::from_min_size(at, Vec2::new(16.0 * slots.len() as f32, LABEL_H - 4.0));
        let mut dots = ui.new_child(
            UiBuilder::new()
                .max_rect(rect)
                .layout(Layout::left_to_right(Align::Center)),
        );
        dots.spacing_mut().item_spacing.x = 4.0;
        for &slot in slots {
            let Some(path) = self.paths[slot].as_ref() else {
                continue;
            };
            let name = format!("Show {}", path.name);
            if path_dot(&mut dots, path.colour, true, &name)
                .on_hover_text(&path.name)
                .clicked()
            {
                self.chrome.reveal_path = Some(slot);
            }
        }
    }

    /// The dashed column after the paths: *Empty path*, *Effects return*,
    /// *Duplicate ▸* and *Apply layout ▸*.  Disabled at the limit, with the reason on hover.
    fn add_path_column(&mut self, ui: &mut egui::Ui, rect: Rect) {
        let blocked = self.add_path_blocker().map(console_reason);
        let response = add_column(ui, rect, blocked, "Add path");
        let names: Vec<(usize, String)> = self
            .path_slots()
            .into_iter()
            .filter_map(|slot| Some((slot, self.paths[slot].as_ref()?.name.clone())))
            .collect();
        popover(ui, &response, |ui| {
            if menu_entry(ui, "Empty path") {
                let _ = self.add_path();
                ui.close();
            }
            if menu_entry(ui, "Effects return") {
                let _ = self.add_return();
                ui.close();
            }
            ui.menu_button("Duplicate ▸", |ui| {
                for (slot, name) in &names {
                    if menu_entry(ui, name) {
                        let _ = self.duplicate_path(*slot);
                        ui.close();
                    }
                }
            });
            self.layout_menu(ui);
        });
    }

    /// *Apply layout ▸*: the layouts, each applied to the current setup.
    pub(crate) fn layout_menu(&mut self, ui: &mut egui::Ui) {
        ui.menu_button("Apply layout ▸", |ui| {
            for layout in crate::app::layouts::PathLayout::ALL {
                if menu_entry(ui, layout.label()) {
                    if !self.apply_layout(layout) {
                        self.push_log(
                            format!("{}: no free path slot yet, try again.", layout.label()),
                            crate::app::LogTone::Amber,
                        );
                    }
                    ui.close();
                }
            }
        });
    }

    /// The dashed column after the outputs: the device list of the platform.
    fn add_output_column(&mut self, ui: &mut egui::Ui, rect: Rect) {
        let blocked = self.add_output_disabled_reason().map(console_reason);
        let response = add_column(ui, rect, blocked, "Add output");
        popover(ui, &response, |ui| self.add_output_list(ui));
    }
}

/// The spec's short reasons for the add columns (§3.3).
fn console_reason(reason: &'static str) -> &'static str {
    if reason.starts_with("Four paths") {
        "Four paths is the most"
    } else if reason.starts_with("A removed path") {
        "A removed path is still fading out"
    } else if reason.contains("not available") {
        "Extra outputs are not available on this platform"
    } else {
        "Four extra outputs is the most"
    }
}

/// A group's small label at `at`.  Returns where it ends.
fn group_label(ui: &egui::Ui, at: egui::Pos2, text: &str) -> f32 {
    let colours = palette(ui.ctx());
    let galley = ui.painter().layout_no_wrap(
        text.to_owned(),
        font(Face::SansMedium, TypeRole::Secondary),
        colours.text3,
    );
    let width = galley.size().x;
    ui.painter()
        .galley(egui::pos2(at.x, at.y + 2.0), galley, colours.text3);
    at.x + width
}

/// The 1 pt lines between neighbouring strips of one group.
fn separators(
    ui: &egui::Ui,
    left: f32,
    top: f32,
    count: usize,
    width: f32,
    height: f32,
    colour: egui::Color32,
) {
    for index in 1..count {
        let x = left + index as f32 * (width + 1.0) - 0.5;
        ui.painter()
            .vline(x, top..=top + height, Stroke::new(1.0, colour));
    }
}

/// A dashed add column.  `blocked` disables it with that reason on hover.
fn add_column(ui: &mut egui::Ui, rect: Rect, blocked: Option<&str>, name: &str) -> egui::Response {
    let colours = palette(ui.ctx());
    let sense = if blocked.is_none() {
        Sense::click()
    } else {
        Sense::hover()
    };
    let mut response = ui.allocate_rect(rect, sense);
    crate::ui::widgets::dashed_rect(ui.painter(), rect.shrink(1.0), colours.line2);
    ui.painter().text(
        egui::pos2(rect.center().x, rect.top() + 24.0),
        egui::Align2::CENTER_CENTER,
        "+",
        font(Face::Sans, TypeRole::Title),
        if blocked.is_none() {
            colours.text2
        } else {
            colours.text3
        },
    );
    response = match blocked {
        Some(reason) => response.on_hover_text(reason),
        None => response.on_hover_text(name),
    };
    row_info(
        &response,
        egui::Role::Button,
        blocked.is_none(),
        name,
        None,
        None,
    );
    response
}

/// One plain entry of a popover menu.
pub(crate) fn menu_entry(ui: &mut egui::Ui, label: &str) -> bool {
    let colours = palette(ui.ctx());
    ui.add(
        egui::Button::new(
            RichText::new(label)
                .font(font(Face::Sans, TypeRole::Body))
                .color(colours.text),
        )
        .frame(false),
    )
    .clicked()
}

/// A strip's background and its two parts: the top (name, controls) and
/// the bottom block every strip shares.
fn strip_parts(
    ui: &mut egui::Ui,
    rect: Rect,
    layout: StripLayout,
    sense: Sense,
) -> (egui::Response, egui::Ui, egui::Ui) {
    let colours = palette(ui.ctx());
    let response = ui.allocate_rect(rect, sense);
    ui.painter()
        .rect_filled(rect, CornerRadius::same(4), colours.strip);
    let bottom = Rect::from_min_max(
        egui::pos2(rect.left() + PAD, rect.bottom() - layout.bottom_h()),
        egui::pos2(rect.right() - PAD, rect.bottom() - 8.0),
    );
    let top = Rect::from_min_max(
        egui::pos2(rect.left() + PAD, rect.top() + 8.0),
        egui::pos2(rect.right() - PAD, bottom.top() - 6.0),
    );
    let mut top_ui = ui.new_child(
        UiBuilder::new()
            .max_rect(top)
            .layout(Layout::top_down(Align::Min)),
    );
    // Inside the strip, and inside a scrolling paths group.
    top_ui.shrink_clip_rect(top.expand2(Vec2::new(PAD - 1.0, 4.0)));
    top_ui.spacing_mut().item_spacing.y = 6.0;
    let bottom_ui = ui.new_child(
        UiBuilder::new()
            .max_rect(bottom)
            .layout(Layout::top_down(Align::Center)),
    );
    (response, top_ui, bottom_ui)
}

/// A strip's name in the strip face, cut to fit.
fn strip_name(ui: &mut egui::Ui, name: &str, colour: egui::Color32) -> egui::Response {
    let text_font = font(Face::SansMedium, TypeRole::Control);
    let shown = fit_text(ui, name, &text_font, ui.available_width());
    ui.label(RichText::new(shown).font(text_font).color(colour))
}

/// A secondary line under a strip's name.
fn strip_detail(ui: &mut egui::Ui, text: &str) {
    let colours = palette(ui.ctx());
    let text_font = font(Face::Sans, TypeRole::Secondary);
    let shown = fit_text(ui, text, &text_font, ui.available_width());
    ui.label(RichText::new(shown).font(text_font).color(colours.text3));
}

/// What the bottom block reports.
#[derive(Default)]
struct LevelEvents {
    level: Option<f32>,
    mute: bool,
    solo: bool,
}

/// What the bottom block shows.
struct LevelBlock<'a> {
    /// Keeps the meter's peak hold.
    id: egui::Id,
    levels: &'a [f32],
    level_db: f32,
    range: FaderRange,
    /// The fader's and the meter's AccessKit names start with this.
    name: &'a str,
    /// The 0–48 scale, where the strip has room for it.
    show_scale: bool,
    /// `Some` draws *M*, lit while true.
    mute: Option<bool>,
    solo: Option<bool>,
    /// Dims the fader (muted, or another path soloed).
    dim: bool,
}

/// The bottom block every strip ends in: meter and fader side by side (the
/// meter over the fader's left edge when the strip is narrow), then the
/// toggles.
fn level_block(ui: &mut egui::Ui, layout: StripLayout, block: &LevelBlock<'_>) -> LevelEvents {
    let mut events = LevelEvents::default();
    let area = ui.max_rect();
    let channels = block.levels.len().max(1);
    let bar = match channels {
        0..=2 => 5.0,
        3..=4 => 3.0,
        _ => 2.0,
    };
    let bars_w = channels as f32 * bar + (channels - 1) as f32 * 2.0;
    // The scale goes first when the strip is too narrow for it.
    let show_scale = block.show_scale && area.width() >= 22.0 + bars_w + 6.0 + FADER_MIN_W;
    let meter_w = if show_scale { 22.0 + bars_w } else { bars_w };
    let side_by_side = area.width() >= meter_w + 6.0 + FADER_MIN_W;
    let fader_rect = if side_by_side {
        let group = meter_w + 6.0 + FADER_MIN_W;
        let left = area.center().x - group / 2.0;
        Rect::from_min_size(
            egui::pos2(left + meter_w + 6.0, area.top()),
            Vec2::new(FADER_MIN_W, layout.fader_h),
        )
    } else {
        Rect::from_min_size(area.min, Vec2::new(area.width(), layout.fader_h))
    };
    let meter_rect = if side_by_side {
        Rect::from_min_size(
            egui::pos2(fader_rect.left() - 6.0 - meter_w, area.top()),
            Vec2::new(meter_w, layout.fader_h - 18.0),
        )
    } else {
        Rect::from_min_size(area.min, Vec2::new(meter_w, layout.fader_h - 18.0))
    };

    let mut fader_ui = ui.new_child(
        UiBuilder::new()
            .max_rect(fader_rect)
            .layout(Layout::top_down(Align::Center)),
    );
    if block.dim {
        fader_ui.multiply_opacity(0.45);
    }
    let mut value = block.level_db;
    if fader(
        &mut fader_ui,
        &mut value,
        block.range,
        layout.fader_h,
        &format!("{} level", block.name),
    )
    .changed()
    {
        events.level = Some(value);
    }

    let mut meter_ui = ui.new_child(UiBuilder::new().max_rect(meter_rect));
    let mut state: Vec<MeterChannel> = ui.data(|data| data.get_temp(block.id)).unwrap_or_default();
    vertical_meter(
        &mut meter_ui,
        block.levels,
        &mut state,
        meter_rect.height(),
        show_scale,
        &format!("{} meter", block.name),
    );
    ui.data_mut(|data| data.insert_temp(block.id, state));

    let toggles = Rect::from_min_size(
        egui::pos2(area.left(), area.top() + layout.fader_h + 6.0),
        Vec2::new(area.width(), TOGGLE_H),
    );
    let mut toggle_ui = ui.new_child(
        UiBuilder::new()
            .max_rect(toggles)
            .layout(Layout::left_to_right(Align::Center)),
    );
    let count = usize::from(block.mute.is_some()) + usize::from(block.solo.is_some());
    let used = count as f32 * TOGGLE_W + count.saturating_sub(1) as f32 * 4.0;
    toggle_ui.add_space(((toggles.width() - used) / 2.0).max(0.0));
    toggle_ui.spacing_mut().item_spacing.x = 4.0;
    if let Some(on) = block.mute {
        if toggle(&mut toggle_ui, on, "M", &format!("Mute {}", block.name)).clicked() {
            events.mute = true;
        }
    }
    if let Some(on) = block.solo {
        if toggle(&mut toggle_ui, on, "S", &format!("Solo {}", block.name)).clicked() {
            events.solo = true;
        }
    }
    events
}

/// A small toggle: *M*, *S*.  `name` says what it does for screen readers
/// and on hover.
fn toggle(ui: &mut egui::Ui, on: bool, label: &str, name: &str) -> egui::Response {
    let colours = palette(ui.ctx());
    let (rect, response) = ui.allocate_exact_size(Vec2::new(TOGGLE_W, TOGGLE_H), Sense::click());
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
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        label,
        font(Face::SansMedium, TypeRole::Body),
        ink,
    );
    row_info(
        &response,
        egui::Role::Button,
        ui.is_enabled(),
        name,
        Some(on),
        None,
    );
    crate::ui::widgets::palette_focus(ui, &response, rect);
    response.on_hover_text(name)
}

#[cfg(test)]
mod tests;
