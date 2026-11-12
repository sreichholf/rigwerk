//! The multi-path parts of Signal Routing: the routing cards (starting
//! points), the signal flow strip, and the Paths layer (one card per path).
//!
//! Everything here only draws and queues [`RoutingAction`]s; the page
//! applies them after drawing (`RigWerkApp::apply_routing_action`).

use eframe::egui;
use egui::{Align, Align2, Color32, CornerRadius, FontFamily, FontId, Layout, Sense, Stroke, Vec2};

use crate::app::alignment::DeviceAlignment;
use crate::app::cards::Card;
use crate::app::paths::PathInfo;
use crate::app::RigWerkApp;
use crate::audio::{MAX_PATHS, PATH_LEVEL_MAX_DB, PATH_LEVEL_MIN_DB, PATH_TRIM_MAX_MS};
use crate::ui::theme::ts;
use crate::ui::theme::{
    with_alpha, AMBER, BLUE, BORDER, BORDER_SOFT, CARD, CARD_DEEP, DIM, MUTED, RED, TEAL, TEXT,
};
use crate::ui::widgets::{
    card, fit_text, micro_label, mono, path_level_text, path_toggle, row_info, show_focus_ring,
};

use super::outputs::RoutingAction;
use crate::ui::theme::path_colour;

/// Under the cards.
pub(crate) const CARDS_NOTE: &str =
    "A card sets up paths and routing. It never deletes inserts; missing paths are added.";

/// Height of one path card (and of the Add path tile).
const PATH_CARD_H: f32 = 176.0;

/// The mono line under a card's title.
pub(crate) fn card_caption(card: Card) -> &'static str {
    match card {
        Card::Single => "today · one chain",
        Card::DryWet => "split · no sum",
        Card::Blend => "sum · clean under drive",
        Card::DualAmp => "sum · two amps",
        Card::PerDevice => "each device own path",
    }
}

/// The tag shown when the routing matches no card.
pub(crate) fn custom_label(last: Option<Card>) -> String {
    match last {
        Some(card) => format!("Custom · edited from {}", card.label()),
        None => "Custom".to_owned(),
    }
}

/// The flow strip's Align node, from every output device's alignment:
/// the most it adds anywhere, how many devices have it off, or nothing to
/// align.
pub(crate) fn align_status(devices: &[DeviceAlignment]) -> String {
    let added = devices
        .iter()
        .filter(|device| device.align)
        .map(|device| device.added_ms)
        .fold(0.0, f32::max);
    let off = devices
        .iter()
        .filter(|device| !device.align && device.path_count() > 1)
        .count();
    let mut text = if added >= 0.05 {
        format!("up to +{added:.1} ms")
    } else {
        "nothing to align".to_owned()
    };
    if off > 0 {
        text.push_str(&format!(
            " · off on {off} device{}",
            if off == 1 { "" } else { "s" }
        ));
    }
    text
}

/// The × remove button: two drawn strokes, legible at any font.
pub(crate) fn remove_button(ui: &mut egui::Ui, name: &str) -> bool {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(24.0), Sense::click());
    let hovered = response.hovered();
    ui.painter().rect_filled(
        rect,
        CornerRadius::same(5),
        if hovered {
            with_alpha(RED, 30)
        } else {
            Color32::TRANSPARENT
        },
    );
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(5),
        Stroke::new(
            1.0,
            if hovered {
                with_alpha(RED, 115)
            } else {
                BORDER
            },
        ),
        egui::StrokeKind::Inside,
    );
    let color = if hovered { RED } else { MUTED };
    let arm = 4.5;
    let center = rect.center();
    for (from, to) in [
        (Vec2::new(-arm, -arm), Vec2::new(arm, arm)),
        (Vec2::new(-arm, arm), Vec2::new(arm, -arm)),
    ] {
        ui.painter()
            .line_segment([center + from, center + to], Stroke::new(1.5, color));
    }
    show_focus_ring(ui, &response, rect, 5);
    let response = response.on_hover_text(name);
    row_info(
        &response,
        egui::Role::Button,
        ui.is_enabled(),
        name,
        None,
        None,
    );
    response.clicked()
}

