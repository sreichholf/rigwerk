//! The Mixer's path parts: the path chips in the toolbar, the path menu
//! (rename, colour, duplicate, remove), and the Path stage inspector
//! (path level, mute and solo).
//!
//! The Mixer edits one path at a time, the selected one.  With a single
//! path there are no chips and no Path stage, so the page reads as before;
//! only the "+ Path" button is new.

use eframe::egui;
use egui::{Align, Color32, CornerRadius, FontFamily, FontId, Layout, RichText, Stroke, Vec2};

use crate::app::paths::{PathInfo, PATH_COLOURS};
use crate::app::RigWerkApp;
use crate::audio::{MAX_PATHS, PATH_LEVEL_MAX_DB, PATH_LEVEL_MIN_DB};
use crate::ui::theme::ts;
use crate::ui::theme::{
    with_alpha, AMBER, BORDER_SOFT, CARD_RAISED, DIM, GREEN, MUTED, RED, TEAL, TEXT,
};
use crate::ui::widgets::{
    colour_swatch, db_slider, fit_text, meter_row, micro_label, mono, path_chip, path_level_text,
    path_toggle,
};

/// Names of [`PATH_COLOURS`], for screen readers and hover text.
const PATH_COLOUR_NAMES: [&str; MAX_PATHS] = ["Teal", "Violet", "Amber", "Pink"];

/// Width of the "+ Path" button, kept free when the chips are sized.
const ADD_PATH_W: f32 = 64.0;

/// What the path controls asked for; applied after the frame is drawn.
#[derive(Debug, PartialEq)]
pub(super) enum PathAction {
    Select(usize),
    AddEmpty,
    Duplicate(usize),
    Remove(usize),
    Rename(usize, String),
    Colour(usize, u8),
    Level(usize, f32),
    Mute(usize, bool),
    Solo(usize, bool),
}

impl RigWerkApp {
    pub(super) fn apply_path_action(&mut self, action: PathAction) {
        match action {
            PathAction::Select(slot) => {
                self.select_path(slot);
            }
            PathAction::AddEmpty => {
                self.add_path();
            }
            PathAction::Duplicate(slot) => {
                self.duplicate_path(slot);
            }
            PathAction::Remove(slot) => {
                self.remove_path(slot);
            }
            PathAction::Rename(slot, name) => {
                if !self.rename_path(slot, &name) {
                    self.push_log(
                        format!("Another path is already called {}.", name.trim()),
                        AMBER,
                    );
                }
            }
            PathAction::Colour(slot, colour) => self.set_path_colour(slot, colour),
            PathAction::Level(slot, level) => self.set_path_level(slot, level),
            PathAction::Mute(slot, mute) => self.set_path_mute(slot, mute),
            PathAction::Solo(slot, solo) => self.set_path_solo(slot, solo),
        }
    }

    /// Why "+ Path" is off, or `None` while another path fits.
    pub(crate) fn add_path_blocker(&self) -> Option<&'static str> {
        if self.can_add_path() {
            None
        } else if self.path_count() >= MAX_PATHS {
            Some("Four paths is the most RigWerk runs.")
        } else {
            Some("A removed path is still fading out. Try again in a moment.")
        }
    }
}

/// The toolbar's path part, at most `max_width` wide: one chip per path
/// (only with several), then "+ Path" (empty or a muted duplicate of the
/// selected path).  A chip's right-click menu renames, recolours,
/// duplicates or removes its path.  `add_blocked` says why no path can be
/// added right now.
pub(crate) fn path_toolbar(
    ui: &mut egui::Ui,
    infos: &[PathInfo],
    add_blocked: Option<&str>,
    max_width: f32,
    actions: &mut Vec<PathAction>,
) {
    let several = infos.len() > 1;
    if several {
        let gap = ui.spacing().item_spacing.x;
        let chip_w = ((max_width - ADD_PATH_W) / infos.len() as f32 - gap).max(56.0);
        for info in infos {
            let response = path_chip(
                ui,
                info.colour,
                &info.name,
                info.mute,
                info.selected,
                chip_w,
            )
            .on_hover_text(format!(
                "{} · {}. Click to edit, right-click for more.",
                info.name,
                info.summary()
            ));
            if response.clicked() {
                actions.push(PathAction::Select(info.slot));
            }
            // Clicks inside (the name field, the swatches) keep it open.
            let menu = egui::Popup::context_menu(&response)
                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                .show(|ui| path_menu(ui, info, add_blocked.is_none(), actions));
            if menu.is_none() {
                // Closed (or never open): a name typed and left without
                // Enter is still taken.
                commit_draft(ui, "menu", info, actions);
            }
        }
    }
    let selected = infos.iter().find(|info| info.selected);
    let mut add = ui
        .add_enabled(
            add_blocked.is_none(),
            egui::Button::new(RichText::new("+ Path").size(ts(9.5)).color(MUTED))
                .fill(Color32::TRANSPARENT)
                .stroke(Stroke::new(1.0, BORDER_SOFT))
                .corner_radius(CornerRadius::same(12))
                .min_size(Vec2::new(0.0, 24.0)),
        )
        .on_hover_text("Add a signal path: empty, or a muted copy of this one");
    if let Some(reason) = add_blocked {
        add = add.on_disabled_hover_text(reason);
    }
    egui::Popup::menu(&add).show(|ui| {
        ui.set_min_width(170.0);
        if ui.button("Empty path").clicked() {
            actions.push(PathAction::AddEmpty);
        }
        if let Some(info) = selected {
            if ui
                .button(format!("Duplicate {}", info.name))
                .on_hover_text("A full copy of the chain and its routing, added muted")
                .clicked()
            {
                actions.push(PathAction::Duplicate(info.slot));
            }
        }
    });
}

