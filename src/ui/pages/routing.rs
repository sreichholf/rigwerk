//! Signal Routing, in the two layers the engine has:
//!
//! 1. Input -> Mix: which input channel feeds Mix L and Mix R, the bus the
//!    chain processes (`routes`, columns 0 and 1).
//! 2. Mix -> outputs: which device channels play the finished mix, per
//!    output device ([`outputs`]).
//!
//! Plus the Mix card (channel mode and monitoring).

mod outputs;

use eframe::egui;
use egui::{
    Align, Align2, Color32, CornerRadius, FontFamily, FontId, Layout, RichText, Sense, Stroke, Vec2,
};

use crate::app::RigWerkApp;
use crate::ui::theme::ts;
use crate::ui::theme::{
    with_alpha, AMBER, BLUE, BORDER, BORDER_SOFT, CARD, CARD_DEEP, DIM, MUTED, TEAL, TEXT,
};
use crate::ui::widgets::{
    card, card_fixed, ellipsize, horizontal_line, micro_label, mono, row_info, show_focus_ring,
    status_dot, tab_button_sized,
};

/// Side-by-side columns need 520 + 280 plus the gap. Below this available
/// width the Mix card is clipped, so the two cards stack instead.
pub(crate) const ROUTING_STACK_BELOW: f32 = 1100.0;

/// Mix sides layer ① feeds. The stored route table is still 2×4 for older
/// prefs, but only its first two columns mean anything: they are Mix L and
/// Mix R, the stereo bus the chain processes.
pub(crate) const MIX_COLUMNS: [&str; 2] = ["MIX L", "MIX R"];

/// Width and height of one matrix cell, in both layers.
pub(crate) const CELL: Vec2 = Vec2::new(58.0, 48.0);

/// Width of a matrix row header (plus the gap before the first cell).
pub(crate) const ROW_HEADER_W: f32 = 142.0;

/// Shared outer height for Input → Mix and Mix when they sit on one row.
pub(crate) fn routing_top_row_height(inputs: usize) -> f32 {
    let rows = inputs.max(1) as f32;
    // 15 px inset each side, then the taller Input → Mix body.
    let body = 22.0 // header
        + 16.0 // subtitle
        + 10.0
        + 24.0 // MIX L / MIX R headers
        + 2.0
        + rows * (CELL.y + 5.0)
        + 8.0
        + 1.0 // hairline
        + 8.0
        + 52.0; // tip
    (body + 30.0).max(300.0)
}

pub(crate) fn routing_columns_stack(available_width: f32) -> bool {
    available_width < ROUTING_STACK_BELOW
}

/// Size of layer ①: every input row onto the two mix sides.
pub(crate) fn input_matrix_label(inputs: usize) -> String {
    format!("{inputs} IN -> MIX L/R")
}

/// Stereo channel name for a matrix row.
pub(crate) fn input_channel_role(index: usize) -> &'static str {
    match index {
        0 => "Left",
        1 => "Right",
        _ => "Input",
    }
}

/// Backend and name of the selected input. ASIO appears only when that is
/// the device's own backend.
pub(crate) fn input_channel_caption(backend: &str, name: &str) -> String {
    let backend = backend.trim();
    let name = name.trim();
    if backend.is_empty() && name.is_empty() {
        "Input".to_owned()
    } else if backend.is_empty() {
        name.to_owned()
    } else if name.is_empty() {
        backend.to_uppercase()
    } else {
        format!("{} · {name}", backend.to_uppercase())
    }
}

/// Mix → Outputs header count. Playing channels are those a mix side
/// reaches plus unreached ones under Mono copy, which carry the average
/// (see `OutputDeviceSettings::playing_channels`).
pub(crate) fn patch_count_label(devices: usize, channels: usize) -> String {
    let plural = |count: usize, word: &str| {
        if count == 1 {
            format!("{count} {word}")
        } else {
            format!("{count} {word}S")
        }
    };
    format!(
        "{} · {} PLAYING",
        plural(devices, "DEVICE"),
        plural(channels, "CHANNEL")
    )
}

