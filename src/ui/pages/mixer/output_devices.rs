//! Output devices under the master Output stage: their rows in the chain, the
//! device list in the master inspector and the clock device inspector.
//!
//! Only the clock device is listed for now. Followers are stored in the
//! session but nothing plays them yet; steps 2 and 3 add their rows, their
//! inspector and the add list.

use eframe::egui;
use egui::{
    Align, Align2, Color32, CornerRadius, FontFamily, FontId, Layout, Margin, RichText, Sense,
    Stroke, Vec2,
};

use crate::app::settings::rate_label;
use crate::app::RigWerkApp;
use crate::audio::{
    EngineState, OutputDeviceSettings, DEVICE_LEVEL_MAX_DB, DEVICE_LEVEL_MIN_DB,
    MAX_OUTPUT_CHANNELS,
};
use crate::ui::pages::mixer::chain::ChainSelection;
use crate::ui::pages::mixer::rack::RackAction;
use crate::ui::theme::ts;
use crate::ui::theme::{
    with_alpha, AMBER, BLUE, BORDER_SOFT, CARD_DEEP, DIM, GREEN, MUTED, RED, TEAL, TEXT,
};
use crate::ui::widgets::{
    db_slider, meter_row, micro_label, mono, row_info, show_focus_ring, small_fact,
};

/// Why "+ add output device" is greyed out.
pub(crate) const ADD_DEVICE_HINT: &str = "Followers come in a later version";

/// One output device as the Mixer lists it.
#[derive(Clone, Debug)]
pub(crate) struct OutputDeviceRow {
    pub(crate) name: String,
    pub(crate) backend: String,
    /// Channels the meters show: the real count, capped at
    /// [`MAX_OUTPUT_CHANNELS`]. See [`device_channel_count`].
    pub(crate) channels: usize,
    /// The device whose callback runs the chain.
    pub(crate) clock: bool,
    pub(crate) settings: OutputDeviceSettings,
}

/// Channels to show for a device: what its open stream reported, else what
/// the device scan found, else stereo; never more than the map can address.
// An ASIO scan may report more than 4 channels, but a running ASIO stream
// opens at most 4, so the count can drop when the bridge starts.
pub(crate) fn device_channel_count(reported: u32, inventory: usize) -> usize {
    let channels = if reported > 0 {
        reported as usize
    } else if inventory > 0 {
        inventory
    } else {
        2
    };
    channels.min(MAX_OUTPUT_CHANNELS)
}

/// The device level as the rows show it.
pub(crate) fn device_level_text(settings: &OutputDeviceSettings) -> String {
    if settings.mute {
        "muted".to_owned()
    } else {
        format!("{:+.1} dB", settings.level_db)
    }
}

/// A device's state word and colour. While only the clock device plays, it
/// is the bridge's state.
pub(crate) fn device_state(state: EngineState) -> (&'static str, Color32) {
    match state {
        EngineState::Running => ("running", GREEN),
        EngineState::Starting => ("starting", AMBER),
        EngineState::Preview => ("preview", AMBER),
        EngineState::Error => ("error", RED),
        EngineState::Idle | EngineState::Stopping => ("stopped", DIM),
    }
}

/// What a screen reader hears after a row's name.
pub(crate) fn device_row_value(row: &OutputDeviceRow, state: EngineState) -> String {
    let role = if row.clock {
        "clock device"
    } else {
        "follower"
    };
    format!(
        "{role} · {} · {}",
        device_level_text(&row.settings),
        device_state(state).0
    )
}

impl RigWerkApp {
    /// The output devices under the master, clock device first.
    pub(crate) fn output_device_rows(&self) -> Vec<OutputDeviceRow> {
        let output = self.selected_output();
        // A count from a stream that is no longer open may belong to another
        // device, so only a running bridge's count is trusted.
        let reported = if self.engine.state == EngineState::Running {
            self.snapshot.output_channels
        } else {
            0
        };
        // Followers (`self.followers`) join this list in steps 2 and 3, once
        // something plays them.
        vec![OutputDeviceRow {
            name: output.name.clone(),
            backend: output.backend.clone(),
            channels: device_channel_count(reported, output.channels),
            clock: true,
            settings: self.clock_device,
        }]
    }

