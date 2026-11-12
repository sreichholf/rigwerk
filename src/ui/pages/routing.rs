//! Signal Routing, in the layers the engine has:
//!
//! 1. Input -> paths: which input channel feeds each path's L and R, before
//!    its chain (`routes`, columns 0 and 1; one matrix per path).
//! 2. Paths: one card per path (chain, level, mute, solo, Open in Mixer,
//!    Duplicate, remove) and Add path ([`paths`]).
//! 3. Paths -> outputs: which device channels play each path, per output
//!    device, and how the paths add up there ([`outputs`]).
//!
//! Above them the routing cards (starting points, `app::cards`) and, with
//! several paths, a flow strip.  Plus the Mix card (channel mode and
//! monitoring).  With one path the page reads as before: Input -> Mix,
//! Mix -> outputs, no per-path chrome.

mod outputs;
mod paths;
mod sum;

use crate::app::LogTone;
use eframe::egui;
use egui::{
    Align, Align2, Color32, CornerRadius, FontFamily, FontId, Layout, RichText, Sense, Stroke, Vec2,
};

use crate::app::paths::PathInfo;
use crate::app::RigWerkApp;
use crate::ui::theme::ts;
use crate::ui::theme::{
    path_colour, with_alpha, AMBER, BLUE, BORDER, BORDER_SOFT, CARD, CARD_DEEP, DIM, MUTED, TEAL,
    TEXT,
};
use crate::ui::widgets::{
    card, card_fixed, ellipsize, horizontal_line, micro_label, mono, row_info, show_focus_ring,
    status_dot, tab_button_sized,
};

/// Side-by-side columns need 520 + 280 plus the gap. Below this available
/// width the Mix card is clipped, so the two cards stack instead.
pub(crate) const ROUTING_STACK_BELOW: f32 = 1100.0;

/// Mix sides layer ① feeds. The stored route table is still 2×4 for older
/// prefs, but only its first two columns mean anything: they are Mix L and
/// Mix R, the stereo bus the chain processes.
pub(crate) const MIX_COLUMNS: [&str; 2] = ["MIX L", "MIX R"];

/// Width and height of one matrix cell, in both layers.
pub(crate) const CELL: Vec2 = Vec2::new(58.0, 48.0);

/// Width of a matrix row header (plus the gap before the first cell).
pub(crate) const ROW_HEADER_W: f32 = 142.0;

/// Shared outer height for Input → Mix and Mix when they sit on one row.
pub(crate) fn routing_top_row_height(inputs: usize) -> f32 {
    let rows = inputs.max(1) as f32;
    // 15 px inset each side, then the taller Input → Mix body.
    let body = 22.0 // header
        + 16.0 // subtitle
        + 10.0
        + 24.0 // MIX L / MIX R headers
        + 2.0
        + rows * (CELL.y + 5.0)
        + 8.0
        + 1.0 // hairline
        + 8.0
        + 52.0; // tip
    (body + 30.0).max(300.0)
}

pub(crate) fn routing_columns_stack(available_width: f32) -> bool {
    available_width < ROUTING_STACK_BELOW
}

/// Size of layer ①: every input row onto the two mix sides.
pub(crate) fn input_matrix_label(inputs: usize) -> String {
    format!("{inputs} IN -> MIX L/R")
}

/// Stereo channel name for a matrix row.
pub(crate) fn input_channel_role(index: usize) -> &'static str {
    match index {
        0 => "Left",
        1 => "Right",
        _ => "Input",
    }
}

/// Backend and name of the selected input. ASIO appears only when that is
/// the device's own backend.
pub(crate) fn input_channel_caption(backend: &str, name: &str) -> String {
    let backend = backend.trim();
    let name = name.trim();
    if backend.is_empty() && name.is_empty() {
        "Input".to_owned()
    } else if backend.is_empty() {
        name.to_owned()
    } else if name.is_empty() {
        backend.to_uppercase()
    } else {
        format!("{} · {name}", backend.to_uppercase())
    }
}

