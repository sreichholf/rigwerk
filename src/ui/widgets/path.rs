//! Signal path widgets: the sidebar path entry, the path level text, the
//! mute / solo toggle, the toolbar path chip and the colour swatch.

use eframe::egui;
use egui::{Color32, CornerRadius, FontFamily, FontId, Sense, Stroke, Vec2};

use crate::ui::theme::ts;
use crate::ui::theme::{with_alpha, BORDER, BORDER_SOFT, CARD_DEEP, DIM, MUTED, TEXT};

use super::{fit_text, nav_row, row_info, show_focus_ring, NavLead};

/// A sidebar sub-entry for one signal path under Mixer: the path's colour
/// dot where the icon sits, indented, otherwise the same row as
/// [`nav_item`].
pub(crate) fn nav_path_item(
    ui: &mut egui::Ui,
    colour: Color32,
    label: &str,
    hint: &str,
    selected: bool,
) -> egui::Response {
    nav_row(ui, NavLead::Dot(colour), label, hint, selected)
}

/// A path level as the Mixer's Path stage and Signal Routing show it:
/// "+1.5 dB", "0.0 dB", "−6.0 dB", and "−∞ dB" at the floor (silent).
pub(crate) fn path_level_text(level_db: f32) -> String {
    if level_db <= crate::audio::PATH_LEVEL_MIN_DB {
        "−∞ dB".to_owned()
    } else if level_db.abs() < 0.05 {
        "0.0 dB".to_owned()
    } else if level_db > 0.0 {
        format!("+{level_db:.1} dB")
    } else {
        format!("−{:.1} dB", -level_db)
    }
}

/// A path's mute / solo (or polarity) toggle: `label` in a small outlined
/// box, filled in `colour` when on.  At least square, wider for words.
/// `name` is what a screen reader hears; `hint` shows on hover.
pub(crate) fn path_toggle(
    ui: &mut egui::Ui,
    label: &str,
    on: bool,
    colour: Color32,
    name: &str,
    hint: &str,
) -> bool {
    let text_colour = if on { colour } else { MUTED };
    let font = FontId::new(ts(9.0), FontFamily::Proportional);
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font, text_colour);
    let size = Vec2::new((galley.size().x + 14.0).max(22.0), 22.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let fill = if on {
        with_alpha(colour, 26)
    } else if response.hovered() {
        with_alpha(TEXT, 8)
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, CornerRadius::same(5), fill);
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(5),
        Stroke::new(1.0, if on { with_alpha(colour, 110) } else { BORDER }),
        egui::StrokeKind::Inside,
    );
    ui.painter()
        .galley(rect.center() - galley.size() / 2.0, galley, text_colour);
    show_focus_ring(ui, &response, rect, 5);
    let response = response.on_hover_text(hint);
    row_info(
        &response,
        egui::Role::Button,
        ui.is_enabled(),
        name,
        Some(on),
        None,
    );
    response.clicked()
}

/// One signal path as a toolbar chip: colour dot and name, at most
/// `max_width` wide (the name is cut to fit).  The selected path's chip is
/// filled in its colour.
pub(crate) fn path_chip(
    ui: &mut egui::Ui,
    colour: Color32,
    name: &str,
    muted: bool,
    selected: bool,
    max_width: f32,
) -> egui::Response {
    let font = FontId::new(ts(10.0), FontFamily::Proportional);
    let text_color = if selected {
        TEXT
    } else if muted {
        DIM
    } else {
        MUTED
    };
    let text = fit_text(ui, name, &font, (max_width - 32.0).clamp(24.0, 160.0));
    let galley = ui.painter().layout_no_wrap(text, font, text_color);
    let size = Vec2::new(galley.size().x + 32.0, 24.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let fill = if selected {
        with_alpha(colour, 26)
    } else if response.hovered() {
        with_alpha(TEXT, 8)
    } else {
        CARD_DEEP
    };
    ui.painter().rect_filled(rect, CornerRadius::same(12), fill);
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(12),
        Stroke::new(
            1.0,
            if selected {
                with_alpha(colour, 120)
            } else {
                BORDER_SOFT
            },
        ),
        egui::StrokeKind::Inside,
    );
    let dot = egui::pos2(rect.left() + 13.0, rect.center().y);
    if muted {
        ui.painter()
            .circle_stroke(dot, 3.5, Stroke::new(1.5, colour));
    } else {
        ui.painter().circle_filled(dot, 4.0, colour);
    }
    ui.painter().galley(
        egui::pos2(rect.left() + 23.0, rect.center().y - galley.size().y / 2.0),
        galley,
        text_color,
    );
    show_focus_ring(ui, &response, rect, 12);
    row_info(
        &response,
        egui::Role::Button,
        ui.is_enabled(),
        name,
        Some(selected),
        muted.then(|| "muted".to_owned()),
    );
    response
}

/// One colour of the path palette as a round swatch; `chosen` rings it.
pub(crate) fn colour_swatch(
    ui: &mut egui::Ui,
    colour: Color32,
    name: &str,
    chosen: bool,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(22.0), Sense::click());
    ui.painter().circle_filled(rect.center(), 7.0, colour);
    if chosen {
        ui.painter()
            .circle_stroke(rect.center(), 10.0, Stroke::new(1.5, TEXT));
    } else if response.hovered() {
        ui.painter()
            .circle_stroke(rect.center(), 10.0, Stroke::new(1.0, MUTED));
    }
    show_focus_ring(ui, &response, rect, 11);
    row_info(
        &response,
        egui::Role::Button,
        ui.is_enabled(),
        name,
        Some(chosen),
        None,
    );
    response
}

#[cfg(test)]
mod tests {
    use super::super::node_named;
    use super::{colour_swatch, nav_path_item, path_chip};
    use crate::ui::theme::TEAL;
    use eframe::egui::{self, accesskit::Toggled};

    #[test]
    fn path_rows_expose_their_name_and_state() {
        let tree = super::super::accesskit_frame(|ui| {
            let entry = nav_path_item(ui, TEAL, "Wet", "2 stages · 1.3 ms", false);
            assert!(!entry.clicked());
            assert!(!path_chip(ui, TEAL, "Stream", true, true, 200.0).clicked());
            assert!(!colour_swatch(ui, TEAL, "Teal", true).clicked());
        });
        let entry = node_named(&tree, "Wet");
        assert_eq!(entry.role(), egui::Role::Button);
        assert_eq!(entry.value(), Some("2 stages · 1.3 ms"));
        assert_eq!(entry.toggled(), Some(Toggled::False));
        let chip = node_named(&tree, "Stream");
        assert_eq!(chip.value(), Some("muted"));
        assert_eq!(chip.toggled(), Some(Toggled::True));
        let swatch = node_named(&tree, "Teal");
        assert_eq!(swatch.role(), egui::Role::Button);
        assert_eq!(swatch.toggled(), Some(Toggled::True));
    }
}
