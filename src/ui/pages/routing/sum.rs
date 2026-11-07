//! The sum block of Signal Routing's layer ③, shown on a device that
//! plays several paths: per path its level, polarity, trim and alignment
//! delay there, then the latency line and the latency bars, and the
//! device's align switch.

use eframe::egui;
use egui::{
    Align, Align2, Color32, CornerRadius, FontFamily, FontId, Layout, RichText, Sense, Stroke, Vec2,
};

use crate::app::alignment::DeviceAlignment;
use crate::app::paths::PathInfo;
use crate::audio::{OutputDeviceSettings, PathMapSettings, MAX_ALIGN};
use crate::ui::pages::mixer::output_devices::{block, OutputDeviceRow};
use crate::ui::theme::ts;
use crate::ui::theme::{with_alpha, AMBER, BLUE, BORDER_SOFT, CARD, DIM, MUTED, RED, TEXT};
use crate::ui::widgets::{
    fit_text, horizontal_line, micro_label, mono, path_toggle, tab_button_sized,
};

use super::outputs::MapEdit;
use super::paths::{level_stepper, trim_stepper};

/// Under the sum block when several paths play on one device.
pub(crate) const SUM_NOTE: &str =
    "Cells in one column add up. Master level and mute (Mixer -> Output) act after the sum.";

/// What the sum block shows on one device besides the settings.
pub(super) struct SumView<'a> {
    /// The paths this device plays, in display order.
    pub(super) reaching: &'a [&'a PathInfo],
    pub(super) alignment: &'a DeviceAlignment,
    /// Path slots in an unaligned sum here (the red warning).
    pub(super) unaligned: &'a [usize],
}

/// Under the latency bars: what alignment does not cover.
pub(crate) const ALIGN_SCOPE_NOTE: &str = "Alignment lines up the paths on this device only. Each device still has its own buffer, and a follower its cushion, so devices differ from each other.";

/// The device's "Align paths  On | Off" switch.  Returns the new state on
/// a click that changes it.
pub(super) fn align_switch(ui: &mut egui::Ui, align: bool, device: &str) -> Option<bool> {
    let mut changed = None;
    // Right to left: Off, On, then the label.
    let off = tab_button_sized(ui, "Off", !align, RED, 9.5);
    let off = off.on_hover_text(format!(
        "Each path plays on {device} as soon as its chain finishes (lowest latency; summed paths comb filter)"
    ));
    if off.clicked() && align {
        changed = Some(false);
    }
    let on = tab_button_sized(ui, "On", align, BLUE, 9.5).on_hover_text(format!(
        "Every path on {device} waits for the slowest one there"
    ));
    if on.clicked() && !align {
        changed = Some(true);
    }
    ui.label(micro_label("ALIGN PATHS", DIM));
    changed
}

/// The sum block: per path on this device, its level, polarity and trim
/// and the alignment delay it gets; then the latency line, the latency
/// bars and what the alignment does not cover.
pub(super) fn sum_block(
    ui: &mut egui::Ui,
    row: &OutputDeviceRow,
    settings: &OutputDeviceSettings,
    view: &SumView,
    edits: &mut Vec<MapEdit>,
) {
    block(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(micro_label(
                &format!("SUM ON {}", row.name.to_uppercase()),
                BLUE,
            ));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let (text, color) = if view.alignment.align {
                    ("ALIGNED", BLUE)
                } else {
                    (
                        "ALIGNMENT OFF",
                        if view.unaligned.is_empty() {
                            AMBER
                        } else {
                            RED
                        },
                    )
                };
                ui.label(mono(text, color, 8.5));
            });
        });
        ui.add_space(6.0);
        egui::ScrollArea::horizontal()
            .id_salt(("routing_sum_scroll", row.name.as_str()))
            .auto_shrink([false, true])
            .show(ui, |ui| {
                egui::Grid::new(("routing_sum", row.name.as_str()))
                    .num_columns(5)
                    .spacing([16.0, 8.0])
                    .show(ui, |ui| {
                        for header in ["PATH", "LEVEL HERE", "POLARITY", "TRIM", "ALIGNMENT"] {
                            ui.label(micro_label(header, DIM));
                        }
                        ui.end_row();
                        for info in view.reaching {
                            let (text, color) = alignment_cell(view, info);
                            sum_row(ui, row, settings.path_map[info.slot], info, edits);
                            ui.label(mono(text, color, 9.0));
                            ui.end_row();
                        }
                    });
            });
        ui.add_space(6.0);
        horizontal_line(ui, BORDER_SOFT);
        ui.add_space(6.0);
        ui.add(
            egui::Label::new(
                RichText::new(latency_line(&row.name, view.alignment, view.reaching))
                    .size(ts(10.0))
                    .color(if view.alignment.clamped { RED } else { MUTED }),
            )
            .wrap(),
        );
        ui.add_space(6.0);
        latency_bars(ui, view);
        ui.add_space(6.0);
        ui.add(egui::Label::new(RichText::new(ALIGN_SCOPE_NOTE).size(ts(9.5)).color(DIM)).wrap());
    });
}

