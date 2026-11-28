//! The effect browser body.  The shell draws the frame and the close.
//! Only the visible rows are built.

use eframe::egui;
use egui::{Align, CornerRadius, Layout, Rect, RichText, Sense, Stroke, Vec2};

use crate::app::plugins::{
    category_tags, position_phrase, visible_effects, EffectFilter, PluginCandidate,
};
use crate::app::RigWerkApp;
use crate::audio::PluginFormat;
use crate::ui::fonts::Face;
use crate::ui::theme::{font, palette, TypeRole};
use crate::ui::widgets::{chip, palette_focus, row_info, OutlineButton, TextField};

const ROW_H: f32 = 52.0;
const NOTICE_SECS: u64 = 8;

enum RowAction {
    Choose,
    Add,
    Favourite,
}

impl RigWerkApp {
    pub(crate) fn effect_browser(&mut self, ui: &mut egui::Ui) {
        let path = self.plugin_browser.path;
        let path_name = self
            .paths
            .get(path)
            .and_then(|slot| slot.as_ref())
            .map(|slot| slot.name.clone())
            .unwrap_or_else(|| "Path".to_owned());
        let names: Vec<String> = self
            .engine_plugin_views(path)
            .iter()
            .map(|view| view.name.clone())
            .collect();
        let phrase = position_phrase(&names, self.plugin_browser.position);
        let path_id = ui.make_persistent_id("effect_browser_path");
        let path_focused = ui.memory(|memory| memory.has_focus(path_id));
        let (up, down, enter, shift_enter) = if path_focused {
            (false, false, false, false)
        } else {
            ui.input_mut(|input| {
                let up = input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp);
                let down = input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown);
                let shift_enter = input.consume_key(egui::Modifiers::SHIFT, egui::Key::Enter);
                let enter = input.consume_key(egui::Modifiers::NONE, egui::Key::Enter);
                (up, down, enter, shift_enter)
            })
        };

        let mut change = false;
        header(ui, &path_name, &phrase, &mut change);
        let mut search = self.plugin_browser.search.clone();
        let search_response = search_field(ui, &mut search, self.plugin_browser.focus_search);
        self.plugin_browser.focus_search = false;
        self.plugin_browser.search = search;
        if search_response.changed() {
            self.plugin_browser.chosen = 0;
            self.plugin_browser.follow = true;
        }

        let tags = category_tags(&self.plugin_browser.library);
        filters(ui, self, &tags);

        let rows: Vec<PluginCandidate> = {
            let browser = &self.plugin_browser;
            visible_effects(
                &browser.library,
                &browser.search,
                &browser.filter,
                browser.show_vst3,
                browser.show_clap,
                browser.show_lv2,
                &browser.favourites,
                &browser.recent,
            )
            .into_iter()
            .cloned()
            .collect()
        };
        if !rows.is_empty() {
            if up {
                self.plugin_browser.chosen = self.plugin_browser.chosen.saturating_sub(1);
                self.plugin_browser.follow = true;
            }
            if down {
                self.plugin_browser.chosen = (self.plugin_browser.chosen + 1).min(rows.len() - 1);
                self.plugin_browser.follow = true;
            }
            if self.plugin_browser.chosen >= rows.len() {
                self.plugin_browser.chosen = rows.len() - 1;
            }
        }

        let mut picked: Option<(PluginCandidate, bool)> = None;
        let mut starred: Option<PluginCandidate> = None;
        list(ui, self, &rows, &mut picked, &mut starred);
        notice(ui, self);
        let mut add_path = false;
        let mut rescan = false;
        footer(ui, self, path_id, &mut add_path, &mut rescan);

        if change {
            self.cycle_browser_position();
        }
        if let Some(candidate) = starred {
            self.toggle_favourite(&candidate);
        }
        if enter || shift_enter {
            if let Some(candidate) = rows.get(self.plugin_browser.chosen).cloned() {
                picked = Some((candidate, shift_enter));
            }
        }
        if let Some((candidate, open_editor)) = picked {
            self.add_listed_effect(&candidate, open_editor);
        }
        if add_path {
            self.add_plugin_from_path();
        }
        if rescan && !self.plugin_browser.scanning {
            self.begin_plugin_scan();
        }
    }
}

