//! The Live Issues strip: under the page header, above Input / Bridge /
//! Output (see `docs/mockups/live-issues-plan.md`).  One row per problem
//! from `RigWerkApp::live_issues`: severity, title, one-line detail, where
//! it is, and one button.  Hidden while there is nothing to report; the
//! header collapses the rows to the worst title and "+N more".

use eframe::egui;
use egui::{Align, CornerRadius, Layout, Margin, RichText, Sense, Stroke, Vec2};

use crate::app::issues::{IssueAction, IssueSeverity, LiveIssue};
use crate::app::RigWerkApp;
use crate::ui::theme::ts;
use crate::ui::theme::{with_alpha, AMBER, BORDER_SOFT, CARD, DIM, MUTED, RED, TEXT};
use crate::ui::widgets::{ellipsize, horizontal_line, micro_label, mono, row_info};

fn severity_color(severity: IssueSeverity) -> egui::Color32 {
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

impl RigWerkApp {
    /// The strip, when there are issues.  Actions run after drawing.
    pub(crate) fn issues_strip(&mut self, ui: &mut egui::Ui, width: f32) {
        let issues = self.live_issues();
        let Some(worst) = issues.first() else {
            return;
        };
        let color = severity_color(worst.severity);
        let mut action: Option<IssueAction> = None;
        let mut toggle = false;
        let collapsed = self.live_issues_collapsed;
        egui::Frame::new()
            .fill(CARD)
            .stroke(Stroke::new(1.0, with_alpha(color, 110)))
            .corner_radius(CornerRadius::same(13))
            .inner_margin(Margin::symmetric(15, 9))
            .show(ui, |ui| {
                ui.set_width(width - 32.0);
                let header = ui
                    .horizontal(|ui| {
                        ui.label(mono(if collapsed { "▸" } else { "▾" }, MUTED, 10.0));
                        ui.label(micro_label("ISSUES", color));
                        ui.label(mono(issue_counts(&issues), color, 9.0));
                        if collapsed {
                            ui.add_space(8.0);
                            let more = if issues.len() > 1 {
                                format!(" +{} more", issues.len() - 1)
                            } else {
                                String::new()
                            };
                            ui.label(
                                RichText::new(format!("{}{more}", ellipsize(&worst.title, 90)))
                                    .size(ts(10.0))
                                    .color(MUTED),
                            );
                        }
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(mono(if collapsed { "show" } else { "hide" }, DIM, 8.5));
                        });
                    })
                    .response;
                let header = ui.interact(header.rect, header.id.with("toggle"), Sense::click());
                row_info(
                    &header,
                    egui::Role::Button,
                    true,
                    "Live issues",
                    Some(!collapsed),
                    Some(issue_counts(&issues)),
                );
                if header.clicked() {
                    toggle = true;
                }
                if collapsed {
                    return;
                }
                for issue in &issues {
                    ui.add_space(6.0);
                    horizontal_line(ui, BORDER_SOFT);
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        severity_tag(ui, issue.severity);
                        let button_w = 132.0;
                        let body_w = (ui.available_width() - button_w - 12.0).max(120.0);
                        ui.allocate_ui_with_layout(
                            Vec2::new(body_w, 0.0),
                            Layout::top_down(Align::LEFT),
                            |ui| {
                                ui.set_width(body_w);
                                ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new(&issue.title).size(ts(11.0)).color(TEXT),
                                    );
                                    ui.label(mono(&issue.place, DIM, 8.5));
                                });
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(&issue.detail).size(ts(9.8)).color(MUTED),
                                    )
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
                                action = Some(issue.action);
                            }
                        });
                    });
                }
            });
        if toggle {
            self.live_issues_collapsed = !self.live_issues_collapsed;
        }
        if let Some(action) = action {
            self.apply_issue_action(action);
        }
        ui.add_space(10.0);
    }
}

#[cfg(test)]
mod tests;