    /// The device rows under the Output stage in the chain list, and the
    /// greyed-out add button.
    pub(super) fn output_device_children(
        &mut self,
        ui: &mut egui::Ui,
        rows: &[OutputDeviceRow],
        select: &mut Option<ChainSelection>,
    ) {
        let state = self.engine.state;
        for (index, row) in rows.iter().enumerate() {
            let selected = self.chain_selected == ChainSelection::OutputDevice(index);
            let (clicked, toggle_mute) = chain_device_row(ui, row, selected, state);
            if toggle_mute && row.clock {
                self.clock_device.mute = !self.clock_device.mute;
            }
            if clicked {
                *select = Some(ChainSelection::OutputDevice(index));
            }
        }
        ui.add_space(4.0);
        ui.add_enabled(
            false,
            egui::Button::new(
                RichText::new("+ add output device")
                    .size(ts(10.0))
                    .color(DIM),
            )
            .fill(Color32::TRANSPARENT)
            .stroke(Stroke::new(1.0, BORDER_SOFT))
            .corner_radius(CornerRadius::same(8))
            .min_size(Vec2::new(ui.available_width(), 30.0)),
        )
        .on_disabled_hover_text(ADD_DEVICE_HINT);
    }

    /// The "Output devices" block of the master inspector.
    pub(super) fn output_devices_block(
        &mut self,
        ui: &mut egui::Ui,
        rows: &[OutputDeviceRow],
        select: &mut Option<ChainSelection>,
    ) {
        let state = self.engine.state;
        block(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(micro_label("OUTPUT DEVICES", BLUE));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.add_enabled(
                        false,
                        egui::Button::new(
                            RichText::new("+ Add output device")
                                .size(ts(9.5))
                                .color(BLUE),
                        )
                        .fill(with_alpha(BLUE, 14))
                        .stroke(Stroke::new(1.0, with_alpha(BLUE, 45)))
                        .corner_radius(CornerRadius::same(7)),
                    )
                    .on_disabled_hover_text(ADD_DEVICE_HINT);
                });
            });
            ui.add_space(6.0);
            for (index, row) in rows.iter().enumerate() {
                let selected = self.chain_selected == ChainSelection::OutputDevice(index);
                if list_device_row(ui, row, selected, state) {
                    *select = Some(ChainSelection::OutputDevice(index));
                }
            }
            ui.add_space(4.0);
            ui.label(
                RichText::new(
                    "The clock device drives the chain: plugins, gate and EQ run on its callback.",
                )
                .size(ts(10.5))
                .color(MUTED),
            );
        });
    }

    /// Inspector for the clock device: what it is, its clock, and its own
    /// level and mute after the master.
    pub(super) fn clock_device_inspector(
        &mut self,
        ui: &mut egui::Ui,
        row: &OutputDeviceRow,
        actions: &mut Vec<RackAction>,
    ) {
        ui.horizontal(|ui| {
            // Shorten the name so the CLOCK tag and MUTE always fit.
            let title_font = FontId::new(ts(16.0), FontFamily::Proportional);
            let mute_w = ui
                .painter()
                .layout_no_wrap(
                    "MUTED".to_owned(),
                    FontId::new(ts(9.0), FontFamily::Proportional),
                    TEXT,
                )
                .size()
                .x
                + ui.spacing().button_padding.x * 2.0;
            let name_w = title_name_width(
                ui.available_width(),
                tag_width(ui, "CLOCK"),
                mute_w,
                ui.spacing().item_spacing.x,
            );
            let name = fit_text(ui, &row.name, &title_font, name_w);
            ui.label(RichText::new(name).font(title_font).color(TEXT).strong())
                .on_hover_text(&row.name);
            clock_tag(ui);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if mute_button(ui, self.clock_device.mute, &row.name).clicked() {
                    self.clock_device.mute = !self.clock_device.mute;
                }
            });
        });
        ui.label(
            RichText::new("Clock device · the chain runs on this device's callback.")
                .size(ts(11.0))
                .color(DIM),
        );
        ui.add_space(12.0);

        let rate = if self.output_rate == 0 {
            "Match input".to_owned()
        } else {
            rate_label(self.output_rate)
        };
        let buffer = format!("{} smp", self.output_buffer);
        let step = match self.output_step() {
            Some((frames, ms)) => format!("{frames} smp · {ms:.1} ms"),
            None => "—".to_owned(),
        };
        block(ui, |ui| {
            ui.label(micro_label("DEVICE", BLUE));
            ui.add_space(4.0);
            ui.label(RichText::new(&row.name).size(ts(12.0)).color(TEXT));
            ui.label(mono(
                format!("{} · {} CH", row.backend.to_uppercase(), row.channels),
                DIM,
                9.5,
            ));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                small_fact(ui, "OUT RATE", &rate);
                ui.add_space(14.0);
                small_fact(ui, "BUFFER", &buffer);
                ui.add_space(14.0);
                small_fact(ui, "DRIVER STEP", &step);
            });
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new("Device, rate and buffer are set on Live Bridge.")
                        .size(ts(10.5))
                        .color(MUTED),
                );
                if ui
                    .add(
                        egui::Button::new(
                            RichText::new("Change on Live Bridge")
                                .size(ts(9.5))
                                .color(BLUE),
                        )
                        .fill(with_alpha(BLUE, 14))
                        .stroke(Stroke::new(1.0, with_alpha(BLUE, 45)))
                        .corner_radius(CornerRadius::same(7)),
                    )
                    .on_hover_text(
                        "Open Live Bridge, where the output device and its clock are picked",
                    )
                    .clicked()
                {
                    actions.push(RackAction::OpenLive);
                }
            });
        });
        ui.add_space(10.0);

        block(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(micro_label("DEVICE LEVEL", BLUE));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(mono(
                        device_level_text(&self.clock_device),
                        if self.clock_device.mute { RED } else { TEXT },
                        12.0,
                    ));
                });
            });
            ui.add_space(4.0);
            db_slider(
                ui,
                &mut self.clock_device.level_db,
                DEVICE_LEVEL_MIN_DB,
                DEVICE_LEVEL_MAX_DB,
                (BLUE, "Device level"),
            );
            ui.add_space(6.0);
            for channel in 0..row.channels {
                meter_row(
                    ui,
                    &(channel + 1).to_string(),
                    self.meter_out_channels[channel],
                    self.peak_out_channels[channel],
                    BLUE,
                );
            }
            ui.add_space(4.0);
            ui.label(
                RichText::new("Trims this device only, after the master level.")
                    .size(ts(10.5))
                    .color(MUTED),
            );
        });
        ui.add_space(10.0);

        block(ui, |ui| {
            ui.label(
                RichText::new(
                    "The clock device can't be removed. To move the clock, pick a different output on Live Bridge.",
                )
                .size(ts(10.5))
                .color(MUTED),
            );
        });
    }
}

