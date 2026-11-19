//! The session menu: the saved list, save, rename and delete.
//!
//! A row shows the name, the devices, the rate, the buffer and an estimated
//! latency.  The live figure is only the open session while it is not edited.
//! From 8 saved sessions the list filters by name, and past 12 rows it scrolls.

use eframe::egui;
use egui::{Align2, CornerRadius, FontId, Rect, RichText, Sense, Stroke, Vec2};

use crate::app::screen::format_ms;
use crate::app::settings::rate_label;
use crate::app::RigWerkApp;
use crate::ui::theme::{font, palette, with_alpha, Face, TypeRole};
use crate::ui::widgets::{fit_text, palette_focus, row_info, OutlineButton, TextField};

/// Allocated height of one row, including the gap under it.
pub(crate) const SESSION_ROW_H: f32 = 72.0;
const ROW_GAP: f32 = 4.0;
/// The list gains a filter at this many saved sessions.
pub(crate) const SESSION_FILTER_FROM: usize = 8;
/// The list scrolls once it has more rows than this.
pub(crate) const SESSION_SCROLL_AFTER: usize = 12;

const LOAD_WARNING: &str = "Load and lose changes";
const LIVE_HOVER: &str = "This is the clock output estimate.";
const BUFFERS_HOVER: &str = "Buffers only. Open the session to include the chain.";
const DELETE_LABEL: &str = "Delete";
const DELETE_CONFIRM: &str = "Delete session";

/// `true` when the saved list is long enough for a name filter.
pub(crate) fn session_list_shows_filter(saved: usize) -> bool {
    saved >= SESSION_FILTER_FROM
}

/// Viewport height for more than [`SESSION_SCROLL_AFTER`] rows.  Fewer rows
/// are not placed in a scroll area.
pub(crate) fn session_list_scroll_height(rows: usize) -> Option<f32> {
    (rows > SESSION_SCROLL_AFTER).then_some(SESSION_ROW_H * SESSION_SCROLL_AFTER as f32)
}

/// Case-insensitive match on the session name.  A blank filter matches all.
pub(crate) fn session_matches_filter(name: &str, filter: &str) -> bool {
    let filter = filter.trim();
    filter.is_empty() || name.to_lowercase().contains(&filter.to_lowercase())
}

/// The input and output names.  Empty names are left out.
pub(crate) fn session_devices(input: &str, output: &str) -> String {
    match (input.is_empty(), output.is_empty()) {
        (false, false) => format!("{input} → {output}"),
        (false, true) => input.to_owned(),
        (true, false) => output.to_owned(),
        (true, true) => String::new(),
    }
}

/// `48 kHz`, or `48 → 44.1 kHz` when the output rate is set and differs.
pub(crate) fn session_rate_text(rate: u32, output_rate: u32) -> String {
    if output_rate != 0 && output_rate != rate {
        let full = rate_label(rate);
        let head = full.trim_end_matches(" kHz");
        format!("{head} → {}", rate_label(output_rate))
    } else {
        rate_label(rate)
    }
}

/// `64 smp`, or `64 / 128 smp` when the output buffer is set and differs.
pub(crate) fn session_buffer_text(buffer: usize, output_buffer: usize) -> String {
    if output_buffer == 0 || output_buffer == buffer {
        format!("{buffer} smp")
    } else {
        format!("{buffer} / {output_buffer} smp")
    }
}

/// `≈ 7.6 ms` below 10 ms, `≈ 13 ms` from 10 ms.
pub(crate) fn session_latency_text(ms: f32) -> String {
    format!("≈ {}", format_ms(ms))
}

/// Rate, buffer and latency, in the status-bar shape.
pub(crate) fn session_facts(
    rate: u32,
    output_rate: u32,
    buffer: usize,
    output_buffer: usize,
    latency_ms: f32,
) -> String {
    format!(
        "{} · {} · {}",
        session_rate_text(rate, output_rate),
        session_buffer_text(buffer, output_buffer),
        session_latency_text(latency_ms)
    )
}

/// Input buffer time plus output buffer time.  `bridge_buffer_ms` is added
/// only when both rates are non-zero and differ.  A zero output rate or
/// buffer follows the input.
pub(crate) fn buffers_only_ms(
    rate: u32,
    buffer: usize,
    output_rate: u32,
    output_buffer: usize,
    bridge_buffer_ms: f32,
) -> f32 {
    let input_rate = rate.max(1);
    let playback_rate = if output_rate == 0 {
        input_rate
    } else {
        output_rate.max(1)
    };
    let playback_buffer = if output_buffer == 0 {
        buffer
    } else {
        output_buffer
    };
    let input_ms = buffer as f32 / input_rate as f32 * 1000.0;
    let output_ms = playback_buffer as f32 / playback_rate as f32 * 1000.0;
    let bridge = if rate != 0 && output_rate != 0 && rate != output_rate {
        bridge_buffer_ms
    } else {
        0.0
    };
    input_ms + output_ms + bridge
}

