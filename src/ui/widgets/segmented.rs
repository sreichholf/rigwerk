//! A segmented control for exclusive choices (`docs/ui-spec.md` §4.5).  The
//! chosen segment is filled with `accent_bg` and labelled in `accent_text`.
//! Left and right arrows move the choice while a segment has focus.

use eframe::egui;
use egui::{CornerRadius, Rect, Sense, Stroke, Vec2};

use crate::ui::fonts::Face;
use crate::ui::theme::{font, palette, TypeRole};
use crate::ui::widgets::{palette_focus, row_info, text_width};

const HEIGHT: f32 = 32.0;

/// Paints `options` and writes the chosen index into `selected`.
pub(crate) fn segmented(
    ui: &mut egui::Ui,
    selected: &mut usize,
    options: &[&str],
) -> egui::Response {
    paint(ui, selected, options, &[])
}

/// [`segmented`] with a hover text per option; an option without one is
/// explained by its label, as in [`segmented`].
pub(crate) fn segmented_hinted(
    ui: &mut egui::Ui,
    selected: &mut usize,
    options: &[&str],
    hints: &[&str],
) -> egui::Response {
    paint(ui, selected, options, hints)
}

fn paint(
    ui: &mut egui::Ui,
    selected: &mut usize,
    options: &[&str],
    hints: &[&str],
) -> egui::Response {
    if options.is_empty() {
        return ui.allocate_response(Vec2::ZERO, Sense::hover());
    }
    *selected = (*selected).min(options.len() - 1);

    let text_font = font(Face::Sans, TypeRole::Control);
    let widths: Vec<f32> = options
        .iter()
        .map(|option| text_width(ui, option, &text_font) + 24.0)
        .collect();
    let width = widths.iter().sum::<f32>().max(HEIGHT);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, HEIGHT), Sense::hover());

    let colours = palette(ui.ctx());
    ui.painter()
        .rect_filled(rect, CornerRadius::same(4), colours.slot);
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(4),
        Stroke::new(1.0, colours.line2),
        egui::StrokeKind::Inside,
    );

    let mut changed = false;
    let mut combined: Option<egui::Response> = None;
    let mut x = rect.left();
    for (index, option) in options.iter().enumerate() {
        let segment =
            Rect::from_min_size(egui::pos2(x, rect.top()), Vec2::new(widths[index], HEIGHT));
        x += widths[index];
        let mut response = ui.interact(
            segment,
            ui.scope_id().with(("segment", index, *option)),
            Sense::click(),
        );
        let on = index == *selected;
        if on {
            ui.painter().rect_filled(
                segment.shrink(1.0),
                CornerRadius::same(3),
                colours.accent_bg,
            );
        }
        if index > 0 {
            ui.painter().vline(
                segment.left(),
                rect.y_range(),
                Stroke::new(1.0, colours.line),
            );
        }
        ui.painter().text(
            segment.center(),
            egui::Align2::CENTER_CENTER,
            *option,
            text_font.clone(),
            if on {
                colours.accent_text
            } else {
                colours.text
            },
        );
        if response.clicked() && !on {
            *selected = index;
            changed = true;
        }
        changed |= arrows(ui, &response, selected, index, options.len());
        row_info(
            &response,
            egui::Role::RadioButton,
            ui.is_enabled(),
            *option,
            Some(on),
            None,
        );
        palette_focus(ui, &response, segment);
        response = response.on_hover_text(hint(hints, index, option));
        combined = Some(match combined {
            Some(previous) => previous.union(response),
            None => response,
        });
    }
    let mut response = combined.expect("at least one segment");
    if changed {
        response.mark_changed();
    }
    response
}

/// The hover text for the option at `index`: its hint when one was given,
/// else the label itself.
fn hint<'a>(hints: &[&'a str], index: usize, option: &'a str) -> &'a str {
    hints.get(index).copied().unwrap_or(option)
}

fn arrows(
    ui: &egui::Ui,
    response: &egui::Response,
    selected: &mut usize,
    index: usize,
    count: usize,
) -> bool {
    if !response.has_focus() {
        return false;
    }
    ui.memory_mut(|memory| {
        memory.set_focus_lock_filter(
            response.id,
            egui::EventFilter {
                horizontal_arrows: true,
                ..Default::default()
            },
        );
    });
    let (left, right) = ui.input(|input| {
        (
            input.key_pressed(egui::Key::ArrowLeft),
            input.key_pressed(egui::Key::ArrowRight),
        )
    });
    if right && index + 1 < count {
        *selected = index + 1;
        true
    } else if left && index > 0 {
        *selected = index - 1;
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests;
