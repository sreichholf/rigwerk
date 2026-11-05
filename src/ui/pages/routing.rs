//! Signal Routing: which input reaches which output tap.

use eframe::egui;
use egui::{
    Align, Align2, Color32, CornerRadius, FontFamily, FontId, Layout, Rect, RichText, Sense,
    Stroke, Vec2,
};

use crate::app::RigWerkApp;
use crate::ui::theme::ts;
use crate::ui::theme::{
    with_alpha, AMBER, BLUE, BORDER, BORDER_SOFT, CARD, CARD_DEEP, DIM, MUTED, TEAL, TEXT,
};
use crate::ui::widgets::{
    card, ellipsize, horizontal_line, micro_label, mono, route_summary, row_info, show_focus_ring,
    status_dot, tab_button_sized,
};

/// Side-by-side columns need 520 + 280 plus the gap. Below this available
/// width the patch card is clipped, so the two cards stack instead.
pub(crate) const ROUTING_STACK_BELOW: f32 = 1100.0;

/// The tap bars share one stereo output meter. They are not per-tap readings.
pub(crate) const SHARED_STEREO_METER_LABEL: &str = "SHARED STEREO METER";

/// Row note while a tap is showing that shared meter.
pub(crate) const SHARED_TAP_NOTE: &str = "SHARED";

/// Taps the mixer actually plays. The stored route table is still 2×4 for
/// older prefs, but only the first two columns reach EQ, inserts and the
/// stereo output device — columns 3 and 4 are dead weight in the engine.
pub(crate) const PLAYBACK_OUTPUTS: usize = 2;

pub(crate) fn routing_columns_stack(available_width: f32) -> bool {
    available_width < ROUTING_STACK_BELOW
}

/// Route-table size shown in the UI (what you can hear), not the full
/// stored matrix width.
pub(crate) fn matrix_size_label(inputs: usize, outputs: usize) -> String {
    format!("{inputs} IN -> {outputs} OUT")
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

/// Level for one output-tap bar.
///
/// The app publishes one stereo output meter, not a meter per tap. A routed
/// tap shows the louder of those two channels. An unrouted tap shows nothing.
/// The value does not depend on which tap it is.
pub(crate) fn shared_tap_level(routed: bool, meter_l: f32, meter_r: f32) -> f32 {
    if !routed {
        return 0.0;
    }
    meter_l.max(meter_r).clamp(0.0, 1.0)
}

pub(crate) fn route_cell(ui: &mut egui::Ui, enabled: bool, input: usize, output: usize) -> bool {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(58.0, 48.0), Sense::click());
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
            format!("{}{}", input + 1, output + 1),
            FontId::new(ts(7.5), FontFamily::Monospace),
            Color32::from_rgb(53, 64, 80),
        );
    }
    show_focus_ring(ui, &response, rect, 8);
    row_info(
        &response,
        egui::Role::CheckBox,
        ui.is_enabled(),
        format!("Route input {} to output {}", input + 1, output + 1),
        Some(enabled),
        Some(if enabled { "enabled" } else { "muted" }.to_owned()),
    );
    response.clicked()
}

pub(crate) fn tap_row(
    ui: &mut egui::Ui,
    output: usize,
    active: bool,
    level: f32,
    color: Color32,
    level_note: &str,
) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 18.0), Sense::hover());
    ui.painter().text(
        rect.left_center() + Vec2::new(2.0, 0.0),
        Align2::LEFT_CENTER,
        format!("OUT {}", output + 1),
        FontId::new(ts(8.5), FontFamily::Monospace),
        if active { MUTED } else { DIM },
    );
    let bar = Rect::from_min_size(
        egui::pos2(rect.left() + 58.0, rect.center().y - 2.0),
        Vec2::new((rect.width() - 106.0).max(20.0), 4.0),
    );
    ui.painter()
        .rect_filled(bar, CornerRadius::same(2), Color32::from_rgb(25, 32, 44));
    if active {
        ui.painter().rect_filled(
            Rect::from_min_size(
                bar.min,
                Vec2::new(bar.width() * level.clamp(0.0, 1.0), bar.height()),
            ),
            CornerRadius::same(2),
            color,
        );
    }
    ui.painter().text(
        rect.right_center() + Vec2::new(-2.0, 0.0),
        Align2::RIGHT_CENTER,
        if active { level_note } else { "MUTED" },
        FontId::new(ts(7.0), FontFamily::Monospace),
        if active { color } else { DIM },
    );
}