/// One step of a trim stepper, clamped to ±[`PATH_TRIM_MAX_MS`] and kept
/// on 0.01 ms.
pub(crate) fn step_trim(trim_ms: f32, step: f32) -> f32 {
    ((trim_ms + step).clamp(-PATH_TRIM_MAX_MS, PATH_TRIM_MAX_MS) * 100.0).round() / 100.0
}

/// A trim as the stepper shows it, to the 0.01 ms it steps by: "0.00 ms",
/// "+1.25 ms", "−0.50 ms".
pub(crate) fn trim_text(trim_ms: f32) -> String {
    if trim_ms.abs() < 0.005 {
        "0.00 ms".to_owned()
    } else if trim_ms > 0.0 {
        format!("+{trim_ms:.2} ms")
    } else {
        format!("−{:.2} ms", -trim_ms)
    }
}

/// One step of a level stepper, clamped to the path level range.
pub(crate) fn step_level(level_db: f32, step: f32) -> f32 {
    let next = if level_db <= PATH_LEVEL_MIN_DB && step > 0.0 {
        // From −∞ the first step up lands on a real level.
        PATH_LEVEL_MIN_DB + step
    } else {
        level_db + step
    };
    next.clamp(PATH_LEVEL_MIN_DB, PATH_LEVEL_MAX_DB)
}

/// The stage names of one path's chain, in signal order: the built-in
/// stages that are on and every insert.
pub(crate) fn chain_stage_names(app: &RigWerkApp, slot: usize) -> Vec<String> {
    let Some(path) = app.paths.get(slot).and_then(Option::as_ref) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    if path.mix.eqs[0].is_active() {
        names.push("Pre EQ".to_owned());
    }
    for view in app.engine.plugin_views(slot).iter() {
        names.push(view.name.clone());
    }
    if path.mix.gate.enabled {
        names.push("Gate".to_owned());
    }
    if path.mix.eqs[1].is_active() {
        names.push("Post EQ".to_owned());
    }
    names
}

/// What a level stepper did this frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct LevelStep {
    pub(crate) level_db: f32,
    /// A ± click, a double-click or the end of a drag: the level is final
    /// (worth logging).  `false` while a drag goes on.
    pub(crate) commit: bool,
}

/// How a "− value +" stepper steps, reads and resets.
struct StepSpec {
    /// One ± click.
    step: f32,
    /// Per dragged point.
    drag: f32,
    /// What a double-click sets.
    reset: f32,
    apply: fn(f32, f32) -> f32,
    text: fn(f32) -> String,
    hint: &'static str,
}

/// A "− value +" level stepper (1 dB steps; drag the value for finer
/// steps, double-click it for 0 dB).  Returns the new level on a change,
/// and the final level when a drag ends.
pub(crate) fn level_stepper(ui: &mut egui::Ui, level_db: f32, name: &str) -> Option<LevelStep> {
    stepper(
        ui,
        level_db,
        name,
        &StepSpec {
            step: 1.0,
            drag: 0.1,
            reset: 0.0,
            apply: step_level,
            text: path_level_text,
            hint: "Drag to fine-tune, double-click for 0 dB",
        },
    )
}

/// A "− value +" alignment trim stepper (0.1 ms steps, ±5 ms; drag for
/// 0.01 ms, double-click for 0).  Same results as [`level_stepper`].
pub(crate) fn trim_stepper(ui: &mut egui::Ui, trim_ms: f32, name: &str) -> Option<LevelStep> {
    stepper(
        ui,
        trim_ms,
        name,
        &StepSpec {
            step: 0.1,
            drag: 0.01,
            reset: 0.0,
            apply: step_trim,
            text: trim_text,
            hint:
                "Extra delay on top of the alignment (±5 ms). Drag to fine-tune, double-click for 0",
        },
    )
}