/// A framed block in the output inspectors.
fn block(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(CARD_DEEP)
        .stroke(Stroke::new(1.0, BORDER_SOFT))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
}

const TAG_FONT: f32 = 7.5;
const TAG_PAD: Vec2 = Vec2::new(5.0, 2.0);

/// Paints a small outlined tag ("CLOCK") with its left edge centred on
/// `left_center` and returns its width.
fn paint_tag(ui: &egui::Ui, left_center: egui::Pos2, text: &str, color: Color32) -> f32 {
    let galley = ui.painter().layout_no_wrap(
        text.to_owned(),
        FontId::new(ts(TAG_FONT), FontFamily::Monospace),
        color,
    );
    let size = galley.size() + TAG_PAD * 2.0;
    let rect = egui::Rect::from_min_size(
        egui::pos2(left_center.x, left_center.y - size.y * 0.5),
        size,
    );
    ui.painter()
        .rect_filled(rect, CornerRadius::same(4), with_alpha(color, 18));
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(4),
        Stroke::new(1.0, with_alpha(color, 90)),
        egui::StrokeKind::Inside,
    );
    ui.painter().galley(rect.min + TAG_PAD, galley, color);
    size.x
}

fn tag_width(ui: &egui::Ui, text: &str) -> f32 {
    ui.painter()
        .layout_no_wrap(
            text.to_owned(),
            FontId::new(ts(TAG_FONT), FontFamily::Monospace),
            DIM,
        )
        .size()
        .x
        + TAG_PAD.x * 2.0
}

