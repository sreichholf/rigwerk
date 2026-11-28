//! One chain row (`docs/ui-spec.md` §4.2).
//!
//! 28 pt high, 3 pt corners, filled with `slot`.  The LED is filled while
//! the stage is on and a ring while it is bypassed.  An EQ row can draw a
//! mini curve, a gate row its threshold and an effect row its latency.
//! Selected rows take an `accent` outline and `accent_bg`.  A stage that
//! failed to load prints its name in `error_text` with *Didn't load*.
//! A paused effect draws a pause glyph.
//!
//! Click selects.  The LED bypasses.  Double-clicking an effect asks to
//! open it.  An effect's right-click menu and Alt+↑ / Alt+↓ move it,
//! Ctrl+Enter opens its editor, and dragging it sets an [`EffectDrag`]
//! payload.  The hover on that grip says that moving an effect closes its
//! open editor.  A NAM block row moves the same way and drags as a
//! [`NamDrag`]; a built-in row only bypasses.  A `warn` row prints its mark
//! in the warning colour, which is how an overloaded NAM block shows what
//! it costs.

use eframe::egui;
use egui::{CornerRadius, Rect, Sense, Stroke, Vec2};

use crate::audio::EqSettings;
use crate::ui::fonts::Face;
use crate::ui::theme::{font, palette, TypeRole};
use crate::ui::widgets::{palette_focus, row_info};

/// Row height, fixed so a chain can scroll by rows.
pub(crate) const CHAIN_ROW_H: f32 = 28.0;

/// What a chain row did this frame, besides a drag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChainEvent {
    /// Click the row: select it and open its sheet.
    Select,
    /// Click the LED.
    Bypass,
    /// Double-click an effect: its editor or its controls.
    Open,
    Editor,
    Controls,
    MoveUp,
    MoveDown,
    Remove,
}

/// Drag payload of an effect row: its index in the chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EffectDrag(pub usize);

/// Drag payload of a NAM row: the block's index in its path's stack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NamDrag(pub usize);

/// What a row's right-click menu offers, and how it moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RowMenu {
    /// A built-in stage: bypass only.
    Builtin,
    /// A plugin effect: editor, controls, move, remove, and drag.
    Effect,
    /// A NAM block: move and remove, and drag.
    Nam,
}