/// One routing cell: a dot when on, the faint `glyph` when off. `name` is
/// what a screen reader hears; the value says whether it is on.
pub(crate) fn matrix_cell(
    ui: &mut egui::Ui,
    enabled: bool,
    glyph: &str,
    name: String,
    values: (&str, &str),
) -> bool {
    let (rect, response) = ui.allocate_exact_size(CELL, Sense::click());
    let base = if enabled {
        with_alpha(TEAL, 18)
    } else if response.hovered() {
        with_alpha(TEXT, 7)
    } else {
        CARD_DEEP
    };
    ui.painter().rect_filled(rect, CornerRadius::same(8), base);
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(8),
        Stroke::new(
            1.0,
            if enabled {
                with_alpha(TEAL, 75)
            } else {
                BORDER_SOFT
            },
        ),
        egui::StrokeKind::Inside,
    );
    if enabled {
        ui.painter()
            .circle_filled(rect.center(), 9.0, with_alpha(TEAL, 26));
        ui.painter().circle_filled(rect.center(), 3.3, TEAL);
    } else {
        ui.painter().circle_stroke(
            rect.center(),
            7.0,
            Stroke::new(1.0, Color32::from_rgb(49, 60, 77)),
        );
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            glyph,
            FontId::new(ts(7.5), FontFamily::Monospace),
            Color32::from_rgb(53, 64, 80),
        );
    }
    show_focus_ring(ui, &response, rect, 8);
    row_info(
        &response,
        egui::Role::CheckBox,
        ui.is_enabled(),
        name,
        Some(enabled),
        Some(if enabled { values.0 } else { values.1 }.to_owned()),
    );
    response.clicked()
}

/// A layer ① cell: input `input` onto mix side `side` (0 = L, 1 = R).
pub(crate) fn route_cell(ui: &mut egui::Ui, enabled: bool, input: usize, side: usize) -> bool {
    let letter = if side == 0 { "L" } else { "R" };
    matrix_cell(
        ui,
        enabled,
        &format!("{}{letter}", input + 1),
        format!("Route input {} to Mix {letter}", input + 1),
        ("enabled", "muted"),
    )
}

/// A matrix row header: a name and a small caption under it.
pub(crate) fn row_header(ui: &mut egui::Ui, name: &str, caption: &str, color: Color32) {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(ROW_HEADER_W, CELL.y), Sense::hover());
    let _ = response.on_hover_text(format!("{name} · {caption}"));
    let painter = ui.painter().with_clip_rect(rect);
    painter.rect_filled(rect, CornerRadius::same(8), CARD_DEEP);
    painter.text(
        rect.left_center() + Vec2::new(12.0, -6.0),
        Align2::LEFT_CENTER,
        name,
        FontId::new(ts(11.5), FontFamily::Proportional),
        color,
    );
    painter.text(
        rect.left_center() + Vec2::new(12.0, 8.0),
        Align2::LEFT_CENTER,
        caption,
        FontId::new(ts(8.5), FontFamily::Proportional),
        DIM,
    );
}

/// A column header: a mono name and an optional caption under it.
pub(crate) fn column_header(ui: &mut egui::Ui, name: &str, caption: Option<&str>) {
    ui.add_sized(
        [CELL.x, if caption.is_some() { 28.0 } else { 18.0 }],
        egui::Label::new(match caption {
            Some(caption) => {
                let mut job = egui::text::LayoutJob {
                    halign: Align::Center,
                    ..Default::default()
                };
                job.append(
                    name,
                    0.0,
                    egui::TextFormat::simple(FontId::new(ts(9.0), FontFamily::Monospace), MUTED),
                );
                job.append(
                    &format!("\n{caption}"),
                    0.0,
                    egui::TextFormat::simple(FontId::new(ts(7.5), FontFamily::Monospace), DIM),
                );
                egui::WidgetText::from(job)
            }
            None => egui::WidgetText::from(mono(name, DIM, 9.0)),
        })
        .selectable(false),
    );
}

/// A small circled layer number ("1", "2") before a card title.
pub(crate) fn layer_badge(ui: &mut egui::Ui, number: usize, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
    ui.painter()
        .circle_stroke(rect.center(), 6.0, Stroke::new(1.0, color));
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        number.to_string(),
        FontId::new(ts(8.0), FontFamily::Proportional),
        color,
    );
}