/// Device line, with the load warning on the next line while it is showing.
pub(crate) fn session_detail(devices: &str, load_warning: bool) -> String {
    if load_warning {
        format!("{devices}\n{LOAD_WARNING}")
    } else {
        devices.to_owned()
    }
}

/// One saved session.
///
/// `detail` is the device line.  A second line, after a newline, is
/// "Load and lose changes" while that load still needs a confirm.
/// `live_estimate` selects the clock-output hover; otherwise the hover says
/// the figure is buffers only.
pub(crate) fn session_row(
    ui: &mut egui::Ui,
    name: &str,
    detail: &str,
    facts: &str,
    selected: bool,
    confirming_delete: bool,
    live_estimate: bool,
) -> SessionAction {
    let (devices, warning) = match detail.split_once('\n') {
        Some((devices, warning)) if warning == LOAD_WARNING => (devices, LOAD_WARNING),
        _ => (detail, ""),
    };
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, SESSION_ROW_H), Sense::click());
    let card = Rect::from_min_size(rect.min, Vec2::new(rect.width(), rect.height() - ROW_GAP));
    // Registered after the row, so a click here is delete and not load.
    let delete_label = if confirming_delete {
        DELETE_CONFIRM
    } else {
        DELETE_LABEL
    };
    let delete_font = font(Face::Sans, TypeRole::Secondary);
    let delete_w = measure(ui, delete_label, &delete_font) + 16.0;
    let delete_rect = Rect::from_min_size(
        egui::pos2(card.right() - 8.0 - delete_w, card.top() + 8.0),
        Vec2::new(delete_w, 24.0),
    );
    let delete = ui.interact(delete_rect, response.id.with("delete"), Sense::click());

    let colours = palette(ui.ctx());
    let fill = if selected {
        colours.accent_bg
    } else {
        colours.slot
    };
    let stroke = if selected {
        Stroke::new(1.0, colours.accent)
    } else if response.hovered() {
        Stroke::new(1.0, colours.line2)
    } else {
        Stroke::new(1.0, colours.line)
    };
    let painter = ui.painter();
    painter.rect_filled(card, CornerRadius::same(4), fill);
    painter.rect_stroke(
        card,
        CornerRadius::same(4),
        stroke,
        egui::StrokeKind::Inside,
    );

    let name_font = font(Face::Sans, TypeRole::Body);
    let secondary = font(Face::Sans, TypeRole::Secondary);
    let facts_font = font(Face::Mono, TypeRole::Secondary);
    let name_y = card.top() + 20.0;
    let devices_y = card.top() + 44.0;
    let facts_y = card.top() + 58.0;
    let left = card.left() + 8.0;
    let right = card.right() - 8.0;
    let name_shown = fit_text(
        ui,
        name,
        &name_font,
        (delete_rect.left() - 8.0 - left).max(8.0),
    );
    painter.text(
        egui::pos2(left, name_y),
        Align2::LEFT_CENTER,
        name_shown,
        name_font,
        colours.text,
    );
    let warn_w = if warning.is_empty() {
        0.0
    } else {
        measure(ui, warning, &secondary) + 8.0
    };
    if !devices.is_empty() {
        let shown = fit_text(ui, devices, &secondary, (right - warn_w - left).max(8.0));
        painter.text(
            egui::pos2(left, devices_y),
            Align2::LEFT_CENTER,
            shown,
            secondary.clone(),
            colours.text2,
        );
    }
    if !warning.is_empty() {
        let (pos, align) = if devices.is_empty() {
            (egui::pos2(left, devices_y), Align2::LEFT_CENTER)
        } else {
            (egui::pos2(right, devices_y), Align2::RIGHT_CENTER)
        };
        painter.text(pos, align, warning, secondary, colours.error_text);
    }
    let facts_shown = fit_text(ui, facts, &facts_font, (right - left).max(8.0));
    painter.text(
        egui::pos2(left, facts_y),
        Align2::LEFT_CENTER,
        facts_shown,
        facts_font,
        colours.text2,
    );

    let (delete_fill, delete_stroke, delete_ink) = if confirming_delete {
        (
            with_alpha(colours.error, 48),
            Stroke::new(1.0, colours.error),
            colours.error_text,
        )
    } else {
        (
            egui::Color32::TRANSPARENT,
            Stroke::new(1.0, colours.line2),
            colours.text,
        )
    };
    painter.rect_filled(delete_rect, CornerRadius::same(4), delete_fill);
    painter.rect_stroke(
        delete_rect,
        CornerRadius::same(4),
        delete_stroke,
        egui::StrokeKind::Inside,
    );
    painter.text(
        delete_rect.center(),
        Align2::CENTER_CENTER,
        delete_label,
        delete_font,
        delete_ink,
    );

    row_info(
        &response,
        egui::Role::Button,
        ui.is_enabled(),
        name,
        Some(selected),
        Some(row_value(devices, warning, facts)),
    );
    row_info(
        &delete,
        egui::Role::Button,
        ui.is_enabled(),
        delete_label,
        None,
        None,
    );
    let response = if delete.hovered() {
        response
    } else {
        response.on_hover_text(if live_estimate {
            LIVE_HOVER
        } else {
            BUFFERS_HOVER
        })
    };
    let delete = delete.on_hover_text(if confirming_delete {
        "Delete this session for good."
    } else {
        "Delete this session."
    });
    palette_focus(ui, &response, card);
    palette_focus(ui, &delete, delete_rect);
    if delete.clicked() {
        SessionAction::Delete
    } else if response.clicked() {
        SessionAction::Load
    } else {
        SessionAction::None
    }
}