/// The mark at the right of a row.
#[derive(Clone, Copy)]
pub(crate) enum ChainMark<'a> {
    /// Mini EQ curve, hidden when the row's `show_curve` is false.
    Eq(&'a EqSettings),
    /// Gate threshold, already formatted.
    Threshold(&'a str),
    /// Effect latency, already formatted.  An empty string draws nothing.
    Latency(&'a str),
    /// A built-in row's text, drawn like the threshold.  Not an effect:
    /// the row does not drag.
    Label(&'a str),
}

/// What one row shows.  The strings are borrowed for the frame.
pub(crate) struct ChainRow<'a> {
    /// What screen readers call the row, and what it shows unless `shown`
    /// says otherwise.
    pub name: &'a str,
    /// Text drawn in place of the name: a NAM row shows its capture.
    /// Screen readers get it as the description, and hover shows it whole.
    pub shown: Option<&'a str>,
    pub index: usize,
    pub on: bool,
    pub selected: bool,
    pub failed: bool,
    pub fail_reason: Option<&'a str>,
    pub paused: bool,
    pub mark: ChainMark<'a>,
    /// Density steps that drop the mini curve set this false.
    pub show_curve: bool,
    pub sample_rate: u32,
    pub menu: RowMenu,
    /// Draws the mark in the warning colour (an overloaded block).
    pub warn: bool,
}

impl ChainRow<'_> {
    fn effect(&self) -> bool {
        matches!(self.menu, RowMenu::Effect)
    }

    fn movable(&self) -> bool {
        matches!(self.menu, RowMenu::Effect | RowMenu::Nam)
    }
}

/// The row, and what it did.  Read the event with [`chain_event`] as well
/// as the [`egui::Response`] this returns: the LED, the menu and the arrow
/// keys are not a plain click.
pub(crate) fn chain_row(ui: &mut egui::Ui, row: &ChainRow<'_>) -> egui::Response {
    let width = {
        let available = ui.available_width();
        if available.is_finite() {
            available.max(80.0)
        } else {
            180.0
        }
    };
    let sense = if row.movable() {
        Sense::click_and_drag()
    } else {
        Sense::click()
    };
    // Before allocation: Enter on a focused clickable row is also a click.
    let open_editor = row.effect() && ctrl_enter(ui);
    let (rect, mut response) = ui.allocate_exact_size(Vec2::new(width, CHAIN_ROW_H), sense);
    if response.dragged() {
        response = response.on_hover_and_drag_cursor(egui::CursorIcon::Grabbing);
    }
    let led = Rect::from_center_size(
        egui::pos2(rect.left() + 14.0, rect.center().y),
        Vec2::splat(14.0),
    );
    let led_response = ui.interact(led, response.id.with("led"), Sense::click());

    let mut event = None;
    let pointer = response
        .interact_pointer_pos()
        .or(led_response.interact_pointer_pos());
    let on_led = pointer.is_some_and(|pos| led.contains(pos));
    if open_editor {
        event = Some(ChainEvent::Editor);
    } else if led_response.clicked() || (response.clicked() && on_led) {
        event = Some(ChainEvent::Bypass);
    } else if response.double_clicked() && row.effect() && !on_led {
        event = Some(ChainEvent::Open);
    } else if response.clicked() && !on_led {
        event = Some(ChainEvent::Select);
    }
    if row.movable() {
        match row.menu {
            RowMenu::Effect => response.dnd_set_drag_payload(EffectDrag(row.index)),
            RowMenu::Nam => response.dnd_set_drag_payload(NamDrag(row.index)),
            RowMenu::Builtin => {}
        }
        event = event.or(arrows(ui, &response));
    }

    let menu = match row.menu {
        RowMenu::Effect => response.context_menu(|ui| effect_menu(ui, &mut event)),
        RowMenu::Nam => response.context_menu(|ui| nam_menu(ui, &mut event)),
        RowMenu::Builtin => response.context_menu(|ui| {
            if menu_item(ui, "Bypass") {
                event = Some(ChainEvent::Bypass);
                ui.close();
            }
        }),
    };
    let _ = menu;

    paint(ui, row, rect, led);
    if row.effect() {
        let grip = Rect::from_min_size(rect.left_top(), Vec2::new(6.0, rect.height()));
        let _ = ui
            .interact(grip, response.id.with("grip"), Sense::hover())
            .on_hover_text("Moving an effect closes its open editor");
    }

    let value = if row.failed {
        "Didn't load"
    } else if row.paused {
        "Paused"
    } else if row.on {
        "On"
    } else {
        "Bypassed"
    };
    row_info(
        &response,
        egui::Role::Button,
        ui.is_enabled(),
        row.name,
        Some(row.selected),
        Some(value.to_owned()),
    );
    row_info(
        &led_response,
        egui::Role::Button,
        ui.is_enabled(),
        format!("Bypass {}", row.name),
        Some(!row.on),
        None,
    );
    if let (Some(shown), false) = (row.shown, row.failed) {
        // The text may be shortened; the whole of it is on hover.
        response = response.on_hover_text(shown);
        let shown = shown.to_owned();
        ui.ctx().accesskit_node_builder(response.id, |builder| {
            builder.set_description(shown);
        });
    }
    if let Some(reason) = row.fail_reason.filter(|_| row.failed) {
        response = response.on_hover_text(reason);
        let reason = reason.to_owned();
        ui.ctx().accesskit_node_builder(response.id, |builder| {
            builder.set_description(reason);
        });
    }
    store_event(ui, response.id, event);
    palette_focus(ui, &response, rect);
    response
}

/// The event stored for `response` this frame, if the row did something
/// other than a drag.
pub(crate) fn chain_event(response: &egui::Response) -> Option<ChainEvent> {
    response.ctx.data(|data| data.get_temp(response.id))
}

fn store_event(ui: &egui::Ui, id: egui::Id, event: Option<ChainEvent>) {
    ui.data_mut(|data| match event {
        Some(event) => {
            data.insert_temp(id, event);
        }
        None => data.remove::<ChainEvent>(id),
    });
}

/// Ctrl+Enter on the widget about to be allocated, and only while it has
/// focus.  The press is consumed so it is not also a click.
fn ctrl_enter(ui: &egui::Ui) -> bool {
    let id = ui.next_auto_id();
    let focused = ui.input(|input| input.focused) && ui.memory(|memory| memory.has_focus(id));
    focused && ui.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::Enter))
}