fn stepper(ui: &mut egui::Ui, level_db: f32, name: &str, spec: &StepSpec) -> Option<LevelStep> {
    let mut next = None;
    let mut commit = false;
    let height = 22.0;
    let button_w = 20.0;
    let value_w = 62.0;
    let (rect, base) =
        ui.allocate_exact_size(Vec2::new(button_w * 2.0 + value_w, height), Sense::hover());
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(6),
        Stroke::new(1.0, BORDER),
        egui::StrokeKind::Inside,
    );
    let minus = egui::Rect::from_min_size(rect.min, Vec2::new(button_w, height));
    let value = egui::Rect::from_min_size(
        rect.min + Vec2::new(button_w, 0.0),
        Vec2::new(value_w, height),
    );
    let plus = egui::Rect::from_min_size(
        rect.min + Vec2::new(button_w + value_w, 0.0),
        Vec2::new(button_w, height),
    );
    for (area, glyph, step, label) in [
        (minus, "−", -spec.step, "down"),
        (plus, "+", spec.step, "up"),
    ] {
        let response = ui.interact(area, base.id.with(label), Sense::click());
        if response.hovered() {
            ui.painter()
                .rect_filled(area.shrink(1.0), CornerRadius::same(5), with_alpha(TEXT, 8));
        }
        ui.painter().text(
            area.center(),
            Align2::CENTER_CENTER,
            glyph,
            FontId::new(ts(9.5), FontFamily::Monospace),
            if response.hovered() { TEXT } else { MUTED },
        );
        row_info(
            &response,
            egui::Role::Button,
            ui.is_enabled(),
            format!("{name} {label}"),
            None,
            None,
        );
        if response.clicked() {
            next = Some((spec.apply)(level_db, step));
            commit = true;
        }
    }
    let response = ui.interact(value, base.id.with("value"), Sense::click_and_drag());
    if response.dragged() {
        let delta = response.drag_delta().x * spec.drag;
        if delta != 0.0 {
            next = Some((spec.apply)(next.unwrap_or(level_db), delta));
        }
    }
    if response.double_clicked() {
        next = Some(spec.reset);
        commit = true;
    }
    if response.drag_stopped() {
        next = Some(next.unwrap_or(level_db));
        commit = true;
    }
    let shown = next.unwrap_or(level_db);
    ui.painter().text(
        value.center(),
        Align2::CENTER_CENTER,
        (spec.text)(shown),
        FontId::new(ts(9.0), FontFamily::Monospace),
        TEXT,
    );
    let response = response.on_hover_text(spec.hint);
    row_info(
        &response,
        egui::Role::SpinButton,
        ui.is_enabled(),
        name.to_owned(),
        None,
        Some((spec.text)(shown)),
    );
    next.map(|level_db| LevelStep { level_db, commit })
}

/// A small filled button in `color` (Open in Mixer, Duplicate).
pub(crate) fn small_button(ui: &mut egui::Ui, label: &str, color: Color32) -> egui::Response {
    ui.add(
        egui::Button::new(egui::RichText::new(label).size(ts(9.5)).color(color))
            .fill(CARD_DEEP)
            .stroke(Stroke::new(1.0, BORDER))
            .corner_radius(CornerRadius::same(7)),
    )
}

/// One starting-point card.  Returns whether it was clicked.
fn card_pick(ui: &mut egui::Ui, card: Card, on: bool) -> bool {
    let size = Vec2::new(158.0, 46.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let painter = ui.painter();
    let fill = if on {
        with_alpha(TEAL, 16)
    } else if response.hovered() {
        with_alpha(TEXT, 8)
    } else {
        CARD_DEEP
    };
    painter.rect_filled(rect, CornerRadius::same(9), fill);
    painter.rect_stroke(
        rect,
        CornerRadius::same(9),
        Stroke::new(1.0, if on { with_alpha(TEAL, 90) } else { BORDER }),
        egui::StrokeKind::Inside,
    );
    painter.text(
        rect.left_top() + Vec2::new(12.0, 15.0),
        Align2::LEFT_CENTER,
        card.label(),
        FontId::new(ts(11.0), FontFamily::Proportional),
        if on { TEAL } else { TEXT },
    );
    let caption = card_caption(card);
    painter.text(
        rect.left_top() + Vec2::new(12.0, 32.0),
        Align2::LEFT_CENTER,
        caption,
        FontId::new(ts(8.5), FontFamily::Monospace),
        MUTED,
    );
    show_focus_ring(ui, &response, rect, 9);
    let response = response.on_hover_text(card_hint(card));
    row_info(
        &response,
        egui::Role::Button,
        true,
        format!("{} card", card.label()),
        Some(on),
        Some(caption.to_owned()),
    );
    response.clicked()
}

/// A card's tooltip.
pub(crate) fn card_hint(card: Card) -> &'static str {
    match card {
        Card::Single => "One path on Out 1/2: the routing before paths existed.",
        Card::DryWet => "The first path on the left channels, the second on the right. Nothing adds up.",
        Card::Blend => {
            "A copy of the first path, both summed: the clean one at −6 dB under the driven one. The devices align them."
        }
        Card::DualAmp => {
            "A copy of the first path, both summed, the second with its polarity flipped. Put a different amp on each; the devices align them."
        }
        Card::PerDevice => "The first path on the clock device, the second on every follower.",
    }
}