fn header(ui: &mut egui::Ui, path_name: &str, phrase: &str, change: &mut bool) {
    let colours = palette(ui.ctx());
    let title = format!("Add to {path_name}");
    let heading = ui.label(
        RichText::new(title.clone())
            .font(font(Face::SansSemibold, TypeRole::Title))
            .color(colours.text),
    );
    row_info(
        &heading,
        egui::Role::Heading,
        true,
        title,
        None,
        Some(phrase.to_owned()),
    );
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(phrase)
                .font(font(Face::Sans, TypeRole::Secondary))
                .color(colours.text2),
        );
        ui.label(
            RichText::new("·")
                .font(font(Face::Sans, TypeRole::Secondary))
                .color(colours.text3),
        );
        let response = ui.add(
            egui::Button::new(
                RichText::new("change")
                    .font(font(Face::Sans, TypeRole::Secondary))
                    .color(colours.accent_text),
            )
            .fill(colours.accent_bg)
            .stroke(Stroke::new(1.0, colours.accent))
            .corner_radius(CornerRadius::same(3)),
        );
        row_info(
            &response,
            egui::Role::Button,
            true,
            "change",
            None,
            Some(phrase.to_owned()),
        );
        if response.clicked() {
            *change = true;
        }
        response.on_hover_text("Change where the next effect is added");
    });
}

fn search_field(ui: &mut egui::Ui, search: &mut String, focus: bool) -> egui::Response {
    let response = TextField::new(search, "Search").hint("Search").show(ui);
    if focus {
        response.request_focus();
    }
    response
}

fn filters(ui: &mut egui::Ui, app: &mut RigWerkApp, tags: &[String]) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);
        filter_chip(ui, app, "All", EffectFilter::All);
        filter_chip(ui, app, "Favourites", EffectFilter::Favourites);
        filter_chip(ui, app, "Recent", EffectFilter::Recent);
        for tag in tags {
            filter_chip(ui, app, tag, EffectFilter::Category(tag.clone()));
        }
    });
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        format_chip(ui, app, PluginFormat::Vst3);
        format_chip(ui, app, PluginFormat::Clap);
        format_chip(ui, app, PluginFormat::Lv2);
    });
}

fn filter_chip(ui: &mut egui::Ui, app: &mut RigWerkApp, label: &str, filter: EffectFilter) {
    let mut on = app.plugin_browser.filter == filter;
    if chip(ui, &mut on, label).clicked() {
        app.plugin_browser.filter = filter;
        app.plugin_browser.chosen = 0;
        app.plugin_browser.follow = true;
    }
}

fn format_chip(ui: &mut egui::Ui, app: &mut RigWerkApp, format: PluginFormat) {
    let browser = &mut app.plugin_browser;
    let mut on = match format {
        PluginFormat::Vst3 => browser.show_vst3,
        PluginFormat::Clap => browser.show_clap,
        PluginFormat::Lv2 => browser.show_lv2,
    };
    if chip(ui, &mut on, format.label()).clicked() {
        match format {
            PluginFormat::Vst3 => browser.show_vst3 = on,
            PluginFormat::Clap => browser.show_clap = on,
            PluginFormat::Lv2 => browser.show_lv2 = on,
        }
        if !browser.show_vst3 && !browser.show_clap && !browser.show_lv2 {
            // The last one clicked stays on, so the list is never empty by
            // format alone.
            match format {
                PluginFormat::Vst3 => browser.show_vst3 = true,
                PluginFormat::Clap => browser.show_clap = true,
                PluginFormat::Lv2 => browser.show_lv2 = true,
            }
        }
        browser.chosen = 0;
    }
}

