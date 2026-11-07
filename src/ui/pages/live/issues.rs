//! The Live Issues strip: under the page header, above Input / Bridge /
//! Output (see `docs/mockups/live-issues-plan.md`).  One row per problem
//! from `RigWerkApp::issue_view` (the last tick's issues): severity,
//! title, one-line detail, where it is, the action button, and the
//! secondary ones: "Show on <card>" for an issue about a Live card and
//! Dismiss for a warning.  Hidden while
//! there is nothing to report and nothing dismissed; the header collapses
//! the rows to the worst title and "+N more", and carries the All /
//! Warnings / Errors filters and the way back to dismissed rows.
//!
//! Only draws.  The filter, the dismissals and the card highlight live in
//! `app::issues`; a click here calls those after the frame.  A new error
//! opens a collapsed strip again; that is decided per tick in
//! `RigWerkApp::tick_issues`, not here.  The ring on a card is painted by
//! [`RigWerkApp::card_highlight_ring`], which `live.rs` calls per card.

use std::time::Instant;

use eframe::egui;
use egui::{Align, Color32, CornerRadius, Layout, Margin, Rect, RichText, Sense, Stroke, Vec2};

use crate::app::issues::{IssueAction, IssueCard, IssueFilter, IssueKey, IssueSeverity, LiveIssue};
use crate::app::RigWerkApp;
use crate::ui::theme::ts;
use crate::ui::theme::{with_alpha, AMBER, BORDER, BORDER_SOFT, CARD, DIM, MUTED, RED, TEXT};
use crate::ui::widgets::{ellipsize, horizontal_line, micro_label, mono, row_info, tab_button};

/// What a click in the strip asks for, run once the frame is drawn.
enum StripClick {
    Toggle,
    Filter(IssueFilter),
    Restore,
    Action(IssueAction),
    Dismiss(IssueKey),
    Show(IssueCard, IssueSeverity),
}

fn severity_color(severity: IssueSeverity) -> Color32 {
    match severity {
        IssueSeverity::Error => RED,
        IssueSeverity::Warning => AMBER,
    }
}

/// "1 error · 2 warnings" for the strip header.
pub(crate) fn issue_counts(issues: &[LiveIssue]) -> String {
    let errors = issues
        .iter()
        .filter(|issue| issue.severity == IssueSeverity::Error)
        .count();
    let warnings = issues.len() - errors;
    let plural = |count: usize, word: &str| {
        if count == 1 {
            format!("1 {word}")
        } else {
            format!("{count} {word}s")
        }
    };
    match (errors, warnings) {
        (0, warnings) => plural(warnings, "warning"),
        (errors, 0) => plural(errors, "error"),
        (errors, warnings) => format!(
            "{} · {}",
            plural(errors, "error"),
            plural(warnings, "warning")
        ),
    }
}

/// A small severity tag ("ERROR", "WARNING").
fn severity_tag(ui: &mut egui::Ui, severity: IssueSeverity) {
    let color = severity_color(severity);
    let text = match severity {
        IssueSeverity::Error => "ERROR",
        IssueSeverity::Warning => "WARNING",
    };
    let font = egui::FontId::new(ts(8.0), egui::FontFamily::Monospace);
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, color);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(64.0, 18.0), Sense::hover());
    let tag = egui::Rect::from_min_size(rect.min, Vec2::new(galley.size().x + 12.0, 18.0));
    ui.painter()
        .rect_filled(tag, CornerRadius::same(5), with_alpha(color, 20));
    ui.painter().rect_stroke(
        tag,
        CornerRadius::same(5),
        Stroke::new(1.0, with_alpha(color, 110)),
        egui::StrokeKind::Inside,
    );
    ui.painter()
        .galley(tag.center() - galley.size() / 2.0, galley, color);
}

/// The colour a filter button uses while it is chosen.
fn filter_color(filter: IssueFilter) -> Color32 {
    match filter {
        IssueFilter::All => TEXT,
        IssueFilter::Warnings => AMBER,
        IssueFilter::Errors => RED,
    }
}

/// A quiet row button for the secondary actions, beside the coloured one.
fn secondary_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new(label).size(ts(9.5)).color(MUTED))
            .fill(Color32::TRANSPARENT)
            .stroke(Stroke::new(1.0, BORDER))
            .corner_radius(CornerRadius::same(7)),
    )
}