impl RigWerkApp {
    pub(crate) fn routing_page(&mut self, ui: &mut egui::Ui) {
        let total = ui.available_width();
        if routing_columns_stack(total) {
            card(ui, CARD, BORDER, |ui| {
                ui.set_width(total - 32.0);
                self.routing_input_body(ui);
            });
            card(ui, CARD, BORDER, |ui| {
                ui.set_width(total - 32.0);
                self.routing_mix_body(ui);
            });
        } else {
            let left_w = (total * 0.66).clamp(520.0, (total - 288.0).max(520.0));
            let right_w = (total - left_w - 8.0).max(280.0);
            // Same outer height on both cards so the row stays level.
            let row_h = routing_top_row_height(self.path().mix.routes.len());
            ui.horizontal_top(|ui| {
                ui.allocate_ui_with_layout(
                    Vec2::new(left_w, row_h),
                    Layout::top_down(Align::LEFT),
                    |ui| {
                        ui.set_min_size(Vec2::new(left_w, row_h));
                        card_fixed(ui, CARD, BORDER, row_h, |ui| {
                            ui.set_width(left_w - 32.0);
                            self.routing_input_body(ui);
                        });
                    },
                );
                ui.allocate_ui_with_layout(
                    Vec2::new(right_w, row_h),
                    Layout::top_down(Align::LEFT),
                    |ui| {
                        ui.set_min_size(Vec2::new(right_w, row_h));
                        card_fixed(ui, CARD, BORDER, row_h, |ui| {
                            ui.set_width(right_w - 32.0);
                            self.routing_mix_body(ui);
                        });
                    },
                );
            });
        }

        let mut actions = Vec::new();
        self.routing_outputs_card(ui, total, &mut actions);
        for action in actions {
            self.apply_routing_action(action);
        }
    }