/// One path's level, polarity and trim in the sum block (the alignment
/// cell follows).
fn sum_row(
    ui: &mut egui::Ui,
    row: &OutputDeviceRow,
    entry: PathMapSettings,
    info: &PathInfo,
    edits: &mut Vec<MapEdit>,
) {
    ui.horizontal(|ui| {
        let (dot, _) = ui.allocate_exact_size(Vec2::new(8.0, 18.0), Sense::hover());
        ui.painter().circle_filled(dot.center(), 4.0, info.colour);
        ui.label(RichText::new(&info.name).size(ts(10.5)).color(TEXT));
        ui.label(mono(format!("{:.1} ms", info.latency_ms), DIM, 8.5))
            .on_hover_text("The path's own chain latency (its plugins)");
    });
    if let Some(step) = level_stepper(
        ui,
        entry.level_db,
        &format!("{} level on {}", info.name, row.name),
    ) {
        edits.push(MapEdit::Level {
            slot: info.slot,
            level_db: step.level_db,
            commit: step.commit,
        });
    }
    if path_toggle(
        ui,
        "ø",
        entry.invert,
        AMBER,
        &format!("Flip polarity of {} on {}", info.name, row.name),
        "Flip this path's polarity on this device",
    ) {
        edits.push(MapEdit::Invert {
            slot: info.slot,
            invert: !entry.invert,
        });
    }
    if let Some(step) = trim_stepper(
        ui,
        entry.trim_ms,
        &format!("{} trim on {}", info.name, row.name),
    ) {
        edits.push(MapEdit::Trim {
            slot: info.slot,
            trim_ms: step.level_db,
            commit: step.commit,
        });
    }
}

/// The alignment cell of one path: the delay it gets here, whether it
/// sets the pace, or why it is out of line.
fn alignment_cell(view: &SumView, info: &PathInfo) -> (String, Color32) {
    let alignment = view.alignment;
    let delay = alignment.per_path[info.slot].unwrap_or(0.0);
    if info.latency_samples > MAX_ALIGN {
        return ("past the limit · plays late".to_owned(), RED);
    }
    if view.unaligned.contains(&info.slot) {
        let text = if delay >= 0.05 {
            format!("delay {delay:.2} ms · unaligned")
        } else {
            "unaligned".to_owned()
        };
        return (text, RED);
    }
    if delay >= 0.05 {
        return (format!("waits {delay:.2} ms"), BLUE);
    }
    if alignment.align && alignment.waits_for == Some(info.slot) {
        return ("slowest · sets the pace".to_owned(), DIM);
    }
    ("no delay".to_owned(), DIM)
}