fn arrows(ui: &egui::Ui, response: &egui::Response) -> Option<ChainEvent> {
    if !response.has_focus() {
        return None;
    }
    ui.memory_mut(|memory| {
        memory.set_focus_lock_filter(
            response.id,
            egui::EventFilter {
                vertical_arrows: true,
                ..Default::default()
            },
        );
    });
    ui.input(|input| {
        if !input.modifiers.alt {
            return None;
        }
        if input.key_pressed(egui::Key::ArrowUp) {
            Some(ChainEvent::MoveUp)
        } else if input.key_pressed(egui::Key::ArrowDown) {
            Some(ChainEvent::MoveDown)
        } else {
            None
        }
    })
}

fn nam_menu(ui: &mut egui::Ui, event: &mut Option<ChainEvent>) {
    ui.set_min_width(160.0);
    for (label, item) in [
        ("Bypass", ChainEvent::Bypass),
        ("Move up", ChainEvent::MoveUp),
        ("Move down", ChainEvent::MoveDown),
        ("Remove", ChainEvent::Remove),
    ] {
        if menu_item(ui, label) {
            *event = Some(item);
            ui.close();
        }
    }
}

fn effect_menu(ui: &mut egui::Ui, event: &mut Option<ChainEvent>) {
    ui.set_min_width(160.0);
    for (label, item) in [
        ("Editor", ChainEvent::Editor),
        ("Controls", ChainEvent::Controls),
        ("Bypass", ChainEvent::Bypass),
        ("Move up", ChainEvent::MoveUp),
        ("Move down", ChainEvent::MoveDown),
        ("Remove", ChainEvent::Remove),
    ] {
        if menu_item(ui, label) {
            *event = Some(item);
            ui.close();
        }
    }
}

fn menu_item(ui: &mut egui::Ui, label: &str) -> bool {
    let colours = palette(ui.ctx());
    ui.add(
        egui::Button::new(
            egui::RichText::new(label)
                .font(font(Face::Sans, TypeRole::Body))
                .color(colours.text),
        )
        .fill(colours.sheet)
        .stroke(Stroke::NONE)
        .frame(false),
    )
    .clicked()
}

fn paint(ui: &egui::Ui, row: &ChainRow<'_>, rect: Rect, led: Rect) {
    let colours = palette(ui.ctx());
    let painter = ui.painter();
    let fill = if row.selected {
        colours.accent_bg
    } else {
        colours.slot
    };
    painter.rect_filled(rect, CornerRadius::same(3), fill);
    if row.selected {
        painter.rect_stroke(
            rect,
            CornerRadius::same(3),
            Stroke::new(1.5, colours.accent),
            egui::StrokeKind::Inside,
        );
    }
    let led_centre = led.center();
    if row.on && !row.failed {
        painter.circle_filled(led_centre, 4.0, colours.accent);
    } else {
        painter.circle_stroke(
            led_centre,
            3.5,
            Stroke::new(
                1.5,
                if row.failed {
                    colours.error
                } else {
                    colours.led_off
                },
            ),
        );
    }
    let mut x = led.right() + 6.0;
    if row.failed {
        paint_failed_icon(painter, egui::pos2(x + 6.0, rect.center().y), colours.error);
        x += 16.0;
    }
    let name_colour = if row.failed {
        colours.error_text
    } else {
        colours.text
    };
    let name_font = font(Face::Sans, TypeRole::Body);
    let mark_w = mark_width(ui, row);
    let name_right = rect.right() - 8.0 - mark_w - if row.paused { 14.0 } else { 0.0 };
    let name = super::fit_text(
        ui,
        row.shown.unwrap_or(row.name),
        &name_font,
        (name_right - x).max(8.0),
    );
    painter.text(
        egui::pos2(x, rect.center().y),
        egui::Align2::LEFT_CENTER,
        name,
        name_font,
        name_colour,
    );
    if row.paused {
        paint_pause(
            painter,
            egui::pos2(name_right - 10.0, rect.center().y),
            colours.text,
        );
    }
    paint_mark(ui, row, rect, colours);
}

