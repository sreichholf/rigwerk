//! The session menu: the saved list, save, rename and delete.

use eframe::egui;
use egui::{Align2, CornerRadius, FontFamily, FontId, Rect, RichText, Sense, Stroke, Vec2};

use crate::app::RigWerkApp;
use crate::ui::theme::ts;
use crate::ui::theme::{with_alpha, BLUE, BORDER_SOFT, CARD_DEEP, DIM, MUTED, RED, TEXT};
use crate::ui::widgets::{ellipsize, row_info, show_focus_ring};

pub(crate) fn session_row(
    ui: &mut egui::Ui,
    name: &str,
    note: &str,
    path: &str,
    facts: &str,
    selected: bool,
    confirming_delete: bool,
) -> SessionAction {
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 64.0), Sense::click());
    // The delete control sits on top of the row, so it is registered after the
    // row and wins the click.
    let delete_label = if confirming_delete {
        "CONFIRM DELETE"
    } else {
        "DELETE"
    };
    let delete_w = if confirming_delete { 92.0 } else { 46.0 };
    let delete_rect = Rect::from_min_size(
        rect.right_top() + Vec2::new(-delete_w - 6.0, 5.0),
        Vec2::new(delete_w, 17.0),
    );
    let delete = ui.interact(delete_rect, response.id.with("delete"), Sense::click());
    let fill = if selected {
        with_alpha(BLUE, 12)
    } else if response.hovered() {
        with_alpha(TEXT, 6)
    } else {
        CARD_DEEP
    };
    ui.painter().rect_filled(rect, CornerRadius::same(8), fill);
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(8),
        Stroke::new(
            1.0,
            if selected {
                with_alpha(BLUE, 55)
            } else {
                BORDER_SOFT
            },
        ),
        egui::StrokeKind::Inside,
    );
    ui.painter().text(
        rect.left_top() + Vec2::new(10.0, 9.0),
        Align2::LEFT_TOP,
        name,
        FontId::new(ts(11.0), FontFamily::Proportional),
        TEXT,
    );
    ui.painter().text(
        rect.left_top() + Vec2::new(10.0, 25.0),
        Align2::LEFT_TOP,
        ellipsize(note, 48),
        FontId::new(ts(8.2), FontFamily::Proportional),
        DIM,
    );
    ui.painter().text(
        rect.left_bottom() + Vec2::new(10.0, -9.0),
        Align2::LEFT_BOTTOM,
        ellipsize(path, 52),
        FontId::new(ts(8.2), FontFamily::Monospace),
        MUTED,
    );
    ui.painter().text(
        rect.right_bottom() + Vec2::new(-10.0, -9.0),
        Align2::RIGHT_BOTTOM,
        facts,
        FontId::new(ts(7.8), FontFamily::Monospace),
        DIM,
    );
    let delete_color = if confirming_delete || delete.hovered() {
        RED
    } else {
        DIM
    };
    if confirming_delete || delete.hovered() {
        ui.painter()
            .rect_filled(delete_rect, CornerRadius::same(5), with_alpha(RED, 22));
    }
    ui.painter().rect_stroke(
        delete_rect,
        CornerRadius::same(5),
        Stroke::new(1.0, with_alpha(delete_color, 70)),
        egui::StrokeKind::Inside,
    );
    ui.painter().text(
        delete_rect.center(),
        Align2::CENTER_CENTER,
        delete_label,
        FontId::new(ts(7.3), FontFamily::Monospace),
        delete_color,
    );
    if selected {
        ui.painter().text(
            delete_rect.left_center() + Vec2::new(-8.0, 0.0),
            Align2::RIGHT_CENTER,
            "CURRENT",
            FontId::new(ts(7.3), FontFamily::Monospace),
            BLUE,
        );
    }
    let delete = delete.on_hover_text(if confirming_delete {
        "Click again to delete this session for good"
    } else {
        "Delete this session"
    });
    show_focus_ring(ui, &response, rect, 8);
    show_focus_ring(ui, &delete, delete_rect, 5);
    row_info(
        &response,
        egui::Role::Button,
        ui.is_enabled(),
        name,
        Some(selected),
        Some(if note.is_empty() {
            path.to_owned()
        } else {
            note.to_owned()
        }),
    );
    row_info(
        &delete,
        egui::Role::Button,
        ui.is_enabled(),
        if confirming_delete {
            format!("Confirm delete {name}")
        } else {
            format!("Delete {name}")
        },
        None,
        None,
    );
    if delete.clicked() {
        SessionAction::Delete
    } else if response.clicked() {
        SessionAction::Load
    } else {
        SessionAction::None
    }
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
                ui.set_min_width(320.0);
                action = self.session_menu(ui);
            });
        if let Some((index, choice)) = action {
            self.apply_session_action(index, choice);
        }
    }

    fn session_menu(&mut self, ui: &mut egui::Ui) -> Option<(usize, SessionAction)> {
        let mut chosen = None;
        ui.horizontal(|ui| {
            if ui.button("Save").clicked() {
                self.save_session();
            }
            if ui.button("Save as new session").clicked() {
                self.chrome.session_draft = Some(crate::app::screen::SessionDraft::SaveAs(
                    self.session_name.clone(),
                ));
            }
            if ui.button("Rename").clicked() {
                self.chrome.session_draft = Some(crate::app::screen::SessionDraft::Rename(
                    self.session_name.clone(),
                ));
            }
            if ui.button("Start new session").clicked() {
                self.start_new_session();
            }
        });
        if let Some(draft) = self.chrome.session_draft.clone() {
            let mut text = match &draft {
                crate::app::screen::SessionDraft::SaveAs(text)
                | crate::app::screen::SessionDraft::Rename(text) => text.clone(),
            };
            let response = ui.add(
                egui::TextEdit::singleline(&mut text)
                    .hint_text("Session name")
                    .desired_width(280.0),
            );
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
            ui.label("No saved sessions yet");
            return None;
        }
        let rows: Vec<(String, String, String, String, u32, usize)> = self
            .sessions
            .iter()
            .map(|session| {
                (
                    session.name.clone(),
                    session.note.clone(),
                    session.input.clone(),
                    session.output.clone(),
                    session.rate,
                    session.buffer,
                )
            })
            .collect();
        egui::ScrollArea::vertical()
            .max_height(280.0)
            .show(ui, |ui| {
                for (index, (name, note, input, output, rate, buffer)) in rows.iter().enumerate() {
                    let confirm = self.chrome.load_confirm == Some(index);
                    let row_note = if confirm {
                        "Load and lose changes"
                    } else {
                        note.as_str()
                    };
                    match session_row(
                        ui,
                        name,
                        row_note,
                        &format!("{input}  ->  {output}"),
                        &format!("{rate} Hz · {buffer} smp"),
                        Some(index) == self.selected_session,
                        Some(index) == self.pending_delete,
                    ) {
                        SessionAction::None => {}
                        choice => chosen = Some((index, choice)),
                    }
                    ui.add_space(3.0);
                }
            });
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

#[cfg(test)]
mod tests;
