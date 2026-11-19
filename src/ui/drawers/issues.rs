//! The issues drawer: the list inside the right-hand drawer.  The shell
//! paints the drawer and the word Issues.  One row per problem from
//! `RigWerkApp::issue_view` (the last tick's issues): severity, title, one
//! line of detail, where it is, the action button, *Show* and *Dismiss*
//! for a warning that allows it.
//!
//! Only draws.  The filter and the dismissals live in `app::issues`; a
//! click here calls those after the frame.  *Show* closes the drawer and
//! selects the row's anchor.  A new error sets the banner in
//! `RigWerkApp::tick_issues`, not here.

use eframe::egui;
use egui::{Align, Color32, CornerRadius, Layout, RichText, Stroke};

use crate::app::issues::{Anchor, IssueAction, IssueFilter, IssueKey, IssueSeverity, LiveIssue};
use crate::app::RigWerkApp;
use crate::ui::fonts::Face;
use crate::ui::theme::{font, palette, TypeRole};
use crate::ui::widgets::row_info;

/// What a click in the list asks for, run once the frame is drawn.
enum RowClick {
    Filter(IssueFilter),
    Restore,
    Action(IssueAction),
    Dismiss(IssueKey),
    Show(Anchor),
}

fn severity_color(ui: &egui::Ui, severity: IssueSeverity) -> Color32 {
    let colours = palette(ui.ctx());
    match severity {
        IssueSeverity::Error => colours.error,
        IssueSeverity::Warning => colours.warn,
    }
}

fn severity_word(severity: IssueSeverity) -> &'static str {
    match severity {
        IssueSeverity::Error => "Error",
        IssueSeverity::Warning => "Warning",
    }
}

/// The colour a filter button uses while it is chosen.
fn filter_color(ui: &egui::Ui, filter: IssueFilter) -> Color32 {
    let colours = palette(ui.ctx());
    match filter {
        IssueFilter::All => colours.text,
        IssueFilter::Warnings => colours.warn,
        IssueFilter::Errors => colours.error,
    }
}

fn choice_button(ui: &mut egui::Ui, label: &str, on: bool, color: Color32) -> egui::Response {
    let colours = palette(ui.ctx());
    ui.add(
        egui::Button::new(
            RichText::new(label)
                .font(font(Face::Sans, TypeRole::Secondary))
                .color(if on { color } else { colours.text2 }),
        )
        .fill(if on {
            colours.accent_bg
        } else {
            Color32::TRANSPARENT
        })
        .stroke(Stroke::new(1.0, if on { color } else { colours.line2 }))
        .corner_radius(CornerRadius::same(4)),
    )
}

fn action_button(ui: &mut egui::Ui, label: &str, color: Color32) -> egui::Response {
    ui.add(
        egui::Button::new(
            RichText::new(label)
                .font(font(Face::Sans, TypeRole::Secondary))
                .color(color),
        )
        .fill(color.gamma_multiply(0.14))
        .stroke(Stroke::new(1.0, color.gamma_multiply(0.55)))
        .corner_radius(CornerRadius::same(4)),
    )
}

fn quiet_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    let colours = palette(ui.ctx());
    ui.add(
        egui::Button::new(
            RichText::new(label)
                .font(font(Face::Sans, TypeRole::Secondary))
                .color(colours.text),
        )
        .fill(colours.slot)
        .stroke(Stroke::new(1.0, colours.line2))
        .corner_radius(CornerRadius::same(4)),
    )
}