/// A flow strip node: a mono key over a value, with an optional colour
/// bar on the left (a path).
fn flow_node(ui: &mut egui::Ui, key: &str, key_color: Color32, value: &str, bar: Option<Color32>) {
    let key_font = FontId::new(ts(7.5), FontFamily::Monospace);
    let value_font = FontId::new(ts(10.5), FontFamily::Proportional);
    let key_w = ui
        .painter()
        .layout_no_wrap(key.to_owned(), key_font.clone(), key_color)
        .size()
        .x;
    let value_w = ui
        .painter()
        .layout_no_wrap(value.to_owned(), value_font.clone(), TEXT)
        .size()
        .x;
    let height = if key.is_empty() { 22.0 } else { 36.0 };
    let (rect, _) =
        ui.allocate_exact_size(Vec2::new(key_w.max(value_w) + 22.0, height), Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::same(7), CARD_DEEP);
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(7),
        Stroke::new(1.0, BORDER),
        egui::StrokeKind::Inside,
    );
    if let Some(color) = bar {
        let strip = egui::Rect::from_min_size(rect.min, Vec2::new(3.0, rect.height()));
        ui.painter().rect_filled(
            strip,
            CornerRadius {
                nw: 7,
                sw: 7,
                ne: 0,
                se: 0,
            },
            color,
        );
    }
    if key.is_empty() {
        ui.painter().text(
            rect.left_center() + Vec2::new(11.0, 0.0),
            Align2::LEFT_CENTER,
            value,
            value_font,
            TEXT,
        );
    } else {
        ui.painter().text(
            rect.left_top() + Vec2::new(11.0, 11.0),
            Align2::LEFT_CENTER,
            key,
            key_font,
            key_color,
        );
        ui.painter().text(
            rect.left_top() + Vec2::new(11.0, 25.0),
            Align2::LEFT_CENTER,
            value,
            value_font,
            TEXT,
        );
    }
}

fn flow_arrow(ui: &mut egui::Ui) {
    ui.label(mono("→", MUTED, 10.0));
}