/// Rename, colour, duplicate and remove, for a chip's right-click menu.
/// The menu only exists with several paths, so removing is always allowed.
fn path_menu(ui: &mut egui::Ui, info: &PathInfo, can_add: bool, actions: &mut Vec<PathAction>) {
    ui.set_min_width(190.0);
    ui.label(micro_label("NAME", DIM));
    name_field(ui, "menu", info, 180.0, actions);
    ui.add_space(4.0);
    ui.label(micro_label("COLOUR", DIM));
    colour_swatches(ui, info, actions);
    ui.separator();
    if ui
        .add_enabled(can_add, egui::Button::new("Duplicate (muted)"))
        .clicked()
    {
        actions.push(PathAction::Duplicate(info.slot));
        ui.close();
    }
    if ui.button(RichText::new("Remove path").color(RED)).clicked() {
        actions.push(PathAction::Remove(info.slot));
        ui.close();
    }
}

/// Where a half-typed name lives while its field has focus.  `place`
/// keeps the menu's and the inspector's fields apart.
fn draft_id(place: &str, slot: usize) -> egui::Id {
    egui::Id::unique(("rigwerk_path_name", place, slot))
}

/// Renames to the draft in `place`, if there is one, and drops it.
fn commit_draft(ui: &egui::Ui, place: &str, info: &PathInfo, actions: &mut Vec<PathAction>) {
    let id = draft_id(place, info.slot);
    if let Some(text) = ui.data_mut(|data| data.remove_temp::<String>(id)) {
        push_rename(info, &text, actions);
    }
}

fn push_rename(info: &PathInfo, text: &str, actions: &mut Vec<PathAction>) {
    if !text.trim().is_empty() && text.trim() != info.name {
        actions.push(PathAction::Rename(info.slot, text.to_owned()));
    }
}

/// A name field for path `info`.  The path is renamed when the field is
/// left (Enter, Tab or a click elsewhere), not on every key, so a name
/// being typed (or composed with an input method) is never trimmed or
/// refused under the cursor.  Escape drops the edit.
fn name_field(
    ui: &mut egui::Ui,
    place: &str,
    info: &PathInfo,
    width: f32,
    actions: &mut Vec<PathAction>,
) {
    let id = draft_id(place, info.slot);
    let mut text = ui
        .data_mut(|data| data.get_temp::<String>(id))
        .unwrap_or_else(|| info.name.clone());
    let response = ui.add_sized(
        [width, 24.0],
        egui::TextEdit::singleline(&mut text)
            .char_limit(32)
            .text_color(TEXT),
    );
    if response.lost_focus() {
        ui.data_mut(|data| data.remove::<String>(id));
        if !ui.input(|input| input.key_pressed(egui::Key::Escape)) {
            push_rename(info, &text, actions);
        }
    } else if response.has_focus() {
        ui.data_mut(|data| data.insert_temp(id, text));
    }
}

fn colour_swatches(ui: &mut egui::Ui, info: &PathInfo, actions: &mut Vec<PathAction>) {
    ui.horizontal(|ui| {
        for (index, colour) in PATH_COLOURS.iter().enumerate() {
            let chosen = index as u8 == info.colour_index;
            if colour_swatch(ui, *colour, PATH_COLOUR_NAMES[index], chosen)
                .on_hover_text(PATH_COLOUR_NAMES[index])
                .clicked()
                && !chosen
            {
                actions.push(PathAction::Colour(info.slot, index as u8));
            }
        }
    });
}