/// One bar per path: its own chain latency in its colour, then the
/// alignment delay hatched, and the arrival in ms.
fn latency_bars(ui: &mut egui::Ui, view: &SumView) {
    let arrival = |info: &PathInfo| {
        view.alignment.arrival_ms[info.slot]
            .unwrap_or(info.latency_ms)
            .max(0.0)
    };
    let longest = view
        .reaching
        .iter()
        .map(|info| arrival(info))
        .fold(0.0_f32, f32::max);
    // Short latencies stay short bars: the scale never drops below 6 ms.
    let scale = longest.max(6.0);
    let name_w = 110.0;
    let value_w = 64.0;
    for info in view.reaching {
        ui.horizontal(|ui| {
            let font = FontId::new(ts(9.0), FontFamily::Monospace);
            let (name_rect, _) = ui.allocate_exact_size(Vec2::new(name_w, 14.0), Sense::hover());
            let name = fit_text(ui, &info.name, &font, name_w - 4.0);
            ui.painter().text(
                name_rect.left_center(),
                Align2::LEFT_CENTER,
                name,
                font.clone(),
                MUTED,
            );
            let track_w = (ui.available_width() - value_w - 8.0).max(60.0);
            let (track, response) = ui.allocate_exact_size(Vec2::new(track_w, 8.0), Sense::hover());
            let painter = ui.painter().with_clip_rect(track);
            painter.rect_filled(track, CornerRadius::same(3), CARD);
            let own = info.latency_ms.max(0.0).min(arrival(info));
            let pad = (arrival(info) - own).max(0.0);
            let x = |ms: f32| track.left() + track.width() * (ms / scale).min(1.0);
            let own_rect = egui::Rect::from_min_max(track.min, egui::pos2(x(own), track.bottom()));
            painter.rect_filled(own_rect, CornerRadius::ZERO, with_alpha(info.colour, 230));
            if pad > 0.0 {
                let pad_rect = egui::Rect::from_min_max(
                    egui::pos2(x(own), track.top()),
                    egui::pos2(x(own + pad), track.bottom()),
                );
                let hatch = painter.with_clip_rect(pad_rect.intersect(track));
                let mut start = pad_rect.left() - track.height();
                while start < pad_rect.right() {
                    hatch.line_segment(
                        [
                            egui::pos2(start, track.bottom()),
                            egui::pos2(start + track.height(), track.top()),
                        ],
                        Stroke::new(1.5, with_alpha(BLUE, 150)),
                    );
                    start += 4.0;
                }
            }
            painter.rect_stroke(
                track,
                CornerRadius::same(3),
                Stroke::new(1.0, BORDER_SOFT),
                egui::StrokeKind::Inside,
            );
            let _ = response.on_hover_text(format!(
                "{}: {own:.1} ms chain + {pad:.1} ms alignment",
                info.name
            ));
            let (value, _) = ui.allocate_exact_size(Vec2::new(value_w, 14.0), Sense::hover());
            ui.painter().text(
                value.right_center(),
                Align2::RIGHT_CENTER,
                format!("{:.1} ms", own + pad),
                font,
                MUTED,
            );
        });
    }
}