/// The CLOCK tag as a laid-out widget, for the inspector title.
fn clock_tag(ui: &mut egui::Ui) {
    let width = tag_width(ui, "CLOCK");
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 18.0), Sense::hover());
    paint_tag(ui, rect.left_center(), "CLOCK", BLUE);
    let _ = response.on_hover_text("The chain runs on this device's callback.");
}

/// Width left for the inspector title's device name after the CLOCK tag and
/// the MUTE button (and the gaps before them), never below 40 px.
fn title_name_width(available: f32, tag_w: f32, mute_w: f32, spacing: f32) -> f32 {
    (available - tag_w - mute_w - spacing * 2.0).max(40.0)
}

/// `text` shortened with "…" until it fits `max_width` in `font`.
fn fit_text(ui: &egui::Ui, text: &str, font: &FontId, max_width: f32) -> String {
    let width = |candidate: &str| {
        ui.painter()
            .layout_no_wrap(candidate.to_owned(), font.clone(), TEXT)
            .size()
            .x
    };
    if width(text) <= max_width {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    for keep in (0..chars.len()).rev() {
        let mut candidate: String = chars[..keep].iter().collect();
        candidate.push('…');
        if width(&candidate) <= max_width {
            return candidate;
        }
    }
    "…".to_owned()
}

/// The device's mute toggle, labelled like the master's.
fn mute_button(ui: &mut egui::Ui, mute: bool, device: &str) -> egui::Response {
    let response = ui
        .add(
            egui::Button::new(
                RichText::new(if mute { "MUTED" } else { "MUTE" })
                    .size(ts(9.0))
                    .color(if mute { RED } else { DIM }),
            )
            .fill(if mute {
                with_alpha(RED, 22)
            } else {
                Color32::TRANSPARENT
            })
            .stroke(Stroke::new(
                1.0,
                if mute {
                    with_alpha(RED, 55)
                } else {
                    BORDER_SOFT
                },
            ))
            .corner_radius(CornerRadius::same(6)),
        )
        .on_hover_text("Silence this device only. The master and the bridge keep running.");
    row_info(
        &response,
        egui::Role::Button,
        ui.is_enabled(),
        format!("Mute {device}"),
        Some(mute),
        None,
    );
    response
}

/// A device row under the Output stage, styled like an insert row: state
/// dot, name with its CLOCK tag, backend and channels, level and a small
/// mute toggle. Returns (row clicked, mute clicked).
fn chain_device_row(
    ui: &mut egui::Ui,
    row: &OutputDeviceRow,
    selected: bool,
    state: EngineState,
) -> (bool, bool) {
    let title_font = FontId::new(ts(11.0), FontFamily::Proportional);
    let detail_font = FontId::new(ts(8.5), FontFamily::Monospace);
    let level_font = FontId::new(ts(9.0), FontFamily::Monospace);
    let pad = Vec2::new(10.0, 6.0);
    let text_h =
        ui.fonts_mut(|fonts| fonts.row_height(&title_font) + fonts.row_height(&detail_font)) + 4.0;
    let row_h = text_h + pad.y * 2.0;
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), row_h), Sense::click());
    let border = if selected {
        with_alpha(TEAL, 90)
    } else {
        with_alpha(BLUE, 40)
    };
    ui.painter().rect_filled(
        rect,
        CornerRadius::same(8),
        if selected {
            with_alpha(TEAL, 18)
        } else if response.hovered() {
            with_alpha(TEXT, 8)
        } else {
            Color32::TRANSPARENT
        },
    );
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(8),
        Stroke::new(1.0, border),
        egui::StrokeKind::Inside,
    );
    let inner = rect.shrink2(pad);

    // Right side: the mute toggle, then the level to its left.
    let mute = row.settings.mute;
    let mute_rect = egui::Rect::from_center_size(
        egui::pos2(inner.right() - 11.0, inner.center().y),
        Vec2::new(22.0, 22.0),
    );
    // Painted at a fixed size: a Button would grow by its padding and run
    // into the level readout.
    let mute_response = ui
        .interact(mute_rect, response.id.with("mute"), Sense::click())
        .on_hover_text(if mute {
            "Unmute this device"
        } else {
            "Mute this device only"
        });
    let mute_color = if mute { RED } else { DIM };
    ui.painter().rect_filled(
        mute_rect,
        CornerRadius::same(5),
        if mute {
            with_alpha(RED, 22)
        } else if mute_response.hovered() {
            with_alpha(TEXT, 10)
        } else {
            Color32::TRANSPARENT
        },
    );
    ui.painter().rect_stroke(
        mute_rect,
        CornerRadius::same(5),
        Stroke::new(
            1.0,
            if mute {
                with_alpha(RED, 55)
            } else {
                BORDER_SOFT
            },
        ),
        egui::StrokeKind::Inside,
    );
    ui.painter().text(
        mute_rect.center(),
        Align2::CENTER_CENTER,
        "M",
        FontId::new(ts(8.5), FontFamily::Monospace),
        mute_color,
    );
    show_focus_ring(ui, &mute_response, mute_rect, 5);
    row_info(
        &mute_response,
        egui::Role::Button,
        ui.is_enabled(),
        format!("Mute {}", row.name),
        Some(mute),
        None,
    );
    let level = device_level_text(&row.settings);
    let level_galley =
        ui.painter()
            .layout_no_wrap(level, level_font, if mute { RED } else { MUTED });
    let level_right = mute_rect.left() - 8.0;
    let level_w = level_galley.size().x;
    ui.painter().galley(
        egui::pos2(
            level_right - level_w,
            inner.center().y - level_galley.size().y * 0.5,
        ),
        level_galley,
        MUTED,
    );

    // Left side: state dot, then the two text lines.
    let (_, state_color) = device_state(state);
    let dot = egui::pos2(inner.left() + 4.0, inner.center().y);
    ui.painter()
        .circle_filled(dot, 4.0, with_alpha(state_color, 60));
    ui.painter().circle_filled(dot, 2.4, state_color);
    let text_x = inner.left() + 16.0;
    let text_right = level_right - level_w - 10.0;
    let tag = if row.clock { Some("CLOCK") } else { None };
    let tag_w = tag.map_or(0.0, |text| tag_width(ui, text) + 6.0);
    let name = fit_text(
        ui,
        &row.name,
        &title_font,
        (text_right - text_x - tag_w).max(20.0),
    );
    let name_galley = ui.painter().layout_no_wrap(name, title_font, TEXT);
    let title_h = name_galley.size().y;
    let top = inner.center().y - text_h * 0.5;
    let name_w = name_galley.size().x;
    ui.painter()
        .galley(egui::pos2(text_x, top), name_galley, TEXT);
    if let Some(text) = tag {
        paint_tag(
            ui,
            egui::pos2(text_x + name_w + 6.0, top + title_h * 0.5),
            text,
            BLUE,
        );
    }
    let detail = fit_text(
        ui,
        &format!("{} · {} CH", row.backend.to_uppercase(), row.channels),
        &detail_font,
        (text_right - text_x).max(20.0),
    );
    ui.painter().text(
        egui::pos2(text_x, top + title_h + 4.0),
        Align2::LEFT_TOP,
        detail,
        detail_font,
        DIM,
    );

    show_focus_ring(ui, &response, rect, 8);
    row_info(
        &response,
        egui::Role::Button,
        ui.is_enabled(),
        row.name.clone(),
        Some(selected),
        Some(device_row_value(row, state)),
    );
    let mute_clicked = mute_response.clicked();
    (response.clicked() && !mute_clicked, mute_clicked)
}