/// The widest a text mark gets.  A longer one is shortened with "…".
const MARK_MAX_W: f32 = 96.0;

/// The text mark as drawn, before it is fitted: a failed row says so.
fn mark_text<'a>(row: &ChainRow<'a>) -> Option<&'a str> {
    match row.mark {
        ChainMark::Threshold(text) | ChainMark::Latency(text) | ChainMark::Label(text)
            if !text.is_empty() =>
        {
            Some(if row.failed { "Didn't load" } else { text })
        }
        _ => None,
    }
}

fn mark_width(ui: &egui::Ui, row: &ChainRow<'_>) -> f32 {
    match (row.mark, mark_text(row)) {
        (ChainMark::Eq(_), _) if row.show_curve => 44.0,
        (_, Some(text)) => measure(ui, text).min(MARK_MAX_W),
        _ => 0.0,
    }
}

fn measure(ui: &egui::Ui, text: &str) -> f32 {
    ui.painter()
        .layout_no_wrap(
            text.to_owned(),
            font(Face::Mono, TypeRole::Secondary),
            palette(ui.ctx()).text2,
        )
        .size()
        .x
        + 4.0
}

fn paint_mark(ui: &egui::Ui, row: &ChainRow<'_>, rect: Rect, colours: &crate::ui::theme::Palette) {
    let painter = ui.painter();
    match row.mark {
        ChainMark::Eq(eq) if row.show_curve => {
            let curve = Rect::from_min_size(
                egui::pos2(rect.right() - 48.0, rect.center().y - 8.0),
                Vec2::new(40.0, 16.0),
            );
            let ink = if row.on { colours.text } else { colours.text3 };
            painter.add(egui::Shape::line(
                curve_points(eq, row.sample_rate, curve),
                Stroke::new(1.0, ink),
            ));
        }
        ChainMark::Threshold(_) | ChainMark::Latency(_) | ChainMark::Label(_)
            if mark_text(row).is_some() =>
        {
            let mark_font = font(Face::Mono, TypeRole::Secondary);
            // `measure` pads by 4: the text gets what is left of the width.
            let shown = super::fit_text(
                ui,
                mark_text(row).unwrap_or_default(),
                &mark_font,
                mark_width(ui, row) - 4.0,
            );
            let colour = if row.failed {
                colours.error_text
            } else if row.warn {
                colours.warn
            } else {
                colours.text2
            };
            painter.text(
                egui::pos2(rect.right() - 8.0, rect.center().y),
                egui::Align2::RIGHT_CENTER,
                shown,
                mark_font,
                colour,
            );
        }
        ChainMark::Eq(_)
        | ChainMark::Threshold(_)
        | ChainMark::Latency(_)
        | ChainMark::Label(_) => {}
    }
}

/// The mini curve, 20 Hz to 20 kHz, as dB.  The painter maps it into the row.
pub(super) fn curve_db(eq: &EqSettings, sample_rate: u32) -> [f32; 24] {
    let rate = sample_rate.max(1);
    std::array::from_fn(|index| {
        let t = index as f32 / 23.0;
        let hz = 20.0 * 1_000.0_f32.powf(t);
        eq.response_db(hz, rate)
    })
}

fn curve_points(eq: &EqSettings, sample_rate: u32, rect: Rect) -> Vec<egui::Pos2> {
    curve_db(eq, sample_rate)
        .into_iter()
        .enumerate()
        .map(|(index, db)| {
            let t = index as f32 / 23.0;
            let y = rect.center().y - (db.clamp(-12.0, 12.0) / 12.0) * (rect.height() * 0.5);
            egui::pos2(rect.left() + t * rect.width(), y)
        })
        .collect()
}