fn row_value(devices: &str, warning: &str, facts: &str) -> String {
    let mut lines = Vec::new();
    if !devices.is_empty() {
        lines.push(devices);
    }
    if !warning.is_empty() {
        lines.push(warning);
    }
    if !facts.is_empty() {
        lines.push(facts);
    }
    lines.join("\n")
}

fn measure(ui: &egui::Ui, text: &str, font_id: &FontId) -> f32 {
    ui.painter()
        .layout_no_wrap(text.to_owned(), font_id.clone(), palette(ui.ctx()).text)
        .size()
        .x
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum SessionAction {
    None,
    Load,
    Delete,
}

impl RigWerkApp {
    /// The session name.  The menu lists saved sessions and the edits.
    pub(crate) fn session_button(&mut self, ui: &mut egui::Ui) {
        let colours = crate::ui::theme::palette(ui.ctx());
        let name = if self.session_name.trim().is_empty() {
            "Untitled session".to_owned()
        } else {
            self.session_name.clone()
        };
        let edited = self.session_edited();
        let label = if edited {
            format!("{name} · edited")
        } else {
            name
        };
        let button = ui.add(
            egui::Button::new(
                RichText::new(label)
                    .font(crate::ui::theme::font(
                        crate::ui::theme::Face::Sans,
                        crate::ui::theme::TypeRole::Control,
                    ))
                    .color(colours.text),
            )
            .fill(egui::Color32::TRANSPARENT)
            .stroke(Stroke::new(1.0, colours.line2))
            .corner_radius(CornerRadius::same(4))
            .min_size(Vec2::new(32.0, 32.0)),
        );
        if self.chrome.open_session_menu {
            egui::Popup::open_id(ui.ctx(), egui::Popup::default_response_id(&button));
        }
        let mut action = None;
        egui::Popup::menu(&button)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show(|ui| {
                ui.set_min_width(360.0);
                action = self.session_menu(ui);
            });
        if let Some((index, choice)) = action {
            self.apply_session_action(index, choice);
        }
    }

    fn session_menu(&mut self, ui: &mut egui::Ui) -> Option<(usize, SessionAction)> {
        menu_corners(ui);
        ui.horizontal_wrapped(|ui| {
            if OutlineButton::new("Save")
                .show(ui)
                .on_hover_text("Update the saved session of this name, or add it.")
                .clicked()
            {
                self.save_session();
            }
            if OutlineButton::new("Save as new session")
                .show(ui)
                .on_hover_text("Add the setup as another session.")
                .clicked()
            {
                self.chrome.session_draft = Some(crate::app::screen::SessionDraft::SaveAs(
                    self.session_name.clone(),
                ));
            }
            if OutlineButton::new("Rename")
                .show(ui)
                .on_hover_text("Rename the saved session and the current name.")
                .clicked()
            {
                self.chrome.session_draft = Some(crate::app::screen::SessionDraft::Rename(
                    self.session_name.clone(),
                ));
            }
            if OutlineButton::new("Start new session")
                .show(ui)
                .on_hover_text("Name this setup Untitled session. Nothing else changes.")
                .clicked()
            {
                self.start_new_session();
            }
        });
        if let Some(draft) = self.chrome.session_draft.clone() {
            let mut text = match &draft {
                crate::app::screen::SessionDraft::SaveAs(text)
                | crate::app::screen::SessionDraft::Rename(text) => text.clone(),
            };
            let response = TextField::new(&mut text, "Session name")
                .hint("Session name")
                .width(280.0)
                .show(ui);
            if self.chrome.open_session_menu {
                response.request_focus();
                self.chrome.open_session_menu = false;
            }
            let enter =
                response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
            self.chrome.session_draft = Some(match draft {
                crate::app::screen::SessionDraft::SaveAs(_) => {
                    crate::app::screen::SessionDraft::SaveAs(text.clone())
                }
                crate::app::screen::SessionDraft::Rename(_) => {
                    crate::app::screen::SessionDraft::Rename(text.clone())
                }
            });
            if enter {
                match self.chrome.session_draft.clone() {
                    Some(crate::app::screen::SessionDraft::SaveAs(name)) => {
                        self.session_name = name;
                        self.save_session();
                    }
                    Some(crate::app::screen::SessionDraft::Rename(name)) => {
                        self.rename_session(&name)
                    }
                    None => {}
                }
                self.chrome.session_draft = None;
            }
        }
        if self.sessions.is_empty() {
            let colours = palette(ui.ctx());
            ui.label(
                RichText::new("No saved sessions yet")
                    .font(font(Face::Sans, TypeRole::Body))
                    .color(colours.text2),
            );
            return None;
        }
        let filter = if session_list_shows_filter(self.sessions.len()) {
            session_filter(ui)
        } else {
            String::new()
        };
        let live_index = if self.session_edited() {
            None
        } else {
            self.session_named(&self.session_name)
        };
        let live_ms = self.heard_estimate().total_ms;
        let shown: Vec<(usize, String, String, String, bool)> = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, session)| session_matches_filter(&session.name, &filter))
            .map(|(index, session)| {
                let live = Some(index) == live_index;
                let ms = if live {
                    live_ms
                } else {
                    buffers_only_ms(
                        session.rate,
                        session.buffer,
                        session.output_rate,
                        session.output_buffer,
                        session.bridge_buffer_ms,
                    )
                };
                (
                    index,
                    session.name.clone(),
                    session_devices(&session.input, &session.output),
                    session_facts(
                        session.rate,
                        session.output_rate,
                        session.buffer,
                        session.output_buffer,
                        ms,
                    ),
                    live,
                )
            })
            .collect();
        let mut chosen = None;
        let mut paint_rows = |ui: &mut egui::Ui| {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                for (index, name, devices, facts, live) in &shown {
                    let detail = session_detail(devices, self.chrome.load_confirm == Some(*index));
                    let action = session_row(
                        ui,
                        name,
                        &detail,
                        facts,
                        Some(*index) == self.selected_session,
                        Some(*index) == self.pending_delete,
                        *live,
                    );
                    if action != SessionAction::None {
                        chosen = Some((*index, action));
                    }
                }
            });
        };
        // More than 12 rows scroll.  Fewer stay at their own height.
        if let Some(height) = session_list_scroll_height(shown.len()) {
            egui::ScrollArea::vertical()
                .id_salt("saved-sessions")
                .max_height(height)
                .auto_shrink([false, true])
                .show(ui, |ui| paint_rows(ui));
        } else {
            paint_rows(ui);
        }
        chosen
    }

    fn apply_session_action(&mut self, index: usize, action: SessionAction) {
        match action {
            SessionAction::None => {}
            SessionAction::Load => {
                if self.session_edited() && self.chrome.load_confirm != Some(index) {
                    self.chrome.load_confirm = Some(index);
                } else {
                    self.apply_session(index);
                    self.chrome.load_confirm = None;
                }
            }
            SessionAction::Delete => {
                if self.pending_delete == Some(index) {
                    self.delete_session(index);
                } else {
                    self.pending_delete = Some(index);
                }
            }
        }
    }
}

fn menu_corners(ui: &mut egui::Ui) {
    let widgets = &mut ui.visuals_mut().widgets;
    for widget in [
        &mut widgets.noninteractive,
        &mut widgets.inactive,
        &mut widgets.hovered,
        &mut widgets.active,
        &mut widgets.open,
    ] {
        widget.corner_radius = CornerRadius::same(4);
    }
}

fn session_filter(ui: &mut egui::Ui) -> String {
    let id = egui::Id::unique("session-menu-filter");
    let mut text = ui
        .ctx()
        .data(|data| data.get_temp::<String>(id))
        .unwrap_or_default();
    TextField::new(&mut text, "Filter")
        .hint("Filter")
        .report_value()
        .show(ui);
    ui.ctx().data_mut(|data| data.insert_temp(id, text.clone()));
    text
}

#[cfg(test)]
mod tests;