/// A device row in the master inspector's list: state dot, name, tag, level
/// and state. Clicking it selects the device.
fn list_device_row(
    ui: &mut egui::Ui,
    row: &OutputDeviceRow,
    selected: bool,
    state: EngineState,
) -> bool {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 32.0), Sense::click());
    ui.painter().rect_filled(
        rect,
        CornerRadius::same(7),
        if selected {
            with_alpha(TEAL, 18)
        } else if response.hovered() {
            with_alpha(TEXT, 10)
        } else {
            with_alpha(TEXT, 4)
        },
    );
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(7),
        Stroke::new(
            1.0,
            if selected {
                with_alpha(TEAL, 90)
            } else {
                BORDER_SOFT
            },
        ),
        egui::StrokeKind::Inside,
    );
    let (state_text, state_color) = device_state(state);
    let center_y = rect.center().y;
    let dot = egui::pos2(rect.left() + 14.0, center_y);
    ui.painter()
        .circle_filled(dot, 4.0, with_alpha(state_color, 60));
    ui.painter().circle_filled(dot, 2.4, state_color);

    // Fixed columns from the right: state, level, tag.
    let state_x = rect.right() - 12.0;
    let level_x = state_x - 80.0;
    let tag_x = level_x - 70.0 - tag_width(ui, "CLOCK");
    ui.painter().text(
        egui::pos2(state_x, center_y),
        Align2::RIGHT_CENTER,
        state_text,
        FontId::new(ts(9.0), FontFamily::Monospace),
        state_color,
    );
    ui.painter().text(
        egui::pos2(level_x, center_y),
        Align2::RIGHT_CENTER,
        device_level_text(&row.settings),
        FontId::new(ts(9.0), FontFamily::Monospace),
        if row.settings.mute { RED } else { MUTED },
    );
    if row.clock {
        paint_tag(ui, egui::pos2(tag_x, center_y), "CLOCK", BLUE);
    }
    let name_font = FontId::new(ts(10.5), FontFamily::Proportional);
    let name_x = rect.left() + 28.0;
    let name = fit_text(ui, &row.name, &name_font, (tag_x - name_x - 10.0).max(20.0));
    ui.painter().text(
        egui::pos2(name_x, center_y),
        Align2::LEFT_CENTER,
        name,
        name_font,
        TEXT,
    );

    show_focus_ring(ui, &response, rect, 7);
    // Named apart from the same device's row in the chain list.
    row_info(
        &response,
        egui::Role::Button,
        ui.is_enabled(),
        format!("{} in output devices", row.name),
        Some(selected),
        Some(device_row_value(row, state)),
    );
    ui.add_space(2.0);
    response.clicked()
}