impl RigWerkApp {
    /// The list, when there are issues or dismissed ones.  Clicks run
    /// after drawing.  `width` is the drawer's outer width; the shell has
    /// already inset the row.
    pub(crate) fn issues_strip(&mut self, ui: &mut egui::Ui, _width: f32) {
        let view = self.issue_view();
        if view.is_empty() {
            return;
        }
        let colours = palette(ui.ctx());
        let filter = self.issue_state.filter;
        let mut click: Option<RowClick> = None;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            for choice in IssueFilter::ALL {
                let count = view.count(choice);
                let label = match choice {
                    IssueFilter::All => choice.label().to_owned(),
                    _ if count == 0 => choice.label().to_owned(),
                    _ => format!("{} {count}", choice.label()),
                };
                let button = choice_button(ui, &label, filter == choice, filter_color(ui, choice));
                row_info(
                    &button,
                    egui::Role::Button,
                    true,
                    format!("Issue filter: {}", choice.label()),
                    Some(filter == choice),
                    Some(count.to_string()),
                );
                if button.clicked() {
                    click = Some(RowClick::Filter(choice));
                }
            }
        });
        ui.add_space(8.0);
        let footer = if view.hidden > 0 { 40.0 } else { 0.0 };
        let height = (ui.available_height() - footer).max(40.0);
        egui::ScrollArea::vertical()
            .id_salt("issues_list")
            .max_height(height)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let mut shown = 0;
                for issue in view.filtered(filter) {
                    if shown > 0 {
                        ui.add_space(8.0);
                        let rect = ui.available_rect_before_wrap();
                        ui.painter().hline(
                            rect.x_range(),
                            ui.cursor().top(),
                            Stroke::new(1.0, colours.line),
                        );
                        ui.add_space(8.0);
                    }
                    shown += 1;
                    if let Some(row) = issue_row(ui, issue) {
                        click = Some(row);
                    }
                }
                if shown == 0 {
                    let note = if view.visible.is_empty() {
                        "Every issue here is dismissed.".to_owned()
                    } else {
                        format!("{} right now.", filter.empty_note())
                    };
                    ui.label(
                        RichText::new(note)
                            .font(font(Face::Sans, TypeRole::Body))
                            .color(colours.text2),
                    );
                }
            });
        if view.hidden > 0 {
            ui.add_space(8.0);
            let button = quiet_button(ui, "Show dismissed");
            row_info(
                &button,
                egui::Role::Button,
                true,
                "Show dismissed",
                None,
                Some(view.hidden.to_string()),
            );
            if button.clicked() {
                click = Some(RowClick::Restore);
            }
        }
        match click {
            Some(RowClick::Filter(choice)) => self.set_issue_filter(choice),
            Some(RowClick::Restore) => self.restore_dismissed_issues(),
            Some(RowClick::Action(action)) => self.apply_issue_action(action),
            Some(RowClick::Dismiss(key)) => self.dismiss_issue(key),
            Some(RowClick::Show(anchor)) => self.show_issue(anchor),
            None => {}
        }
    }
}

/// One issue.  Returns the click on one of its buttons.
fn issue_row(ui: &mut egui::Ui, issue: &LiveIssue) -> Option<RowClick> {
    let colours = palette(ui.ctx());
    let mut click = None;
    let color = severity_color(ui, issue.severity);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.label(
            RichText::new(severity_word(issue.severity))
                .font(font(Face::SansMedium, TypeRole::Secondary))
                .color(color),
        );
        ui.vertical(|ui| {
            ui.label(
                RichText::new(&issue.title)
                    .font(font(Face::SansMedium, TypeRole::Control))
                    .color(colours.text),
            );
            ui.add(
                egui::Label::new(
                    RichText::new(&issue.detail)
                        .font(font(Face::Sans, TypeRole::Body))
                        .color(colours.text2),
                )
                .truncate(),
            );
            ui.label(
                RichText::new(&issue.place)
                    .font(font(Face::Sans, TypeRole::Secondary))
                    .color(colours.text3),
            );
        });
    });
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let button = action_button(ui, &issue.action.label(), color);
            row_info(
                &button,
                egui::Role::Button,
                true,
                issue.action_name(),
                None,
                None,
            );
            if button.clicked() {
                click = Some(RowClick::Action(issue.action));
            }
            let show = quiet_button(ui, issue.show_label());
            row_info(
                &show,
                egui::Role::Button,
                true,
                issue.show_name(),
                None,
                None,
            );
            if show.clicked() {
                click = Some(RowClick::Show(issue.anchor));
            }
            if issue.dismissable() {
                let button = quiet_button(ui, "Dismiss");
                row_info(
                    &button,
                    egui::Role::Button,
                    true,
                    issue.dismiss_name(),
                    None,
                    None,
                );
                if button.clicked() {
                    click = Some(RowClick::Dismiss(issue.key));
                }
            }
        });
    });
    click
}

#[cfg(test)]
mod tests;
