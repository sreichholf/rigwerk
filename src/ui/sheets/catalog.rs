//! The file list the amp and cab sheets share.
//!
//! Both sheets pick one file from a folder.  [`catalog_columns`] is that
//! picker: the list is the left half of whatever height is left in the
//! sheet, and it scrolls, and the settings are the right half at the same
//! width and the same height.  A long folder scrolls inside the list
//! instead of pushing the settings down.

use eframe::egui;
use egui::{Align2, CornerRadius, Layout, RichText, Sense, Stroke, Vec2};

use crate::ui::fonts::Face;
use crate::ui::theme::{font, palette, TypeRole};
use crate::ui::widgets::{fit_text, palette_focus, row_info, CONTROL_H};

/// Which row of [`catalog_columns`] was clicked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CatalogPick {
    /// The row before the files, such as *None*.
    Leading,
    /// `entries[index]`.
    Entry(usize),
}

/// One row of the file list.  `key` is what the list remembers it has
/// scrolled to, so each sheet and each file has its own place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CatalogRow<'a> {
    pub name: &'a str,
    /// Stable identity of the row: the file path, or a fixed token for a
    /// row that is not a file.
    pub key: &'a str,
    pub selected: bool,
}

/// The sentence for an empty folder.  `no_files` is the line used when the
/// folder is readable and holds nothing of the kind this sheet lists.
pub(crate) fn catalog_empty_note(folder: &str, no_files: &str) -> String {
    if folder.is_empty() {
        "Browse for a folder, or paste its path.".to_owned()
    } else if !std::path::Path::new(folder).is_dir() {
        "That folder can't be read.".to_owned()
    } else {
        no_files.to_owned()
    }
}

/// The list on the left and `settings` on the right, equal columns filling
/// the height left in `ui`.
///
/// `id` keeps the two sheets' scroll positions apart.  `leading` is an
/// extra first row (the cab sheet's *None*).  `empty` is shown under the
/// rows when there are no files.  The returned pick is the row clicked
/// this frame.
pub(crate) fn catalog_columns(
    ui: &mut egui::Ui,
    id: &str,
    leading: Option<CatalogRow<'_>>,
    entries: &[CatalogRow<'_>],
    empty: Option<&str>,
    settings: impl FnOnce(&mut egui::Ui),
) -> Option<CatalogPick> {
    let colours = palette(ui.ctx());
    let gap = 16.0;
    let height = column_height(ui);
    let width = ui.available_width();
    let column = ((width - gap) * 0.5).max(0.0);
    let mut pick = None;
    ui.allocate_ui_with_layout(
        Vec2::new(width, height),
        Layout::left_to_right(egui::Align::Min),
        |ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            ui.set_min_height(height);
            pick = column_scroll(ui, (id, "list"), column, height, |ui| {
                catalog_list(ui, id, leading, entries, empty)
            });
            let (gap_rect, _) = ui.allocate_exact_size(Vec2::new(gap, height), Sense::hover());
            ui.painter().vline(
                gap_rect.center().x,
                gap_rect.y_range(),
                Stroke::new(1.0, colours.line),
            );
            column_scroll(ui, (id, "settings"), column, height, settings);
        },
    );
    pick
}

fn column_height(ui: &egui::Ui) -> f32 {
    let available = ui.available_height();
    if available.is_finite() {
        available.max(0.0)
    } else {
        CONTROL_H * 8.0
    }
}

fn column_scroll<T>(
    ui: &mut egui::Ui,
    salt: impl std::hash::Hash + std::fmt::Debug,
    width: f32,
    height: f32,
    body: impl FnOnce(&mut egui::Ui) -> T,
) -> T {
    ui.allocate_ui_with_layout(
        Vec2::new(width, height),
        Layout::top_down(egui::Align::Min),
        |ui| {
            ui.set_width(width);
            ui.set_min_height(height);
            egui::ScrollArea::vertical()
                .id_salt(salt)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.set_width(width);
                    ui.set_max_width(width);
                    body(ui)
                })
                .inner
        },
    )
    .inner
}

fn catalog_list(
    ui: &mut egui::Ui,
    id: &str,
    leading: Option<CatalogRow<'_>>,
    entries: &[CatalogRow<'_>],
    empty: Option<&str>,
) -> Option<CatalogPick> {
    ui.spacing_mut().item_spacing.y = 2.0;
    let mut pick = None;
    if let Some(row) = leading {
        let response = catalog_row(ui, row.selected, row.name);
        if response.clicked() {
            pick = Some(CatalogPick::Leading);
        }
        if row.selected {
            reveal(ui, id, &response, row.key);
        }
    }
    if entries.is_empty() {
        if let Some(note) = empty {
            let colours = palette(ui.ctx());
            ui.label(
                RichText::new(note)
                    .font(font(Face::Sans, TypeRole::Body))
                    .color(colours.text2),
            );
        }
    } else {
        for (index, row) in entries.iter().enumerate() {
            let response = catalog_row(ui, row.selected, row.name);
            if response.clicked() {
                pick = Some(CatalogPick::Entry(index));
            }
            if row.selected {
                reveal(ui, id, &response, row.key);
            }
        }
    }
    pick
}

/// Bring `response` into view when this sheet has not already revealed `key`.
///
/// The scroll area keeps its parent's scope, so the sheet `id` has to be
/// part of the memory id.  `key` is the file path, not the row index:
/// two sheets can both be on row 20, and a new folder can reuse an index.
fn reveal(ui: &egui::Ui, id: &str, response: &egui::Response, key: &str) {
    let memory = ui.scope_id().with((id, "catalog_revealed"));
    let seen = ui.data(|data| data.get_temp::<String>(memory));
    if seen.as_deref() == Some(key) {
        return;
    }
    ui.scroll_to_rect(response.rect, None);
    let key = key.to_owned();
    ui.data_mut(|data| data.insert_temp(memory, key));
}

fn catalog_row(ui: &mut egui::Ui, selected: bool, name: &str) -> egui::Response {
    let colours = palette(ui.ctx());
    let width = ui.available_width();
    let (rect, mut response) = ui.allocate_exact_size(Vec2::new(width, CONTROL_H), Sense::click());
    if selected || response.hovered() {
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(3),
            if selected {
                colours.accent_bg
            } else {
                colours.slot
            },
        );
    }
    if selected {
        ui.painter().rect_stroke(
            rect,
            CornerRadius::same(3),
            Stroke::new(1.5, colours.accent),
            egui::StrokeKind::Inside,
        );
    }
    let text_font = font(Face::Sans, TypeRole::Body);
    let shown = fit_text(ui, name, &text_font, (rect.width() - 16.0).max(8.0));
    ui.painter().text(
        egui::pos2(rect.left() + 8.0, rect.center().y),
        Align2::LEFT_CENTER,
        &shown,
        text_font,
        colours.text,
    );
    if shown != name {
        response = response.on_hover_text(name);
    }
    row_info(
        &response,
        egui::Role::Button,
        ui.is_enabled(),
        name,
        Some(selected),
        None,
    );
    palette_focus(ui, &response, rect);
    response
}

#[cfg(test)]
mod tests;