/// Mix → Outputs header count. Playing channels are those a mix side
/// reaches plus unreached ones under Mono copy, which carry the average
/// (see `OutputDeviceSettings::playing_channels`).
pub(crate) fn patch_count_label(devices: usize, channels: usize) -> String {
    let plural = |count: usize, word: &str| {
        if count == 1 {
            format!("{count} {word}")
        } else {
            format!("{count} {word}S")
        }
    };
    format!(
        "{} · {} PLAYING",
        plural(devices, "DEVICE"),
        plural(channels, "CHANNEL")
    )
}

/// One routing cell: a dot when on, the faint `glyph` when off. `name` is
/// what a screen reader hears; the value says whether it is on.
pub(crate) fn matrix_cell(
    ui: &mut egui::Ui,
    enabled: bool,
    glyph: &str,
    name: String,
    values: (&str, &str),
) -> bool {
    matrix_cell_in(ui, enabled, glyph, name, values, TEAL)
}

/// [`matrix_cell`] in a path's colour.
pub(crate) fn matrix_cell_in(
    ui: &mut egui::Ui,
    enabled: bool,
    glyph: &str,
    name: String,
    values: (&str, &str),
    color: Color32,
) -> bool {
    let (rect, response) = ui.allocate_exact_size(CELL, Sense::click());
    let base = if enabled {
        with_alpha(color, 18)
    } else if response.hovered() {
        with_alpha(TEXT, 7)
    } else {
        CARD_DEEP
    };
    ui.painter().rect_filled(rect, CornerRadius::same(8), base);
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(8),
        Stroke::new(
            1.0,
            if enabled {
                with_alpha(color, 75)
            } else {
                BORDER_SOFT
            },
        ),
        egui::StrokeKind::Inside,
    );
    if enabled {
        ui.painter()
            .circle_filled(rect.center(), 9.0, with_alpha(color, 26));
        ui.painter().circle_filled(rect.center(), 3.3, color);
    } else {
        ui.painter().circle_stroke(
            rect.center(),
            7.0,
            Stroke::new(1.0, Color32::from_rgb(49, 60, 77)),
        );
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            glyph,
            FontId::new(ts(7.5), FontFamily::Monospace),
            Color32::from_rgb(53, 64, 80),
        );
    }
    show_focus_ring(ui, &response, rect, 8);
    row_info(
        &response,
        egui::Role::CheckBox,
        ui.is_enabled(),
        name,
        Some(enabled),
        Some(if enabled { values.0 } else { values.1 }.to_owned()),
    );
    response.clicked()
}

/// A layer ① cell: input `input` onto mix side `side` (0 = L, 1 = R).
pub(crate) fn route_cell(ui: &mut egui::Ui, enabled: bool, input: usize, side: usize) -> bool {
    let letter = if side == 0 { "L" } else { "R" };
    matrix_cell(
        ui,
        enabled,
        &format!("{}{letter}", input + 1),
        format!("Route input {} to Mix {letter}", input + 1),
        ("enabled", "muted"),
    )
}

/// A layer ① cell with several paths: input `input` onto path `path`'s
/// side `side` (0 = L, 1 = R).
pub(crate) fn path_route_cell(
    ui: &mut egui::Ui,
    enabled: bool,
    input: usize,
    side: usize,
    path: &str,
    color: Color32,
) -> bool {
    let letter = if side == 0 { "L" } else { "R" };
    matrix_cell_in(
        ui,
        enabled,
        &format!("{}{letter}", input + 1),
        format!("Route input {} to {path} {letter}", input + 1),
        ("enabled", "muted"),
        color,
    )
}

/// A layer ② cell with several paths: path `path`'s side `letter` onto
/// device channel `channel` (0-based).
pub(crate) fn path_output_cell(
    ui: &mut egui::Ui,
    on: bool,
    path: &str,
    letter: &str,
    channel: usize,
    color: Color32,
) -> bool {
    matrix_cell_in(
        ui,
        on,
        &format!("{letter}{}", channel + 1),
        format!("{path} {letter} to Out {}", channel + 1),
        ("routed", "off"),
        color,
    )
}