impl RigWerkApp {
    pub(crate) fn routing_page(&mut self, ui: &mut egui::Ui) {
        let total = ui.available_width();
        if routing_columns_stack(total) {
            self.routing_matrix_card(ui, total);
            self.routing_patch_card(ui, total);
            return;
        }

        let left_w = (total * 0.66).clamp(520.0, (total - 288.0).max(520.0));
        let right_w = (total - left_w - 8.0).max(280.0);
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(
                Vec2::new(left_w, 370.0),
                Layout::top_down(Align::LEFT),
                |ui| {
                    ui.set_min_size(Vec2::new(left_w, 370.0));
                    self.routing_matrix_card(ui, left_w);
                },
            );
            ui.allocate_ui_with_layout(
                Vec2::new(right_w, 370.0),
                Layout::top_down(Align::LEFT),
                |ui| {
                    ui.set_min_size(Vec2::new(right_w, 370.0));
                    self.routing_patch_card(ui, right_w);
                },
            );
        });
    }

    fn routing_matrix_card(&mut self, ui: &mut egui::Ui, width: f32) {
        let input = self.active_input();
        let output = self.selected_output();
        let input_caption = input_channel_caption(&input.backend, &input.name);
        let output_caption = input_channel_caption(&output.backend, &output.name);
        let path_caption = ellipsize(&format!("{input_caption}  ->  {output_caption}"), 64);
        let input_count = self.routes.len();
        // Only the stereo pair the mixer feeds to the device.
        let output_count = PLAYBACK_OUTPUTS.min(
            self.routes
                .first()
                .map(|row| row.len())
                .unwrap_or(PLAYBACK_OUTPUTS),
        );

        card(ui, CARD, BORDER, |ui| {
            ui.set_width(width - 32.0);
            ui.horizontal(|ui| {
                ui.label(micro_label("ROUTING MATRIX", TEAL));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(mono(matrix_size_label(input_count, output_count), DIM, 9.5));
                });
            });
            ui.label(RichText::new(&path_caption).size(ts(10.0)).color(MUTED))
                .on_hover_text(format!("{input_caption}  ->  {output_caption}"));
            ui.add_space(10.0);

            ui.horizontal(|ui| {
                ui.add_space(150.0);
                for output in 1..=output_count {
                    ui.add_sized(
                        [58.0, 18.0],
                        egui::Label::new(mono(format!("OUT {output}"), DIM, 9.0)),
                    );
                }
            });
            ui.add_space(2.0);

            for input in 0..input_count {
                let role = input_channel_role(input);
                ui.horizontal(|ui| {
                    let (rect, response) =
                        ui.allocate_exact_size(Vec2::new(142.0, 48.0), Sense::hover());
                    response.on_hover_text(format!("{role} · {input_caption}"));
                    let painter = ui.painter().with_clip_rect(rect);
                    painter.rect_filled(rect, CornerRadius::same(8), CARD_DEEP);
                    painter.text(
                        rect.left_center() + Vec2::new(12.0, -6.0),
                        Align2::LEFT_CENTER,
                        format!("IN {}", input + 1),
                        FontId::new(ts(11.5), FontFamily::Proportional),
                        TEXT,
                    );
                    painter.text(
                        rect.left_center() + Vec2::new(12.0, 8.0),
                        Align2::LEFT_CENTER,
                        role,
                        FontId::new(ts(8.5), FontFamily::Proportional),
                        DIM,
                    );
                    for output in 0..output_count {
                        if route_cell(ui, self.routes[input][output], input, output) {
                            self.routes[input][output] = !self.routes[input][output];
                            self.engine.set_routes(self.routes);
                            self.push_log(
                                format!(
                                    "Route {} -> OUT {} {}.",
                                    role,
                                    output + 1,
                                    if self.routes[input][output] {
                                        "enabled"
                                    } else {
                                        "muted"
                                    }
                                ),
                                if self.routes[input][output] {
                                    TEAL
                                } else {
                                    MUTED
                                },
                            );
                        }
                    }
                });
                ui.add_space(5.0);
            }

            ui.add_space(10.0);
            horizontal_line(ui, BORDER_SOFT);
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Tip").size(ts(9.5)).color(AMBER).strong());
                ui.label(
                    RichText::new(
                        "Default is Left->Out 1, Right->Out 2. Flip a cell to send a side to both outs or to mute it.",
                    )
                    .size(ts(10.0))
                    .color(MUTED),
                );
            });
            ui.add_space(9.0);
            horizontal_line(ui, BORDER_SOFT);
            ui.add_space(7.0);
            ui.horizontal(|ui| {
                ui.label(micro_label("OUTPUT TAPS", DIM));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(mono(SHARED_STEREO_METER_LABEL, DIM, 8.0));
                });
            });
            ui.add_space(3.0);
            for output in 0..output_count {
                let active = (0..input_count).any(|input| self.routes[input][output]);
                let level = shared_tap_level(active, self.meter_out_l, self.meter_out_r);
                tap_row(
                    ui,
                    output,
                    active,
                    level,
                    if output == 0 {
                        BLUE
                    } else {
                        with_alpha(BLUE, 160)
                    },
                    SHARED_TAP_NOTE,
                );
            }
        });
    }

    fn routing_patch_card(&mut self, ui: &mut egui::Ui, width: f32) {
        card(ui, CARD, BORDER, |ui| {
            ui.set_width(width - 32.0);
            ui.label(micro_label("ACTIVE PATCH", BLUE));
            ui.add_space(8.0);
            route_summary(ui, "SOURCE", &self.active_input().name, TEAL);
            ui.add_space(6.0);
            route_summary(ui, "DESTINATION", &self.selected_output().name, BLUE);
            ui.add_space(12.0);
            horizontal_line(ui, BORDER_SOFT);
            ui.add_space(9.0);
            ui.label(micro_label("CHANNEL MODE", DIM));
            ui.add_space(4.0);
            let mut mono = self.mono;
            if tab_button_sized(ui, "Stereo", !mono, BLUE, 11.0).clicked() {
                mono = false;
            }
            if tab_button_sized(ui, "Mono sum", mono, BLUE, 11.0).clicked() {
                mono = true;
            }
            self.mono = mono;
            ui.add_space(10.0);
            ui.label(micro_label("MONITORING", DIM));
            ui.add_space(4.0);
            ui.checkbox(
                &mut self.monitor,
                RichText::new("Direct monitor input")
                    .size(ts(10.5))
                    .color(MUTED),
            );
            ui.add_space(10.0);
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
                            "Routes apply live to the stereo pair of the selected output. The bridge is stereo today - only Out 1 and Out 2 are heard.",
                        )
                        .size(ts(9.0))
                        .color(MUTED),
                    )
                    .wrap(),
                );
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{
        input_channel_caption, input_channel_role, matrix_size_label, routing_columns_stack,
        shared_tap_level, PLAYBACK_OUTPUTS, SHARED_STEREO_METER_LABEL, SHARED_TAP_NOTE,
    };
    use crate::ui::widgets::ellipsize;

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
    fn the_matrix_size_follows_the_heard_taps() {
        assert_eq!(matrix_size_label(2, PLAYBACK_OUTPUTS), "2 IN -> 2 OUT");
        assert_eq!(input_channel_role(0), "Left");
        assert_eq!(input_channel_role(1), "Right");
        assert_eq!(PLAYBACK_OUTPUTS, 2);
    }

    #[test]
    fn routed_taps_repeat_the_stereo_meter() {
        assert_eq!(shared_tap_level(true, 0.4, 0.25), 0.4);
        assert_eq!(shared_tap_level(true, 0.1, 0.55), 0.55);
        assert_eq!(
            shared_tap_level(true, 0.4, 0.25),
            shared_tap_level(true, 0.4, 0.25)
        );
        assert_eq!(shared_tap_level(false, 0.4, 0.9), 0.0);
        assert_eq!(shared_tap_level(true, 0.0, 0.0), 0.0);
        assert_eq!(shared_tap_level(true, 1.4, 0.2), 1.0);
        let heading = SHARED_STEREO_METER_LABEL.to_ascii_uppercase();
        assert!(heading.contains("SHARED"));
        assert!(heading.contains("STEREO"));
        assert!(!heading.contains("LIVE"));
        assert_eq!(SHARED_TAP_NOTE, "SHARED");
    }

    #[test]
    fn a_route_cell_exposes_its_name_and_enabled_value() {
        use crate::ui::widgets::{accesskit_frame, node_named};
        use eframe::egui::accesskit::Toggled;

        let tree = accesskit_frame(|ui| {
            assert!(!super::route_cell(ui, true, 0, 1));
        });
        let node = node_named(&tree, "Route input 1 to output 2");
        assert_eq!(node.role(), eframe::egui::Role::CheckBox);
        assert_eq!(node.value(), Some("enabled"));
        assert_eq!(node.toggled(), Some(Toggled::True));
    }

    #[test]
    fn routing_columns_stack_below_1100_px() {
        assert!(routing_columns_stack(764.0));
        assert!(routing_columns_stack(1099.0));
        assert!(!routing_columns_stack(1100.0));
        assert!(!routing_columns_stack(1400.0));
    }
}