fn paint_failed_icon(painter: &egui::Painter, centre: egui::Pos2, colour: egui::Color32) {
    painter.circle_stroke(centre, 5.0, Stroke::new(1.2, colour));
    painter.line_segment(
        [centre + Vec2::new(0.0, -2.5), centre + Vec2::new(0.0, 1.0)],
        Stroke::new(1.2, colour),
    );
    painter.circle_filled(centre + Vec2::new(0.0, 3.0), 0.8, colour);
}

fn paint_pause(painter: &egui::Painter, centre: egui::Pos2, colour: egui::Color32) {
    for dx in [-2.5, 1.0] {
        let bar = Rect::from_center_size(centre + Vec2::new(dx, 0.0), Vec2::new(2.0, 8.0));
        painter.rect_filled(bar, CornerRadius::same(1), colour);
    }
}

/// *Bypass all*.  Lit with the accent while `on`.
pub(crate) fn bypass_all(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
    super::chip::chip(ui, on, "Bypass all")
}

/// The effects heading: *Effects n*, the group's latency, and [`bypass_all`].
pub(crate) fn effects_header(
    ui: &mut egui::Ui,
    count: usize,
    latency: &str,
    bypassed: &mut bool,
) -> egui::Response {
    let colours = palette(ui.ctx());
    ui.horizontal(|ui| {
        let heading = ui.label(
            egui::RichText::new(format!("Effects {count}"))
                .font(font(Face::Sans, TypeRole::Body))
                .color(colours.text),
        );
        row_info(
            &heading,
            egui::Role::Heading,
            true,
            format!("Effects {count}"),
            None,
            Some(latency.to_owned()),
        );
        ui.label(
            egui::RichText::new(latency)
                .font(font(Face::Mono, TypeRole::Secondary))
                .color(colours.text2),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            bypass_all(ui, bypassed)
        })
        .inner
    })
    .inner
}

/// The dashed *+ Add effect* row.  Disabled at 32 effects, with the reason
/// on hover.
pub(crate) fn add_effect_row(ui: &mut egui::Ui, enabled: bool) -> egui::Response {
    let width = {
        let available = ui.available_width();
        if available.is_finite() {
            available.max(80.0)
        } else {
            180.0
        }
    };
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, mut response) = ui.allocate_exact_size(Vec2::new(width, CHAIN_ROW_H), sense);
    let colours = palette(ui.ctx());
    dashed_rect(ui.painter(), rect.shrink(1.0), colours.line2);
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        "+ Add effect",
        font(Face::Sans, TypeRole::Body),
        if enabled {
            colours.text2
        } else {
            colours.text3
        },
    );
    if !enabled {
        response = response.on_hover_text("32 effects is the most");
    }
    row_info(
        &response,
        egui::Role::Button,
        enabled,
        "+ Add effect",
        None,
        None,
    );
    response
}

pub(crate) fn dashed_rect(painter: &egui::Painter, rect: Rect, colour: egui::Color32) {
    let stroke = Stroke::new(1.0, colour);
    dash_h(painter, rect.left(), rect.right(), rect.top(), stroke);
    dash_h(painter, rect.left(), rect.right(), rect.bottom(), stroke);
    dash_v(painter, rect.top(), rect.bottom(), rect.left(), stroke);
    dash_v(painter, rect.top(), rect.bottom(), rect.right(), stroke);
}

fn dash_h(painter: &egui::Painter, x0: f32, x1: f32, y: f32, stroke: Stroke) {
    let mut x = x0;
    while x < x1 {
        let end = (x + 4.0).min(x1);
        painter.line_segment([egui::pos2(x, y), egui::pos2(end, y)], stroke);
        x = end + 3.0;
    }
}

fn dash_v(painter: &egui::Painter, y0: f32, y1: f32, x: f32, stroke: Stroke) {
    let mut y = y0;
    while y < y1 {
        let end = (y + 4.0).min(y1);
        painter.line_segment([egui::pos2(x, y), egui::pos2(x, end)], stroke);
        y = end + 3.0;
    }
}

#[cfg(test)]
mod tests;