#[cfg(test)]
mod tests {
    use super::{
        chain_device_row, device_channel_count, device_level_text, device_row_value, device_state,
        fit_text, list_device_row, title_name_width, OutputDeviceRow,
    };
    use crate::audio::{EngineState, OutputDeviceSettings, MAX_OUTPUT_CHANNELS};
    use crate::ui::widgets::{accesskit_frame, node_named};
    use eframe::egui::{self, accesskit::Toggled};

    fn clock_row(level_db: f32, mute: bool) -> OutputDeviceRow {
        OutputDeviceRow {
            name: "AKG N9 Hybrid".to_owned(),
            backend: "PipeWire".to_owned(),
            channels: 2,
            clock: true,
            settings: OutputDeviceSettings {
                level_db,
                mute,
                ..OutputDeviceSettings::default()
            },
        }
    }

    #[test]
    fn channel_count_prefers_the_stream_then_the_scan() {
        assert_eq!(device_channel_count(6, 2), 6);
        assert_eq!(device_channel_count(0, 4), 4);
        assert_eq!(device_channel_count(0, 0), 2);
        assert_eq!(device_channel_count(1, 0), 1);
        assert_eq!(device_channel_count(32, 2), MAX_OUTPUT_CHANNELS);
        assert_eq!(device_channel_count(0, 16), MAX_OUTPUT_CHANNELS);
    }