impl RigWerkApp {
    /// The starting-point cards, the current one highlighted, "Custom"
    /// when the routing matches none.
    pub(super) fn routing_cards_row(
        &self,
        ui: &mut egui::Ui,
        width: f32,
        actions: &mut Vec<RoutingAction>,
    ) {
        let current = self.current_card();
        card(ui, CARD, BORDER, |ui| {
            ui.set_width(width - 32.0);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
                let (rect, _) = ui.allocate_exact_size(Vec2::new(78.0, 46.0), Sense::hover());
                ui.painter().text(
                    rect.left_center(),
                    Align2::LEFT_CENTER,
                    "START FROM",
                    FontId::new(ts(9.0), FontFamily::Proportional),
                    MUTED,
                );
                for card in Card::ALL {
                    if card_pick(ui, card, current == Some(card)) {
                        actions.push(RoutingAction::ApplyCard(card));
                    }
                }
                if current.is_none() {
                    let text = custom_label(self.routing_last_card);
                    let galley = ui.painter().layout_no_wrap(
                        text,
                        FontId::new(ts(9.0), FontFamily::Monospace),
                        AMBER,
                    );
                    let width = galley.size().x + 18.0;
                    let (rect, response) =
                        ui.allocate_exact_size(Vec2::new(width, 46.0), Sense::hover());
                    let tag = egui::Rect::from_center_size(rect.center(), Vec2::new(width, 22.0));
                    ui.painter().rect_stroke(
                        tag,
                        CornerRadius::same(6),
                        Stroke::new(1.0, with_alpha(AMBER, 100)),
                        egui::StrokeKind::Inside,
                    );
                    ui.painter()
                        .galley(tag.center() - galley.size() / 2.0, galley, AMBER);
                    let _ = response.on_hover_text("The routing was edited and matches no card.");
                }
                // The note fills what is left of the row, right-aligned; on
                // a narrow page it is left out rather than wrapped below.
                let note_w = ui.max_rect().right() - ui.cursor().min.x - 1.0;
                if note_w > 200.0 {
                    let (rect, _) = ui.allocate_exact_size(Vec2::new(note_w, 46.0), Sense::hover());
                    let mut note = ui.new_child(
                        egui::UiBuilder::new()
                            .max_rect(egui::Rect::from_min_max(
                                egui::pos2(rect.right() - note_w.min(260.0), rect.top()),
                                rect.max,
                            ))
                            .layout(Layout::top_down(Align::Max).with_main_align(Align::Center)),
                    );
                    note.add(
                        egui::Label::new(
                            egui::RichText::new(CARDS_NOTE).size(ts(9.5)).color(MUTED),
                        )
                        .halign(Align::Max)
                        .wrap(),
                    );
                }
            });
        });
    }

    /// Input -> route ① -> paths -> route ② -> master, in one strip.  Only
    /// with more than one path.
    pub(super) fn routing_flow(&self, ui: &mut egui::Ui, width: f32, infos: &[PathInfo]) {
        let input = self.active_input().name.clone();
        let devices = self.output_device_rows().len();
        let master = if self.mute {
            "muted · shared".to_owned()
        } else {
            format!("{:+.1} dB · shared", self.output_gain)
        };
        card(ui, CARD, BORDER, |ui| {
            ui.set_width(width - 32.0);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = Vec2::new(8.0, 6.0);
                ui.label(micro_label("SIGNAL PATH", MUTED));
                ui.add_space(4.0);
                flow_node(ui, "INPUT", TEAL, &input, None);
                flow_arrow(ui);
                flow_node(
                    ui,
                    "ROUTE 1",
                    TEAL,
                    &format!("Input -> {} paths", infos.len()),
                    None,
                );
                flow_arrow(ui);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 4.0;
                    for info in infos {
                        flow_node(
                            ui,
                            "",
                            MUTED,
                            &format!("{} · {}", info.name, info.summary()),
                            Some(path_colour(info.colour_index)),
                        );
                    }
                });
                flow_arrow(ui);
                let alignments: Vec<DeviceAlignment> = (0..devices)
                    .filter_map(|device| self.device_alignment(device))
                    .collect();
                let status = align_status(&alignments);
                let color = if status == "nothing to align" {
                    DIM
                } else {
                    BLUE
                };
                flow_node(ui, "ALIGN", color, &status, None);
                flow_arrow(ui);
                flow_node(
                    ui,
                    "ROUTE 2",
                    BLUE,
                    &format!(
                        "Paths -> {devices} device{}",
                        if devices == 1 { "" } else { "s" }
                    ),
                    None,
                );
                flow_arrow(ui);
                flow_node(ui, "MASTER", BLUE, &master, None);
            });
        });
    }

    /// The Paths layer: one card per path, plus Add path.
    pub(super) fn routing_paths_card(
        &self,
        ui: &mut egui::Ui,
        width: f32,
        infos: &[PathInfo],
        actions: &mut Vec<RoutingAction>,
    ) {
        let can_add = self.can_add_path();
        let full = infos.len() >= MAX_PATHS;
        card(ui, CARD, BORDER, |ui| {
            let inner = width - 32.0;
            ui.set_width(inner);
            ui.horizontal(|ui| {
                ui.label(micro_label("PATHS", AMBER));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(mono(
                        format!("{} / {MAX_PATHS} · EACH A FULL MIXER CHAIN", infos.len()),
                        DIM,
                        9.5,
                    ));
                });
            });
            ui.add_space(10.0);
            let gap = 10.0;
            let columns = if inner >= 4.0 * 210.0 + 3.0 * gap {
                4
            } else if inner >= 2.0 * 210.0 + gap {
                2
            } else {
                1
            };
            let cell_w = (inner - gap * (columns as f32 - 1.0)) / columns as f32;
            let tiles = infos.len() + usize::from(!full);
            let mut index = 0;
            while index < tiles {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = gap;
                    for _ in 0..columns {
                        if index >= tiles {
                            break;
                        }
                        let size = Vec2::new(cell_w, PATH_CARD_H);
                        ui.allocate_ui_with_layout(size, Layout::top_down(Align::LEFT), |ui| {
                            ui.set_min_size(size);
                            match infos.get(index) {
                                Some(info) => {
                                    self.path_card(ui, size, info, infos.len(), can_add, actions)
                                }
                                None => add_path_tile(ui, size, can_add, actions),
                            }
                        });
                        index += 1;
                    }
                });
                ui.add_space(gap - ui.spacing().item_spacing.y.min(gap));
            }
        });
    }

    fn path_card(
        &self,
        ui: &mut egui::Ui,
        size: Vec2,
        info: &PathInfo,
        count: usize,
        can_add: bool,
        actions: &mut Vec<RoutingAction>,
    ) {
        let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
        ui.painter()
            .rect_filled(rect, CornerRadius::same(9), CARD_DEEP);
        ui.painter().rect_stroke(
            rect,
            CornerRadius::same(9),
            Stroke::new(
                1.0,
                if info.selected {
                    with_alpha(path_colour(info.colour_index), 70)
                } else {
                    BORDER_SOFT
                },
            ),
            egui::StrokeKind::Inside,
        );
        let bar = egui::Rect::from_min_size(rect.min, Vec2::new(rect.width(), 3.0));
        ui.painter().rect_filled(
            bar,
            CornerRadius {
                nw: 9,
                ne: 9,
                sw: 0,
                se: 0,
            },
            path_colour(info.colour_index),
        );
        let content = rect
            .shrink2(Vec2::new(12.0, 11.0))
            .translate(Vec2::new(0.0, 2.0));
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(content)
                .layout(Layout::top_down(Align::LEFT)),
        );
        child.set_clip_rect(rect);
        let ui = &mut child;
        if !info.audible {
            ui.multiply_opacity(0.6);
        }
        ui.spacing_mut().item_spacing.y = 8.0;

        // Name, mute and solo.
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let (dot, _) = ui.allocate_exact_size(Vec2::new(10.0, 22.0), Sense::hover());
            ui.painter()
                .circle_filled(dot.center(), 4.0, path_colour(info.colour_index));
            ui.add_space(4.0);
            let font = FontId::new(ts(12.5), FontFamily::Proportional);
            let name = fit_text(
                ui,
                &info.name,
                &font,
                (ui.available_width() - 56.0).max(30.0),
            );
            ui.label(egui::RichText::new(name).font(font).color(TEXT))
                .on_hover_text(&info.name);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if path_toggle(
                    ui,
                    "S",
                    info.solo,
                    AMBER,
                    &format!("Solo {}", info.name),
                    "While any path is soloed, only soloed paths sound.",
                ) {
                    actions.push(RoutingAction::PathSolo(info.slot, !info.solo));
                }
                if path_toggle(
                    ui,
                    "M",
                    info.mute,
                    RED,
                    &format!("Mute {}", info.name),
                    "Silence this path on every device.",
                ) {
                    actions.push(RoutingAction::PathMute(info.slot, !info.mute));
                }
            });
        });

        // The chain, as stage chips.
        let stages = chain_stage_names(self, info.slot);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            if stages.is_empty() {
                chip(ui, "empty chain", DIM, true);
            }
            let room = ui.available_width();
            let mut used = 0.0;
            let font = FontId::new(ts(8.0), FontFamily::Monospace);
            for (index, stage) in stages.iter().enumerate() {
                let width = ui
                    .painter()
                    .layout_no_wrap(stage.clone(), font.clone(), MUTED)
                    .size()
                    .x
                    + 14.0;
                if used + width > room - 34.0 && index + 1 < stages.len() {
                    chip(ui, &format!("+{}", stages.len() - index), DIM, false);
                    break;
                }
                used += width;
                chip(ui, stage, MUTED, false);
            }
        });
        ui.label(mono(info.summary(), DIM, 8.5));

        // Path level.
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Path level").size(ts(9.5)).color(MUTED));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if let Some(step) =
                    level_stepper(ui, info.level_db, &format!("{} path level", info.name))
                {
                    actions.push(RoutingAction::PathLevel(info.slot, step.level_db));
                }
            });
        });

        // Open, duplicate, remove.
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let open = small_button(ui, "Open in Mixer", TEAL)
                .on_hover_text(format!("Edit {}'s chain in the Mixer", info.name));
            row_info(
                &open,
                egui::Role::Button,
                true,
                format!("Open {} in Mixer", info.name),
                None,
                None,
            );
            if open.clicked() {
                actions.push(RoutingAction::OpenPathInMixer(info.slot));
            }
            let duplicate = ui
                .add_enabled_ui(can_add, |ui| small_button(ui, "Duplicate", TEXT))
                .inner
                .on_hover_text("A full copy of this path, added muted")
                .on_disabled_hover_text(if count >= MAX_PATHS {
                    "At most four paths"
                } else {
                    "A removed path is still fading out"
                });
            row_info(
                &duplicate,
                egui::Role::Button,
                can_add,
                format!("Duplicate {}", info.name),
                None,
                None,
            );
            if duplicate.clicked() {
                actions.push(RoutingAction::DuplicatePath(info.slot));
            }
            if count > 1 {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if remove_button(ui, &format!("Remove {}", info.name)) {
                        actions.push(RoutingAction::RemovePath(info.slot));
                    }
                });
            }
        });
    }
}