/// The width a row button with `label` takes, spacing included.
fn button_width(ui: &egui::Ui, label: &str) -> f32 {
    let font = egui::FontId::proportional(ts(9.5));
    let text = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font, TEXT)
        .size()
        .x;
    text + ui.spacing().button_padding.x * 2.0 + ui.spacing().item_spacing.x
}

impl RigWerkApp {
    /// The strip, when there are issues or dismissed ones.  Clicks run
    /// after drawing.
    pub(crate) fn issues_strip(&mut self, ui: &mut egui::Ui, width: f32) {
        let view = self.issue_view();
        if view.is_empty() {
            return;
        }
        let filter = self.issue_state.filter;
        let worst = view.visible.first();
        let color = worst.map_or(DIM, |issue| severity_color(issue.severity));
        let border = worst.map_or(BORDER, |_| with_alpha(color, 110));
        let counts = if view.visible.is_empty() {
            "nothing open".to_owned()
        } else {
            issue_counts(&view.visible)
        };
        let mut click: Option<StripClick> = None;
        let collapsed = self.live_issues_collapsed;
        egui::Frame::new()
            .fill(CARD)
            .stroke(Stroke::new(1.0, border))
            .corner_radius(CornerRadius::same(13))
            .inner_margin(Margin::symmetric(15, 9))
            .show(ui, |ui| {
                ui.set_width(width - 32.0);
                ui.horizontal(|ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        // Right to left: Errors, Warnings, All read left to right.
                        for choice in IssueFilter::ALL.into_iter().rev() {
                            let count = view.count(choice);
                            let label = match choice {
                                IssueFilter::All => choice.label().to_owned(),
                                _ if count == 0 => choice.label().to_owned(),
                                _ => format!("{} {count}", choice.label()),
                            };
                            let button =
                                tab_button(ui, &label, filter == choice, filter_color(choice));
                            row_info(
                                &button,
                                egui::Role::Button,
                                true,
                                format!("Issue filter: {}", choice.label()),
                                Some(filter == choice),
                                Some(count.to_string()),
                            );
                            if button.clicked() {
                                click = Some(StripClick::Filter(choice));
                            }
                        }
                        if view.hidden > 0 {
                            ui.add_space(8.0);
                            let button =
                                secondary_button(ui, &format!("{} dismissed · show", view.hidden));
                            row_info(
                                &button,
                                egui::Role::Button,
                                true,
                                "Show dismissed issues",
                                None,
                                Some(view.hidden.to_string()),
                            );
                            if button.clicked() {
                                click = Some(StripClick::Restore);
                            }
                        }
                        ui.add_space(8.0);
                        // The rest of the header toggles the rows.
                        let area = ui.available_rect_before_wrap();
                        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                            ui.label(mono(if collapsed { "▸" } else { "▾" }, MUTED, 10.0));
                            ui.label(micro_label("ISSUES", color));
                            ui.label(mono(&counts, color, 9.0));
                            if collapsed {
                                if let Some(worst) = worst {
                                    ui.add_space(8.0);
                                    let more = if view.visible.len() > 1 {
                                        format!(" +{} more", view.visible.len() - 1)
                                    } else {
                                        String::new()
                                    };
                                    ui.label(
                                        RichText::new(format!(
                                            "{}{more}",
                                            ellipsize(&worst.title, 70)
                                        ))
                                        .size(ts(10.0))
                                        .color(MUTED),
                                    );
                                }
                            }
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.label(mono(if collapsed { "show" } else { "hide" }, DIM, 8.5));
                            });
                        });
                        let header =
                            ui.interact(area, ui.scope_id().with("issues-toggle"), Sense::click());
                        row_info(
                            &header,
                            egui::Role::Button,
                            true,
                            "Live issues",
                            Some(!collapsed),
                            Some(counts.clone()),
                        );
                        if header.clicked() {
                            click = Some(StripClick::Toggle);
                        }
                    });
                });
                if collapsed {
                    return;
                }
                let mut shown = 0;
                for issue in view.filtered(filter) {
                    shown += 1;
                    ui.add_space(6.0);
                    horizontal_line(ui, BORDER_SOFT);
                    ui.add_space(6.0);
                    if let Some(row) = issue_row(ui, issue) {
                        click = Some(row);
                    }
                }
                if shown == 0 {
                    ui.add_space(6.0);
                    horizontal_line(ui, BORDER_SOFT);
                    ui.add_space(6.0);
                    let note = if view.visible.is_empty() {
                        "Every issue here is dismissed.".to_owned()
                    } else {
                        format!("{} right now.", filter.empty_note())
                    };
                    ui.label(RichText::new(note).size(ts(9.8)).color(MUTED));
                }
            });
        match click {
            Some(StripClick::Toggle) => self.live_issues_collapsed = !self.live_issues_collapsed,
            Some(StripClick::Filter(choice)) => self.set_issue_filter(choice),
            Some(StripClick::Restore) => self.restore_dismissed_issues(),
            Some(StripClick::Action(action)) => self.apply_issue_action(action),
            Some(StripClick::Dismiss(key)) => self.dismiss_issue(key),
            Some(StripClick::Show(card, severity)) => {
                self.show_issue_card(card, severity, Instant::now());
            }
            None => {}
        }
        ui.add_space(10.0);
    }

    /// The ring around a Live card while "Show on <card>" lights it: drawn
    /// just outside `rect` in the issue's colour, pulsing out.
    pub(crate) fn card_highlight_ring(&self, ui: &egui::Ui, rect: Rect, card: IssueCard) {
        let Some((severity, strength)) = self.card_highlight(card, Instant::now()) else {
            return;
        };
        let alpha = (strength * 220.0).round().clamp(0.0, 255.0) as u8;
        ui.painter().rect_stroke(
            rect.expand(3.0),
            CornerRadius::same(16),
            Stroke::new(2.0, with_alpha(severity_color(severity), alpha)),
            egui::StrokeKind::Outside,
        );
        ui.ctx().request_repaint();
    }
}