/// The latency line under the sum: what the device waits for and how much
/// that adds, or with the alignment off how far apart the paths arrive.
pub(crate) fn latency_line(
    device: &str,
    alignment: &DeviceAlignment,
    reaching: &[&PathInfo],
) -> String {
    let name_of = |slot: usize| {
        reaching
            .iter()
            .find(|info| info.slot == slot)
            .map(|info| info.name.as_str())
    };
    let mut text = if reaching.len() < 2 {
        let name = reaching.first().map_or("", |info| info.name.as_str());
        format!(
            "Nothing to align: {name} is the only path on {device} ({:.1} ms).",
            alignment.total_ms
        )
    } else if alignment.align {
        match alignment.waits_for.and_then(name_of) {
            // Within a frame: the paths line up exactly.
            Some(slowest) if alignment.spread_frames() <= 1 => format!(
                "Waits for {slowest}, +{:.1} ms. Every path on {device} plays {:.1} ms after its chain input.",
                alignment.added_ms, alignment.total_ms
            ),
            Some(slowest) => format!(
                "Waits for {slowest}, +{:.1} ms. Paths on {device} play up to {:.1} ms after their chain input (trims shift paths by up to {:.2} ms).",
                alignment.added_ms,
                alignment.total_ms,
                alignment.spread_ms()
            ),
            None if alignment.added_ms >= 0.05 => format!(
                "Nothing to align; the trims add up to +{:.1} ms on {device}.",
                alignment.added_ms
            ),
            None => format!(
                "Nothing to align: every path on {device} has the same latency ({:.1} ms).",
                alignment.total_ms
            ),
        }
    } else {
        let spread = alignment.spread_ms();
        if spread >= 0.05 {
            format!(
                "Alignment off: each path plays on {device} as its chain finishes, up to {spread:.1} ms apart."
            )
        } else {
            format!("Alignment off: the paths on {device} arrive together anyway.")
        }
    };
    if alignment.clamped {
        text.push_str(&format!(
            " A chain is longer than the alignment covers ({MAX_ALIGN} frames) and plays later than shown."
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::super::outputs::{channel_list, MapEdit};
    use super::latency_line;
    use crate::app::blank_app;
    use crate::app::cards::Card;
    use crate::ui::widgets::{accesskit_frame, node_named};

    #[test]
    fn the_latency_line_says_what_the_device_waits_for() {
        let mut app = blank_app();
        assert!(app.apply_card(Card::Blend));
        let infos = app.path_infos();
        let both = [&infos[0], &infos[1]];
        let line = |app: &crate::app::RigWerkApp| {
            latency_line("Scarlett", &app.device_alignment(0).unwrap(), &both)
        };
        assert_eq!(
            line(&app),
            "Nothing to align: every path on Scarlett has the same latency (0.0 ms)."
        );
        assert_eq!(
            latency_line("Scarlett", &app.device_alignment(0).unwrap(), &both[..1]),
            "Nothing to align: Main is the only path on Scarlett (0.0 ms)."
        );
        // 2 ms of plugin latency on the second path: the first waits.
        app.engine.screenshot_path_latency(infos[1].slot, 96);
        assert_eq!(
            line(&app),
            "Waits for Main 2, +2.0 ms. Every path on Scarlett plays 2.0 ms after its chain input."
        );
        // A trim on the first path: they no longer arrive together.
        app.clock_device.path_map[infos[0].slot].trim_ms = 0.5;
        assert_eq!(
            line(&app),
            "Waits for Main 2, +2.5 ms. Paths on Scarlett play up to 2.5 ms after their chain input (trims shift paths by up to 0.50 ms)."
        );
        app.clock_device.path_map[infos[0].slot].trim_ms = 0.0;
        app.set_device_align(0, false);
        assert_eq!(
            line(&app),
            "Alignment off: each path plays on Scarlett as its chain finishes, up to 2.0 ms apart."
        );
        app.engine.screenshot_path_latency(infos[1].slot, 0);
        assert_eq!(channel_list(&[0, 2]), "Out 1, Out 3");
    }

    #[test]
    fn the_sum_block_edits_trim_and_align_and_flags_the_unaligned_sum() {
        let mut app = blank_app();
        assert!(app.apply_card(Card::DualAmp));
        let second = app.path_slots()[1];
        app.apply_map_edit(
            0,
            MapEdit::Trim {
                slot: second,
                trim_ms: 9.0,
                commit: true,
            },
        );
        assert_eq!(app.clock_device.path_map[second].trim_ms, 5.0, "clamped");
        assert!(app
            .log
            .iter()
            .any(|entry| entry.text.contains("Main 2 trim on")));
        app.apply_map_edit(
            0,
            MapEdit::Trim {
                slot: second,
                trim_ms: 0.0,
                commit: false,
            },
        );
        app.apply_map_edit(0, MapEdit::Align(false));
        assert!(!app.device_align(0));
        app.engine.screenshot_path_latency(second, 96);
        let tree = accesskit_frame(|ui| app.routing_page(ui));
        let trim = format!("Main 2 trim on {}", app.output_device_rows()[0].name);
        let _ = node_named(&tree, &trim);
        let _ = node_named(&tree, "Align paths");
        app.apply_map_edit(0, MapEdit::Align(true));
        assert!(app.device_align(0));
        assert!(app.routing_warnings(0).is_empty());
        app.engine.screenshot_path_latency(second, 0);
    }
}