/// A chain chip: a mono stage name in a small outlined box.
fn chip(ui: &mut egui::Ui, text: &str, color: Color32, dashed: bool) {
    let font = FontId::new(ts(8.0), FontFamily::Monospace);
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, color);
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(galley.size().x + 10.0, 18.0), Sense::hover());
    let stroke = Stroke::new(1.0, if dashed { BORDER_SOFT } else { BORDER });
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(4),
        stroke,
        egui::StrokeKind::Inside,
    );
    ui.painter()
        .galley(rect.center() - galley.size() / 2.0, galley, color);
    let _ = response.on_hover_text(text);
}

/// The dashed "+ Add path" tile.
fn add_path_tile(ui: &mut egui::Ui, size: Vec2, enabled: bool, actions: &mut Vec<RoutingAction>) {
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(size, sense);
    let hovered = enabled && response.hovered();
    let color = if !enabled {
        with_alpha(MUTED, 100)
    } else if hovered {
        TEAL
    } else {
        MUTED
    };
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(9),
        Stroke::new(
            1.0,
            if hovered {
                with_alpha(TEAL, 90)
            } else {
                BORDER
            },
        ),
        egui::StrokeKind::Inside,
    );
    let center = rect.center();
    ui.painter().text(
        center - Vec2::new(0.0, 18.0),
        Align2::CENTER_CENTER,
        "+",
        FontId::new(ts(16.0), FontFamily::Proportional),
        color,
    );
    ui.painter().text(
        center + Vec2::new(0.0, 4.0),
        Align2::CENTER_CENTER,
        "Add path",
        FontId::new(ts(11.0), FontFamily::Proportional),
        color,
    );
    ui.painter().text(
        center + Vec2::new(0.0, 22.0),
        Align2::CENTER_CENTER,
        "empty chain · added muted",
        FontId::new(ts(8.0), FontFamily::Monospace),
        if enabled { DIM } else { with_alpha(DIM, 100) },
    );
    show_focus_ring(ui, &response, rect, 9);
    let response = if enabled {
        response.on_hover_text("Add an empty path, muted and routed nowhere")
    } else {
        response.on_hover_text("A removed path is still fading out")
    };
    row_info(
        &response,
        egui::Role::Button,
        enabled,
        "Add path",
        None,
        None,
    );
    if enabled && response.clicked() {
        actions.push(RoutingAction::AddPath);
    }
}

#[cfg(test)]
mod tests;