/// A row header for one side of a path: a colour bar on the left, dimmed
/// while the path is not audible.
pub(crate) fn path_row_header(
    ui: &mut egui::Ui,
    name: &str,
    caption: &str,
    color: Color32,
    audible: bool,
) {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(ROW_HEADER_W, CELL.y), Sense::hover());
    let _ = response.on_hover_text(format!("{name} · {caption}"));
    let painter = ui.painter().with_clip_rect(rect);
    let fade = |c: Color32| if audible { c } else { with_alpha(c, 130) };
    painter.rect_filled(rect, CornerRadius::same(8), CARD_DEEP);
    painter.rect_filled(
        egui::Rect::from_min_size(rect.min, Vec2::new(3.0, rect.height())),
        CornerRadius {
            nw: 8,
            sw: 8,
            ne: 0,
            se: 0,
        },
        fade(color),
    );
    let font = FontId::new(ts(11.0), FontFamily::Proportional);
    let shown = crate::ui::widgets::fit_text(ui, name, &font, ROW_HEADER_W - 20.0);
    painter.text(
        rect.left_center() + Vec2::new(12.0, -6.0),
        Align2::LEFT_CENTER,
        shown,
        font,
        fade(TEXT),
    );
    painter.text(
        rect.left_center() + Vec2::new(12.0, 8.0),
        Align2::LEFT_CENTER,
        caption,
        FontId::new(ts(8.0), FontFamily::Proportional),
        fade(DIM),
    );
}

/// A path's name over its two layer ① columns, underlined in its colour.
pub(crate) fn path_group_header(ui: &mut egui::Ui, name: &str, color: Color32, width: f32) {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 20.0), Sense::hover());
    let _ = response.on_hover_text(name);
    let font = FontId::new(ts(8.5), FontFamily::Monospace);
    let shown = crate::ui::widgets::fit_text(ui, &name.to_uppercase(), &font, width - 4.0);
    ui.painter().text(
        rect.center() - Vec2::new(0.0, 2.0),
        Align2::CENTER_CENTER,
        shown,
        font,
        color,
    );
    ui.painter().line_segment(
        [rect.left_bottom(), rect.right_bottom()],
        Stroke::new(2.0, color),
    );
}

/// A matrix row header: a name and a small caption under it.
pub(crate) fn row_header(ui: &mut egui::Ui, name: &str, caption: &str, color: Color32) {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(ROW_HEADER_W, CELL.y), Sense::hover());
    let _ = response.on_hover_text(format!("{name} · {caption}"));
    let painter = ui.painter().with_clip_rect(rect);
    painter.rect_filled(rect, CornerRadius::same(8), CARD_DEEP);
    painter.text(
        rect.left_center() + Vec2::new(12.0, -6.0),
        Align2::LEFT_CENTER,
        name,
        FontId::new(ts(11.5), FontFamily::Proportional),
        color,
    );
    painter.text(
        rect.left_center() + Vec2::new(12.0, 8.0),
        Align2::LEFT_CENTER,
        caption,
        FontId::new(ts(8.5), FontFamily::Proportional),
        DIM,
    );
}

/// A column header: a mono name and an optional caption under it.
pub(crate) fn column_header(ui: &mut egui::Ui, name: &str, caption: Option<&str>) {
    ui.add_sized(
        [CELL.x, if caption.is_some() { 28.0 } else { 18.0 }],
        egui::Label::new(match caption {
            Some(caption) => {
                let mut job = egui::text::LayoutJob {
                    halign: Align::Center,
                    ..Default::default()
                };
                job.append(
                    name,
                    0.0,
                    egui::TextFormat::simple(FontId::new(ts(9.0), FontFamily::Monospace), MUTED),
                );
                job.append(
                    &format!("\n{caption}"),
                    0.0,
                    egui::TextFormat::simple(FontId::new(ts(7.5), FontFamily::Monospace), DIM),
                );
                egui::WidgetText::from(job)
            }
            None => egui::WidgetText::from(mono(name, DIM, 9.0)),
        })
        .selectable(false),
    );
}