/// One issue row; returns the click on one of its buttons.
fn issue_row(ui: &mut egui::Ui, issue: &LiveIssue) -> Option<StripClick> {
    let mut click = None;
    let show_label = issue.show_label();
    ui.horizontal(|ui| {
        severity_tag(ui, issue.severity);
        let mut buttons_w = button_width(ui, &issue.action.label());
        if let Some(label) = &show_label {
            buttons_w += button_width(ui, label);
        }
        if issue.dismissable() {
            buttons_w += button_width(ui, "Dismiss");
        }
        let body_w = (ui.available_width() - buttons_w - 16.0).max(120.0);
        ui.allocate_ui_with_layout(
            Vec2::new(body_w, 0.0),
            Layout::top_down(Align::LEFT),
            |ui| {
                ui.set_width(body_w);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&issue.title).size(ts(11.0)).color(TEXT));
                    ui.label(mono(&issue.place, DIM, 8.5));
                });
                ui.add(
                    egui::Label::new(RichText::new(&issue.detail).size(ts(9.8)).color(MUTED))
                        .wrap(),
                );
            },
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let color = severity_color(issue.severity);
            let button = ui.add(
                egui::Button::new(
                    RichText::new(issue.action.label())
                        .size(ts(9.5))
                        .color(color),
                )
                .fill(with_alpha(color, 14))
                .stroke(Stroke::new(1.0, with_alpha(color, 70)))
                .corner_radius(CornerRadius::same(7)),
            );
            row_info(
                &button,
                egui::Role::Button,
                true,
                issue.action_name(),
                None,
                None,
            );
            if button.clicked() {
                click = Some(StripClick::Action(issue.action));
            }
            if let (Some(card), Some(label), Some(name)) =
                (issue.card, &show_label, issue.show_name())
            {
                let button = secondary_button(ui, label);
                row_info(&button, egui::Role::Button, true, name, None, None);
                if button.clicked() {
                    click = Some(StripClick::Show(card, issue.severity));
                }
            }
            if issue.dismissable() {
                let button = secondary_button(ui, "Dismiss");
                row_info(
                    &button,
                    egui::Role::Button,
                    true,
                    issue.dismiss_name(),
                    None,
                    None,
                );
                if button.clicked() {
                    click = Some(StripClick::Dismiss(issue.key));
                }
            }
        });
    });
    click
}

#[cfg(test)]
mod tests;