fn list(
    ui: &mut egui::Ui,
    app: &mut RigWerkApp,
    rows: &[PluginCandidate],
    picked: &mut Option<(PluginCandidate, bool)>,
    starred: &mut Option<PluginCandidate>,
) {
    let colours = palette(ui.ctx());
    let list_h = (ui.available_height() - 128.0).max(ROW_H);
    ui.allocate_ui_with_layout(
        Vec2::new(ui.available_width(), list_h),
        Layout::top_down(Align::Min),
        |ui| {
            ui.set_width(ui.available_width());
            ui.set_min_height(list_h);
            if app.plugin_browser.scanning && app.plugin_browser.library.is_empty() {
                ui.label(
                    RichText::new("Scanning…")
                        .font(font(Face::Sans, TypeRole::Body))
                        .color(colours.text2),
                );
                return;
            }
            if rows.is_empty() {
                ui.label(
                    RichText::new("No effects match.")
                        .font(font(Face::Sans, TypeRole::Body))
                        .color(colours.text2),
                );
                return;
            }
            let follow = app.plugin_browser.follow;
            let chosen = app.plugin_browser.chosen;
            app.plugin_browser.follow = false;
            let query = app.plugin_browser.search.clone();
            ui.spacing_mut().item_spacing.y = 0.0;
            egui::ScrollArea::vertical()
                .id_salt("effect_browser_rows")
                .auto_shrink([false, false])
                .show_viewport(ui, |ui, viewport| {
                    let total = rows.len() as f32 * ROW_H;
                    ui.set_height(total);
                    if follow {
                        let y = ui.max_rect().top() + chosen as f32 * ROW_H;
                        ui.scroll_to_rect(
                            Rect::from_min_size(
                                egui::pos2(ui.max_rect().left(), y),
                                Vec2::new(1.0, ROW_H),
                            ),
                            Some(Align::Center),
                        );
                    }
                    let min_row = ((viewport.min.y / ROW_H).floor() as usize).min(rows.len());
                    let max_row = ((viewport.max.y / ROW_H).ceil() as usize + 1).min(rows.len());
                    let y_min = ui.max_rect().top() + min_row as f32 * ROW_H;
                    let y_max = ui.max_rect().top() + max_row as f32 * ROW_H;
                    let rect = Rect::from_x_y_ranges(ui.max_rect().x_range(), y_min..=y_max);
                    ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                        ui.skip_ahead_auto_ids(min_row);
                        for (index, candidate) in rows
                            .iter()
                            .enumerate()
                            .skip(min_row)
                            .take(max_row.saturating_sub(min_row))
                        {
                            let holders = holder_names(app, candidate);
                            let favourite = app
                                .plugin_browser
                                .favourites
                                .iter()
                                .any(|id| id.matches(candidate));
                            match effect_row(
                                ui,
                                candidate,
                                &query,
                                index == chosen,
                                favourite,
                                &holders,
                            ) {
                                Some(RowAction::Choose) => app.plugin_browser.chosen = index,
                                Some(RowAction::Add) => *picked = Some((candidate.clone(), false)),
                                Some(RowAction::Favourite) => *starred = Some(candidate.clone()),
                                None => {}
                            }
                        }
                    });
                });
        },
    );
}

fn holder_names(app: &RigWerkApp, candidate: &PluginCandidate) -> Vec<String> {
    let wanted = candidate.path.to_string_lossy().into_owned();
    app.path_order
        .iter()
        .filter_map(|slot| {
            let held = app.engine_plugin_views(*slot).iter().any(|view| {
                view.path == wanted
                    && view.format == candidate.format
                    && view.class_id == candidate.class_id
            });
            if !held {
                return None;
            }
            app.paths
                .get(*slot)
                .and_then(|path| path.as_ref())
                .map(|path| path.name.clone())
        })
        .collect()
}