    /// Layer ①: input channels onto Mix L / Mix R.
    fn routing_input_body(&mut self, ui: &mut egui::Ui) {
        let input = self.active_input();
        let input_caption = input_channel_caption(&input.backend, &input.name);
        let subtitle = format!("{input_caption} -> the mix bus the chain processes");
        let input_count = self.path().mix.routes.len();

        ui.horizontal(|ui| {
            layer_badge(ui, 1, TEAL);
            ui.label(micro_label("INPUT -> MIX", TEAL));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(mono(input_matrix_label(input_count), DIM, 9.5));
            });
        });
        ui.label(
            RichText::new(ellipsize(&subtitle, 72))
                .size(ts(10.0))
                .color(MUTED),
        )
        .on_hover_text(&subtitle);
        ui.add_space(10.0);

        ui.horizontal(|ui| {
            ui.add_space(ROW_HEADER_W);
            for name in MIX_COLUMNS {
                column_header(ui, name, Some("into chain"));
            }
        });
        ui.add_space(2.0);

        for input in 0..input_count {
            let role = input_channel_role(input);
            ui.horizontal(|ui| {
                row_header(ui, &format!("IN {}", input + 1), role, TEXT);
                for (side, column) in MIX_COLUMNS.iter().enumerate() {
                    let routed = self.path().mix.routes[input][side];
                    if route_cell(ui, routed, input, side) {
                        let on = !routed;
                        self.path_mut().mix.routes[input][side] = on;
                        self.push_paths_to_engine();
                        self.push_log(
                            format!(
                                "Route {role} -> {column} {}.",
                                if on { "enabled" } else { "muted" }
                            ),
                            if on { TEAL } else { MUTED },
                        );
                    }
                }
            });
            ui.add_space(5.0);
        }

        ui.add_space(8.0);
        horizontal_line(ui, BORDER_SOFT);
        ui.add_space(8.0);
        ui.horizontal_top(|ui| {
            ui.label(RichText::new("Tip").size(ts(9.5)).color(AMBER).strong());
            ui.add(
                egui::Label::new(
                    RichText::new(
                        "Mono guitar on IN 1? Route it to both MIX L and MIX R, or use Mono sum. Which input channels are captured is still picked on Live.",
                    )
                    .size(ts(10.0))
                    .color(MUTED),
                )
                .wrap(),
            );
        });
    }

    /// Channel mode and monitoring: what happens on the mix bus.
    fn routing_mix_body(&mut self, ui: &mut egui::Ui) {
        ui.label(micro_label("MIX", AMBER));
        ui.add_space(8.0);
        ui.label(micro_label("CHANNEL MODE", DIM));
        ui.add_space(4.0);
        let mut mono = self.path().mix.mono;
        ui.horizontal(|ui| {
            if tab_button_sized(ui, "Stereo", !mono, BLUE, 11.0).clicked() {
                mono = false;
            }
            if tab_button_sized(ui, "Mono sum", mono, BLUE, 11.0).clicked() {
                mono = true;
            }
        });
        if mono != self.path().mix.mono {
            self.path_mut().mix.mono = mono;
        }
        ui.add_space(10.0);
        ui.label(micro_label("MONITORING", DIM));
        ui.add_space(4.0);
        ui.checkbox(
            &mut self.monitor,
            RichText::new("Direct monitor input")
                .size(ts(10.5))
                .color(MUTED),
        );
        ui.add_space(6.0);
        let mut drift = self.drift;
        if ui
            .checkbox(
                &mut drift,
                RichText::new("Automatic drift correction")
                    .size(ts(10.5))
                    .color(MUTED),
            )
            .changed()
        {
            self.drift = drift;
        }
        ui.add_space(10.0);
        horizontal_line(ui, BORDER_SOFT);
        ui.add_space(8.0);
        ui.horizontal_top(|ui| {
            status_dot(ui, TEAL);
            ui.add(
                egui::Label::new(
                    RichText::new(
                        "Both layers apply live. Layer 1 shapes what the chain hears; layer 2 only decides which device channels play the finished mix.",
                    )
                    .size(ts(9.5))
                    .color(MUTED),
                )
                .wrap(),
            );
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{
        input_channel_caption, input_channel_role, input_matrix_label, patch_count_label,
        routing_columns_stack, routing_top_row_height, MIX_COLUMNS,
    };
    use crate::ui::widgets::{accesskit_frame, ellipsize, node_named};
    use eframe::egui::accesskit::Toggled;

    fn input_row_caption(backend: &str, name: &str) -> String {
        ellipsize(&input_channel_caption(backend, name), 22)
    }

    #[test]
    fn input_rows_name_the_selected_device() {
        let pipewire = input_channel_caption("PIPEWIRE", "Analog In");
        assert_eq!(pipewire, "PIPEWIRE · Analog In");
        assert!(!pipewire.to_ascii_uppercase().contains("ASIO"));
        assert!(!pipewire.to_ascii_lowercase().contains("guitar"));
        assert!(!pipewire.to_ascii_lowercase().contains("spare"));

        let mic = input_channel_caption("wasapi", "Microphone");
        assert_eq!(mic, "WASAPI · Microphone");
        assert!(!mic.contains("ASIO"));

        let asio = input_channel_caption("ASIO", "Focusrite USB");
        assert_eq!(asio, "ASIO · Focusrite USB");
        assert!(!asio.to_ascii_lowercase().contains("guitar"));
        assert!(!asio.to_ascii_lowercase().contains("spare"));
    }

    #[test]
    fn a_long_device_name_stays_inside_the_row() {
        let shown = input_row_caption("PIPEWIRE", "Analog Input That Is Very Long");
        assert!(shown.starts_with("PIPEWIRE · "));
        assert!(shown.chars().count() <= 22);
        assert!(shown.contains('…'));
        assert!(!shown.to_ascii_uppercase().contains("ASIO"));
        assert!(!shown.to_ascii_lowercase().contains("guitar"));
    }

    #[test]
    fn layer_one_feeds_mix_l_and_mix_r() {
        assert_eq!(input_matrix_label(2), "2 IN -> MIX L/R");
        assert_eq!(MIX_COLUMNS, ["MIX L", "MIX R"]);
        assert_eq!(input_channel_role(0), "Left");
        assert_eq!(input_channel_role(1), "Right");
    }

    #[test]
    fn a_route_cell_exposes_its_name_and_enabled_value() {
        let tree = accesskit_frame(|ui| {
            assert!(!super::route_cell(ui, true, 0, 1));
            assert!(!super::route_cell(ui, false, 1, 0));
        });
        let node = node_named(&tree, "Route input 1 to Mix R");
        assert_eq!(node.role(), eframe::egui::Role::CheckBox);
        assert_eq!(node.value(), Some("enabled"));
        assert_eq!(node.toggled(), Some(Toggled::True));
        let off = node_named(&tree, "Route input 2 to Mix L");
        assert_eq!(off.value(), Some("muted"));
        assert_eq!(off.toggled(), Some(Toggled::False));
    }

    #[test]
    fn the_patch_header_counts_devices_and_playing_channels() {
        assert_eq!(patch_count_label(1, 2), "1 DEVICE · 2 CHANNELS PLAYING");
        assert_eq!(patch_count_label(3, 1), "3 DEVICES · 1 CHANNEL PLAYING");
        assert_eq!(patch_count_label(1, 0), "1 DEVICE · 0 CHANNELS PLAYING");
    }

    #[test]
    fn routing_columns_stack_below_1100_px() {
        assert!(routing_columns_stack(764.0));
        assert!(routing_columns_stack(1099.0));
        assert!(!routing_columns_stack(1100.0));
        assert!(!routing_columns_stack(1400.0));
    }

    #[test]
    fn side_by_side_routing_cards_share_one_height() {
        let two = routing_top_row_height(2);
        let four = routing_top_row_height(4);
        assert!(two >= 300.0);
        assert!(four > two);
    }
}