    #[test]
    fn device_level_reads_muted_or_decibels() {
        assert_eq!(
            device_level_text(&clock_row(-1.5, false).settings),
            "-1.5 dB"
        );
        assert_eq!(
            device_level_text(&clock_row(0.0, false).settings),
            "+0.0 dB"
        );
        assert_eq!(device_level_text(&clock_row(-1.5, true).settings), "muted");
    }

    #[test]
    fn device_state_follows_the_bridge() {
        assert_eq!(device_state(EngineState::Running).0, "running");
        assert_eq!(device_state(EngineState::Idle).0, "stopped");
        assert_eq!(device_state(EngineState::Error).0, "error");
        assert_eq!(
            device_row_value(&clock_row(-6.0, false), EngineState::Running),
            "clock device · -6.0 dB · running"
        );
    }

    #[test]
    fn chain_device_row_exposes_name_selection_and_mute() {
        let row = clock_row(-1.5, true);
        let tree = accesskit_frame(|ui| {
            let (clicked, mute) = chain_device_row(ui, &row, true, EngineState::Idle);
            assert!(!clicked && !mute);
        });
        let node = node_named(&tree, "AKG N9 Hybrid");
        assert_eq!(node.role(), egui::Role::Button);
        assert_eq!(node.toggled(), Some(Toggled::True));
        assert_eq!(node.value(), Some("clock device · muted · stopped"));
        let mute = node_named(&tree, "Mute AKG N9 Hybrid");
        assert_eq!(mute.role(), egui::Role::Button);
        assert_eq!(mute.toggled(), Some(Toggled::True));
    }

    #[test]
    fn inspector_list_row_exposes_name_and_level() {
        let row = clock_row(-1.5, false);
        let tree = accesskit_frame(|ui| {
            assert!(!list_device_row(ui, &row, false, EngineState::Running));
        });
        let node = node_named(&tree, "AKG N9 Hybrid in output devices");
        assert_eq!(node.role(), egui::Role::Button);
        assert_eq!(node.toggled(), Some(Toggled::False));
        assert_eq!(node.value(), Some("clock device · -1.5 dB · running"));
    }

    #[test]
    fn the_title_name_leaves_room_for_tag_and_mute() {
        assert_eq!(title_name_width(320.0, 50.0, 60.0, 8.0), 194.0);
        assert_eq!(title_name_width(100.0, 50.0, 60.0, 8.0), 40.0);
    }

    #[test]
    fn long_names_are_shortened_to_the_width() {
        let font = egui::FontId::proportional(16.0);
        let _ = accesskit_frame(|ui| {
            assert_eq!(fit_text(ui, "Out", &font, 500.0), "Out");
            let long = "Focusrite Scarlett 18i20 3rd Gen USB Analog Surround 7.1";
            let short = fit_text(ui, long, &font, 120.0);
            assert!(short.ends_with('…'), "{short}");
            let width = ui
                .painter()
                .layout_no_wrap(short.clone(), font.clone(), egui::Color32::WHITE)
                .size()
                .x;
            assert!(width <= 120.0, "{short} is {width} px");
        });
    }
}