/// The path meters as drawn: smoothed level and held peak, L then R.
pub(super) struct PathMeters {
    pub(super) level: [f32; 2],
    pub(super) peak: [f32; 2],
}

/// The Path stage: the selected path's level, mute and solo, its meters,
/// and its name, colour, duplicate and remove.
pub(super) fn path_inspector(
    ui: &mut egui::Ui,
    info: &PathInfo,
    meters: &PathMeters,
    can_add: bool,
    can_remove: bool,
    actions: &mut Vec<PathAction>,
) {
    ui.horizontal(|ui| {
        let (dot, _) = ui.allocate_exact_size(Vec2::splat(14.0), egui::Sense::hover());
        ui.painter().circle_filled(dot.center(), 5.0, info.colour);
        // Leave room for MUTE and SOLO; a long name is cut, not overlapped.
        let font = FontId::new(ts(16.0), FontFamily::Proportional);
        let title = fit_text(
            ui,
            &format!("Path · {}", info.name),
            &font,
            (ui.available_width() - 150.0).max(80.0),
        );
        ui.label(RichText::new(title).font(font).color(TEXT).strong())
            .on_hover_text(&info.name);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let label = if info.solo { "SOLOED" } else { "SOLO" };
            if path_toggle(
                ui,
                label,
                info.solo,
                AMBER,
                label,
                "While any path is soloed, only soloed paths sound.",
            ) {
                actions.push(PathAction::Solo(info.slot, !info.solo));
            }
            let label = if info.mute { "MUTED" } else { "MUTE" };
            if path_toggle(
                ui,
                label,
                info.mute,
                RED,
                label,
                "Silence this path on every device. The other paths keep playing.",
            ) {
                actions.push(PathAction::Mute(info.slot, !info.mute));
            }
        });
    });
    ui.label(
        RichText::new("Path stage · after Post EQ, before the shared Output.")
            .size(ts(11.0))
            .color(DIM),
    );
    ui.add_space(12.0);
    ui.label(mono(path_level_text(info.level_db), TEXT, 12.0));
    let mut level = info.level_db;
    if db_slider(
        ui,
        &mut level,
        PATH_LEVEL_MIN_DB,
        PATH_LEVEL_MAX_DB,
        (info.colour, format!("{} level", info.name)),
    ) {
        actions.push(PathAction::Level(info.slot, level));
    }
    ui.add_space(10.0);
    meter_row(ui, "L", meters.level[0], meters.peak[0], info.colour);
    meter_row(ui, "R", meters.level[1], meters.peak[1], info.colour);
    ui.label(
        RichText::new("Meters after the path level, mute and solo, before the master.")
            .size(ts(9.5))
            .color(DIM),
    );
    ui.add_space(6.0);
    let state = if !info.audible {
        if info.mute {
            ("Muted: this path is silent everywhere.", RED)
        } else {
            ("Another path is soloed, so this one is silent.", AMBER)
        }
    } else if !info.routed {
        ("Not routed: send it to a device on Signal Routing.", AMBER)
    } else {
        ("Playing. Per-device level sits on Signal Routing.", GREEN)
    };
    ui.label(RichText::new(state.0).size(ts(10.5)).color(state.1));

    ui.add_space(16.0);
    ui.label(micro_label("NAME", DIM));
    ui.add_space(2.0);
    name_field(
        ui,
        "inspector",
        info,
        ui.available_width().min(280.0),
        actions,
    );
    ui.add_space(10.0);
    ui.label(micro_label("COLOUR", DIM));
    colour_swatches(ui, info, actions);
    ui.add_space(14.0);
    ui.horizontal(|ui| {
        if ui
            .add_enabled(
                can_add,
                egui::Button::new(RichText::new("Duplicate path").size(ts(11.0)).color(TEAL))
                    .fill(with_alpha(TEAL, 14))
                    .stroke(Stroke::new(1.0, with_alpha(TEAL, 45)))
                    .corner_radius(CornerRadius::same(8)),
            )
            .on_hover_text("A full copy of this chain and its routing, added muted")
            .clicked()
        {
            actions.push(PathAction::Duplicate(info.slot));
        }
        if ui
            .add_enabled(
                can_remove,
                egui::Button::new(RichText::new("Remove path").size(ts(11.0)).color(DIM))
                    .fill(CARD_RAISED)
                    .stroke(Stroke::new(1.0, BORDER_SOFT))
                    .corner_radius(CornerRadius::same(8)),
            )
            .on_hover_text("Fade this path out and drop it with its inserts")
            .clicked()
        {
            actions.push(PathAction::Remove(info.slot));
        }
    });
}

#[cfg(test)]
mod tests;