fn effect_row(
    ui: &mut egui::Ui,
    candidate: &PluginCandidate,
    query: &str,
    selected: bool,
    favourite: bool,
    holders: &[String],
) -> Option<RowAction> {
    let colours = palette(ui.ctx());
    let width = ui.available_width().max(80.0);
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, ROW_H), Sense::click());
    let fill = if selected {
        colours.accent_bg
    } else {
        colours.slot
    };
    ui.painter().rect_filled(rect, CornerRadius::same(3), fill);
    if selected {
        ui.painter().rect_stroke(
            rect,
            CornerRadius::same(3),
            Stroke::new(1.0, colours.accent),
            egui::StrokeKind::Inside,
        );
    }
    let star = Rect::from_center_size(
        egui::pos2(rect.right() - 16.0, rect.center().y),
        Vec2::splat(22.0),
    );
    let text_right = star.left() - 8.0;
    paint_name(
        ui,
        egui::pos2(rect.left() + 12.0, rect.center().y - 9.0),
        text_right,
        &candidate.name,
        query,
        if candidate.scan_error.is_some() {
            colours.error_text
        } else {
            colours.text
        },
    );
    let detail = if let Some(error) = &candidate.scan_error {
        error.clone()
    } else {
        detail_line(candidate, holders)
    };
    ui.painter().text(
        egui::pos2(rect.left() + 12.0, rect.center().y + 9.0),
        egui::Align2::LEFT_CENTER,
        detail.clone(),
        font(Face::Sans, TypeRole::Secondary),
        if candidate.scan_error.is_some() {
            colours.error_text
        } else {
            colours.text2
        },
    );
    let star_response = ui.interact(star, response.id.with("star"), Sense::click());
    paint_star(
        ui.painter(),
        star.center(),
        6.0,
        if favourite {
            colours.accent_text
        } else {
            colours.text3
        },
        favourite,
    );
    let star_name = format!("Favourite {}", candidate.name);
    row_info(
        &star_response,
        egui::Role::Button,
        true,
        star_name,
        Some(favourite),
        None,
    );
    row_info(
        &response,
        egui::Role::Button,
        true,
        candidate.name.clone(),
        Some(selected),
        Some(detail),
    );
    palette_focus(ui, &response, rect);
    let pointer = response.interact_pointer_pos();
    let response = response.on_hover_text(if candidate.scan_error.is_some() {
        "Enter shows why this bundle cannot be added."
    } else {
        "Enter adds this effect. Shift+Enter also opens its editor."
    });
    let on_star = pointer.is_some_and(|pos| star.contains(pos));
    if star_response.clicked() || (on_star && response.clicked()) {
        return Some(RowAction::Favourite);
    }
    if response.double_clicked() {
        return Some(RowAction::Add);
    }
    if response.clicked() {
        return Some(RowAction::Choose);
    }
    None
}

/// Plex has no star glyph, so this is drawn.
fn paint_star(
    painter: &egui::Painter,
    centre: egui::Pos2,
    radius: f32,
    colour: egui::Color32,
    filled: bool,
) {
    let mut points = Vec::with_capacity(10);
    for step in 0..5 {
        let outer = -std::f32::consts::FRAC_PI_2 + step as f32 * std::f32::consts::TAU / 5.0;
        points.push(centre + Vec2::new(outer.cos(), outer.sin()) * radius);
        let inner = outer + std::f32::consts::TAU / 10.0;
        points.push(centre + Vec2::new(inner.cos(), inner.sin()) * radius * 0.45);
    }
    if filled {
        // The centre sits inside the star, so these slices fill it.
        for index in 0..points.len() {
            let next = points[(index + 1) % points.len()];
            painter.add(egui::Shape::convex_polygon(
                vec![centre, points[index], next],
                colour,
                Stroke::NONE,
            ));
        }
    } else {
        painter.add(egui::Shape::closed_line(points, Stroke::new(1.0, colour)));
    }
}