/// A small circled layer number ("1", "2") before a card title.
pub(crate) fn layer_badge(ui: &mut egui::Ui, number: usize, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
    ui.painter()
        .circle_stroke(rect.center(), 6.0, Stroke::new(1.0, color));
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        number.to_string(),
        FontId::new(ts(8.0), FontFamily::Proportional),
        color,
    );
}

impl RigWerkApp {
    pub(crate) fn routing_page(&mut self, ui: &mut egui::Ui) {
        let total = ui.available_width();
        let infos = self.path_infos();
        let multi = infos.len() > 1;
        let mut actions = Vec::new();

        self.routing_cards_row(ui, total, &mut actions);
        if multi {
            self.routing_flow(ui, total, &infos);
        }

        if routing_columns_stack(total) {
            card(ui, CARD, BORDER, |ui| {
                ui.set_width(total - 32.0);
                self.routing_input_body(ui, &infos);
            });
            card(ui, CARD, BORDER, |ui| {
                ui.set_width(total - 32.0);
                self.routing_mix_body(ui, &infos);
            });
        } else {
            let left_w = (total * 0.66).clamp(520.0, (total - 288.0).max(520.0));
            let right_w = (total - left_w - 8.0).max(280.0);
            // Same outer height on both cards so the row stays level.
            let row_h = routing_top_row_height(self.path().mix.routes.len())
                + if multi {
                    // Path group headers on the left, a channel mode row
                    // per path on the right.
                    (infos.len() as f32 - 1.0) * 28.0 + 24.0
                } else {
                    0.0
                };
            ui.horizontal_top(|ui| {
                ui.allocate_ui_with_layout(
                    Vec2::new(left_w, row_h),
                    Layout::top_down(Align::LEFT),
                    |ui| {
                        ui.set_min_size(Vec2::new(left_w, row_h));
                        card_fixed(ui, CARD, BORDER, row_h, |ui| {
                            ui.set_width(left_w - 32.0);
                            self.routing_input_body(ui, &infos);
                        });
                    },
                );
                ui.allocate_ui_with_layout(
                    Vec2::new(right_w, row_h),
                    Layout::top_down(Align::LEFT),
                    |ui| {
                        ui.set_min_size(Vec2::new(right_w, row_h));
                        card_fixed(ui, CARD, BORDER, row_h, |ui| {
                            ui.set_width(right_w - 32.0);
                            self.routing_mix_body(ui, &infos);
                        });
                    },
                );
            });
        }

        self.routing_paths_card(ui, total, &infos, &mut actions);
        self.routing_outputs_card(ui, total, &infos, &mut actions);
        for action in actions {
            self.apply_routing_action(action);
        }
    }

    /// Layer ①: input channels onto Mix L / Mix R, or with several paths
    /// onto each path's L / R.
    fn routing_input_body(&mut self, ui: &mut egui::Ui, infos: &[PathInfo]) {
        if infos.len() > 1 {
            self.routing_input_paths_body(ui, infos);
            return;
        }
        let input = self.active_input();
        let input_caption = input_channel_caption(&input.backend, &input.name);
        let subtitle = format!("{input_caption} -> the mix bus the chain processes");
        let input_count = self.path().mix.routes.len();

        ui.horizontal(|ui| {
            layer_badge(ui, 1, TEAL);
            ui.label(micro_label("INPUT -> MIX", TEAL));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(mono(input_matrix_label(input_count), DIM, 9.5));
            });
        });
        ui.label(
            RichText::new(ellipsize(&subtitle, 72))
                .size(ts(10.0))
                .color(MUTED),
        )
        .on_hover_text(&subtitle);
        ui.add_space(10.0);

        ui.horizontal(|ui| {
            ui.add_space(ROW_HEADER_W);
            for name in MIX_COLUMNS {
                column_header(ui, name, Some("into chain"));
            }
        });
        ui.add_space(2.0);

        for input in 0..input_count {
            let role = input_channel_role(input);
            ui.horizontal(|ui| {
                row_header(ui, &format!("IN {}", input + 1), role, TEXT);
                for (side, column) in MIX_COLUMNS.iter().enumerate() {
                    let routed = self.path().mix.routes[input][side];
                    if route_cell(ui, routed, input, side) {
                        let on = !routed;
                        self.set_path_route(input, side, on);
                        self.push_paths_to_engine();
                        self.push_log(
                            format!(
                                "Route {role} -> {column} {}.",
                                if on { "enabled" } else { "muted" }
                            ),
                            if on { LogTone::Teal } else { LogTone::Muted },
                        );
                    }
                }
            });
            ui.add_space(5.0);
        }

        ui.add_space(8.0);
        horizontal_line(ui, BORDER_SOFT);
        ui.add_space(8.0);
        ui.horizontal_top(|ui| {
            ui.label(RichText::new("Tip").size(ts(9.5)).color(AMBER).strong());
            ui.add(
                egui::Label::new(
                    RichText::new(
                        "Mono guitar on IN 1? Route it to both MIX L and MIX R, or use Mono sum. Which input channels are captured is still picked on Live.",
                    )
                    .size(ts(10.0))
                    .color(MUTED),
                )
                .wrap(),
            );
        });
    }

    /// Layer ① with several paths: per path, the input channels onto its
    /// own L / R (taps 1/2 of its stored 2×4 routes).
    fn routing_input_paths_body(&mut self, ui: &mut egui::Ui, infos: &[PathInfo]) {
        let input = self.active_input();
        let input_caption = input_channel_caption(&input.backend, &input.name);
        let subtitle = format!("{input_caption} -> each path's own L/R, before its chain");
        let input_count = self.path().mix.routes.len();

        ui.horizontal(|ui| {
            layer_badge(ui, 1, TEAL);
            ui.label(micro_label("INPUT -> PATHS", TEAL));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(mono(
                    format!("{input_count} IN → {} × L/R", infos.len()),
                    DIM,
                    9.5,
                ));
            });
        });
        ui.label(
            RichText::new(ellipsize(&subtitle, 80))
                .size(ts(10.0))
                .color(MUTED),
        )
        .on_hover_text(&subtitle);
        ui.add_space(10.0);

        let gap = 18.0;
        egui::ScrollArea::horizontal()
            .id_salt("routing_input_paths")
            .auto_shrink([false, true])
            .show(ui, |ui| {
                let spacing = ui.spacing().item_spacing.x;
                let group_w = CELL.x * 2.0 + spacing;
                ui.horizontal(|ui| {
                    ui.add_space(ROW_HEADER_W);
                    for info in infos {
                        path_group_header(ui, &info.name, path_colour(info.colour_index), group_w);
                        ui.add_space(gap);
                    }
                });
                ui.horizontal(|ui| {
                    ui.add_space(ROW_HEADER_W);
                    for _ in infos {
                        column_header(ui, "L", None);
                        column_header(ui, "R", None);
                        ui.add_space(gap);
                    }
                });
                ui.add_space(2.0);
                for input in 0..input_count {
                    let role = input_channel_role(input);
                    ui.horizontal(|ui| {
                        row_header(ui, &format!("IN {}", input + 1), role, TEXT);
                        for info in infos {
                            for side in 0..2 {
                                let Some(path) = self.paths[info.slot].as_ref() else {
                                    continue;
                                };
                                let mut routes = path.mix.routes;
                                let routed = routes[input][side];
                                if path_route_cell(
                                    ui,
                                    routed,
                                    input,
                                    side,
                                    &info.name,
                                    path_colour(info.colour_index),
                                ) {
                                    routes[input][side] = !routed;
                                    self.set_path_routes(info.slot, routes);
                                    self.push_paths_to_engine();
                                    self.push_log(
                                        format!(
                                            "Route {role} -> {} {} {}.",
                                            info.name,
                                            if side == 0 { "L" } else { "R" },
                                            if routed { "muted" } else { "enabled" }
                                        ),
                                        if routed {
                                            LogTone::Muted
                                        } else {
                                            LogTone::Teal
                                        },
                                    );
                                }
                            }
                            ui.add_space(gap);
                        }
                    });
                    ui.add_space(5.0);
                }
            });

        ui.add_space(8.0);
        horizontal_line(ui, BORDER_SOFT);
        ui.add_space(8.0);
        ui.horizontal_top(|ui| {
            ui.label(RichText::new("Tip").size(ts(9.5)).color(AMBER).strong());
            ui.add(
                egui::Label::new(
                    RichText::new(
                        "Mono guitar on IN 1? Route it to L and R of every path that should hear it, or use that path's Mono sum.",
                    )
                    .size(ts(10.0))
                    .color(MUTED),
                )
                .wrap(),
            );
        });
    }

    /// Channel mode and monitoring: what happens on the mix bus.
    fn routing_mix_body(&mut self, ui: &mut egui::Ui, infos: &[PathInfo]) {
        ui.label(micro_label("MIX", AMBER));
        ui.add_space(8.0);
        ui.label(micro_label("CHANNEL MODE", DIM));
        ui.add_space(4.0);
        let multi = infos.len() > 1;
        if multi {
            // One row per path: each path has its own mono sum.
            for info in infos {
                let Some(mut mono) = self.paths[info.slot].as_ref().map(|path| path.mix.mono)
                else {
                    continue;
                };
                ui.horizontal(|ui| {
                    status_dot(ui, path_colour(info.colour_index));
                    let font = egui::FontId::new(ts(10.0), FontFamily::Proportional);
                    let name = crate::ui::widgets::fit_text(ui, &info.name, &font, 70.0);
                    let (rect, _) = ui.allocate_exact_size(Vec2::new(72.0, 18.0), Sense::hover());
                    ui.painter()
                        .text(rect.left_center(), Align2::LEFT_CENTER, name, font, TEXT);
                    if tab_button_sized(ui, "Stereo", !mono, BLUE, 9.5).clicked() {
                        mono = false;
                    }
                    if tab_button_sized(ui, "Mono sum", mono, BLUE, 9.5).clicked() {
                        mono = true;
                    }
                });
                self.set_path_mono(info.slot, mono);
                ui.add_space(2.0);
            }
        } else {
            let mut mono = self.path().mix.mono;
            ui.horizontal(|ui| {
                if tab_button_sized(ui, "Stereo", !mono, BLUE, 11.0).clicked() {
                    mono = false;
                }
                if tab_button_sized(ui, "Mono sum", mono, BLUE, 11.0).clicked() {
                    mono = true;
                }
            });
            if mono != self.path().mix.mono {
                self.set_edited_mono(mono);
            }
        }
        ui.add_space(10.0);
        ui.label(micro_label("MONITORING", DIM));
        ui.add_space(4.0);
        ui.checkbox(
            &mut self.monitor,
            RichText::new("Direct monitor input")
                .size(ts(10.5))
                .color(MUTED),
        );
        ui.add_space(6.0);
        let mut drift = self.drift;
        if ui
            .checkbox(
                &mut drift,
                RichText::new("Automatic drift correction")
                    .size(ts(10.5))
                    .color(MUTED),
            )
            .changed()
        {
            self.drift = drift;
        }
        ui.add_space(10.0);
        horizontal_line(ui, BORDER_SOFT);
        ui.add_space(8.0);
        ui.horizontal_top(|ui| {
            status_dot(ui, TEAL);
            ui.add(
                egui::Label::new(
                    RichText::new(if multi {
                        "Every layer applies live. Layer 1 shapes what each path's chain hears, the paths run their chains, and layer 2 decides which device channels play each path."
                    } else {
                        "Both layers apply live. Layer 1 shapes what the chain hears; layer 2 only decides which device channels play the finished mix."
                    })
                    .size(ts(9.5))
                    .color(MUTED),
                )
                .wrap(),
            );
        });
    }
}

#[cfg(test)]
mod tests;