fn paint_name(
    ui: &egui::Ui,
    origin: egui::Pos2,
    right: f32,
    name: &str,
    query: &str,
    colour: egui::Color32,
) {
    let body = font(Face::Sans, TypeRole::Body);
    let strong = font(Face::SansSemibold, TypeRole::Body);
    let (before, hit, after) = split_match(name, query);
    let mut x = origin.x;
    for (text, face) in [(before, &body), (hit, &strong), (after, &body)] {
        if text.is_empty() || x >= right {
            continue;
        }
        let shown = crate::ui::widgets::fit_text(ui, text, face, right - x);
        let painted = ui.painter().text(
            egui::pos2(x, origin.y),
            egui::Align2::LEFT_CENTER,
            shown,
            face.clone(),
            colour,
        );
        x = painted.right();
    }
}

/// Only ASCII is split.  Anything else is drawn in the body face.
fn split_match<'a>(name: &'a str, query: &str) -> (&'a str, &'a str, &'a str) {
    let query = query.trim();
    if query.is_empty() || !name.is_ascii() || !query.is_ascii() {
        return (name, "", "");
    }
    let Some(start) = name.to_lowercase().find(&query.to_lowercase()) else {
        return (name, "", "");
    };
    let end = start + query.len();
    (&name[..start], &name[start..end], &name[end..])
}

fn detail_line(candidate: &PluginCandidate, holders: &[String]) -> String {
    let mut parts = Vec::new();
    if !candidate.vendor.trim().is_empty() {
        parts.push(candidate.vendor.clone());
    }
    parts.push(candidate.format.label().to_owned());
    if let Some(channels) = candidate.channels {
        parts.push(match channels {
            1 => "mono".to_owned(),
            2 => "stereo".to_owned(),
            other => other.to_string(),
        });
    }
    let mut line = parts.join(" · ");
    if !holders.is_empty() {
        if !line.is_empty() {
            line.push_str(" · ");
        }
        line.push_str("in ");
        line.push_str(&holders.join(", "));
    }
    line
}

fn notice(ui: &mut egui::Ui, app: &RigWerkApp) {
    let Some((text, at)) = &app.plugin_browser.notice else {
        return;
    };
    if at.elapsed().as_secs() >= NOTICE_SECS {
        return;
    }
    let colours = palette(ui.ctx());
    let bad = {
        let lower = text.to_lowercase();
        lower.contains("could not") || lower.contains("does not") || lower.contains("failed")
    };
    let response = ui.label(
        RichText::new(text)
            .font(font(Face::Sans, TypeRole::Secondary))
            .color(if bad {
                colours.error_text
            } else {
                colours.text
            }),
    );
    row_info(
        &response,
        egui::Role::Status,
        true,
        text.clone(),
        None,
        None,
    );
}

fn footer(
    ui: &mut egui::Ui,
    app: &mut RigWerkApp,
    path_id: egui::Id,
    add_path: &mut bool,
    rescan: &mut bool,
) {
    let colours = palette(ui.ctx());
    ui.label(
        RichText::new("Add from path")
            .font(font(Face::Sans, TypeRole::Secondary))
            .color(colours.text2),
    );
    ui.horizontal(|ui| {
        let mut path = app.plugin_browser.path_input.clone();
        let field = TextField::new(&mut path, "Add from path")
            .hint("Path to a .vst3, .clap or .lv2 bundle")
            .id(path_id)
            .width((ui.available_width() - 150.0).max(80.0))
            .show(ui);
        app.plugin_browser.path_input = path;
        if field.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)) {
            *add_path = true;
        }
        if OutlineButton::new("Add").show(ui).clicked() {
            *add_path = true;
        }
        let scanning = app.plugin_browser.scanning;
        let label = if scanning { "Scanning…" } else { "Rescan" };
        let response = OutlineButton::new(label).enabled(!scanning).show(ui);
        if response.clicked() {
            *rescan = true;
        }
    });
}

#[cfg(test)]
mod tests;
