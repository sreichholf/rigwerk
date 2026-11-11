//! Tuner: one note big, every string at once below it.  The analysis and the
//! frame types live in `audio::tuner`; this module only draws the frame and
//! edits the page settings.

mod gauge;
mod rows;

use eframe::egui;
use egui::{
    Align, Align2, Color32, CornerRadius, FontFamily, FontId, Layout, Margin, Rect, RichText,
    Sense, Stroke, Vec2,
};

use crate::app::tuner::{TunerDisplay, TunerListen, REFERENCE_MAX_HZ, REFERENCE_MIN_HZ};
use crate::app::RigWerkApp;
use crate::audio::{
    cents_to_ratio, note_name, octave, EngineState, TuneState, TunerChannel, TunerFrame,
    TunerTolerance, Tuning, TuningGroup, CENTER_CENTS, PRESETS,
};
use crate::ui::theme::ts;
use crate::ui::theme::{
    with_alpha, AMBER, BLUE, BORDER, BORDER_SOFT, CARD, CARD_DEEP, CARD_RAISED, DIM, MUTED, TEAL,
    TEXT,
};
use crate::ui::widgets::{
    card_fixed, micro_label, mono, pill, primary_button, progress_line, switch, tab_button,
};

/// Page content width the card split is tuned for (the 1240 px window).
const REFERENCE_PAGE_W: f32 = 964.0;
/// Settings card width at [`REFERENCE_PAGE_W`], and the floor it may shrink to.
const SETTINGS_W: f32 = 344.0;
const SETTINGS_MIN_W: f32 = 300.0;
/// The main card never narrows past this; the settings card gives way first.
const MAIN_MIN_W: f32 = 380.0;
/// Height both side-by-side cards are drawn at, so their edges line up.
const ROW_H: f32 = 396.0;

/// The settings the page was drawn with.  Copying them lets the frame stay
/// borrowed while the card collects edits for after the draw.
#[derive(Clone, Copy)]
struct View {
    preset: Option<usize>,
    reference_hz: f32,
    channel: TunerChannel,
    display: TunerDisplay,
    listen: TunerListen,
    auto_mute: bool,
    tolerance: TunerTolerance,
}

impl View {
    fn tuning(&self) -> Option<&'static Tuning> {
        self.preset.and_then(|index| PRESETS.get(index))
    }
}

/// Edits collected while drawing, applied once the frame borrow has ended.
#[derive(Default)]
struct Actions {
    preset: Option<Option<usize>>,
    reference_hz: Option<f32>,
    channel: Option<TunerChannel>,
    display: Option<TunerDisplay>,
    listen: Option<TunerListen>,
    auto_mute: Option<bool>,
    tolerance: Option<TunerTolerance>,
    start: bool,
}

/// What the big readout shows.  In All strings mode this is the lowest
/// sounding, non-ghost string rather than the mono reading, because a strummed
/// chord's mono pitch can be a subharmonic of the whole chord.
struct Lead {
    valid: bool,
    string: Option<usize>,
    midi: i16,
    cents: f32,
    target_hz: f32,
    detected_hz: f32,
    /// The analysed window's level and the reading's own clarity, so the
    /// settings meters follow the same reading the big readout shows.
    level_db: f32,
    confidence: f32,
    state: TuneState,
}

impl Lead {
    fn silent() -> Self {
        Self {
            valid: false,
            string: None,
            midi: 0,
            cents: 0.0,
            target_hz: 0.0,
            detected_hz: 0.0,
            level_db: -120.0,
            confidence: 0.0,
            state: TuneState::Silent,
        }
    }
}

impl RigWerkApp {
    pub(crate) fn tuner_page(&mut self, ui: &mut egui::Ui) {
        let total = ui.available_width();
        let gap = 10.0;
        ui.spacing_mut().item_spacing.y = 0.0;
        let settings_w = (total * SETTINGS_W / REFERENCE_PAGE_W).clamp(SETTINGS_MIN_W, SETTINGS_W);
        let main_w = (total - gap - settings_w).max(MAIN_MIN_W);

        let view = View {
            preset: self.tuner.preset,
            reference_hz: self.tuner.reference_hz,
            channel: self.tuner.channel,
            display: self.tuner.display,
            listen: self.tuner.listen,
            auto_mute: self.tuner.auto_mute,
            tolerance: self.tuner.tolerance,
        };
        // A screenshot draws the demo frame even though no bridge is open, so
        // the image shows the tuner rather than its idle state.
        let active = self.screenshot.is_some()
            || matches!(
                self.engine.state,
                EngineState::Running | EngineState::Preview
            );
        let muted = self.tuner.auto_muted(self.page, self.engine.state);
        let state = self.engine.state;
        let mut actions = Actions::default();

        {
            let frame = self.tuner.frame();
            let lead = lead_reading(frame, &view);
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                ui.allocate_ui_with_layout(
                    Vec2::new(main_w, ROW_H),
                    Layout::top_down(Align::LEFT),
                    |ui| {
                        ui.set_min_size(Vec2::new(main_w, ROW_H));
                        card_fixed(ui, CARD, BORDER, ROW_H, |ui| {
                            ui.set_width(main_w - 32.0);
                            main_card(ui, frame, &view, &lead, active, state, &mut actions);
                        });
                    },
                );
                ui.allocate_ui_with_layout(
                    Vec2::new(settings_w, ROW_H),
                    Layout::top_down(Align::LEFT),
                    |ui| {
                        ui.set_min_size(Vec2::new(settings_w, ROW_H));
                        card_fixed(ui, CARD, BORDER, ROW_H, |ui| {
                            ui.set_width(settings_w - 32.0);
                            settings_card(ui, &view, &lead, active, muted, &mut actions);
                        });
                    },
                );
            });
            ui.add_space(14.0);
            rows::all_strings_card(
                ui,
                total,
                frame,
                view.tuning(),
                view.reference_hz,
                view.tolerance.cents(),
            );
        }

        self.apply_tuner_actions(actions);
        ui.add_space(15.0);
        self.activity_strip(ui, total);
    }

    fn apply_tuner_actions(&mut self, actions: Actions) {
        if let Some(preset) = actions.preset {
            self.tuner.set_preset(preset);
        }
        if let Some(hz) = actions.reference_hz {
            self.tuner.set_reference(hz);
        }
        if let Some(channel) = actions.channel {
            self.tuner.channel = channel;
        }
        if let Some(display) = actions.display {
            self.tuner.display = display;
        }
        if let Some(listen) = actions.listen {
            if listen == TunerListen::Single || self.tuner.preset.is_some() {
                self.tuner.listen = listen;
            }
        }
        if let Some(auto_mute) = actions.auto_mute {
            self.tuner.auto_mute = auto_mute;
        }
        if let Some(tolerance) = actions.tolerance {
            self.tuner.tolerance = tolerance;
        }
        if actions.start {
            self.start_bridge();
        }
    }
}

fn main_card(
    ui: &mut egui::Ui,
    frame: &TunerFrame,
    view: &View,
    lead: &Lead,
    active: bool,
    state: EngineState,
    actions: &mut Actions,
) {
    ui.spacing_mut().item_spacing.y = 4.0;
    let tuning = view.tuning();
    ui.horizontal(|ui| {
        ui.label(micro_label("TUNER", TEAL));
        ui.add_space(2.0);
        let tag = match tuning {
            Some(tuning) => format!(
                "{} · {} STRINGS",
                tuning.name.to_uppercase(),
                tuning.strings.len()
            ),
            None => "CHROMATIC".to_owned(),
        };
        pill(ui, &tag, DIM);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            for display in [TunerDisplay::Strobe, TunerDisplay::Needle] {
                let label = match display {
                    TunerDisplay::Strobe => "Strobe",
                    TunerDisplay::Needle => "Needle",
                };
                if tab_button(ui, label, view.display == display, TEAL).clicked() {
                    actions.display = Some(display);
                }
            }
        });
    });

    ui.add_space(2.0);
    readout(ui, lead, tuning, active);
    ui.add_space(4.0);
    if active {
        match view.display {
            TunerDisplay::Strobe => gauge::strobe(
                ui,
                lead.cents,
                lead.state,
                lead.valid,
                view.tolerance.cents(),
            ),
            TunerDisplay::Needle => gauge::needle(
                ui,
                lead.cents,
                lead.state,
                lead.valid,
                view.tolerance.cents(),
            ),
        }
    } else {
        idle_panel(ui, state, actions);
    }
    ui.add_space(4.0);
    strings_section(ui, frame, tuning, lead, active);
}

fn lead_reading(frame: &TunerFrame, view: &View) -> Lead {
    if view.listen == TunerListen::All && view.preset.is_some() {
        if let Some(row) = frame
            .strings
            .iter()
            .find(|row| row.sounding && row.harmonic_of.is_none())
        {
            return Lead {
                valid: true,
                string: Some(row.string),
                midi: row.midi,
                cents: row.cents,
                target_hz: row.target_hz,
                detected_hz: row.target_hz * cents_to_ratio(row.cents),
                level_db: row.level_db,
                confidence: row.confidence,
                state: row.state,
            };
        }
        let mut lead = Lead::silent();
        lead.level_db = frame.mono.level_db;
        return lead;
    }
    let mono = &frame.mono;
    Lead {
        valid: mono.valid,
        string: mono.string,
        midi: mono.midi,
        cents: mono.cents,
        target_hz: mono.target_hz,
        detected_hz: mono.frequency,
        level_db: mono.level_db,
        confidence: mono.confidence,
        state: mono.state,
    }
}

fn readout(ui: &mut egui::Ui, lead: &Lead, tuning: Option<&'static Tuning>, active: bool) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 96.0), Sense::hover());
    let painter = ui.painter();
    let tint = |color: Color32| {
        if active {
            color
        } else {
            with_alpha(color, 70)
        }
    };
    let color = tint(state_color(lead.state));
    let note_color = tint(TEXT);
    let painter_text = |position: egui::Pos2,
                        align: Align2,
                        text: &str,
                        size: f32,
                        family: FontFamily,
                        color: Color32| {
        painter.text(
            position,
            align,
            text.to_owned(),
            FontId::new(ts(size), family),
            color,
        );
    };

    let (name, octave_text) = match note_label(lead, tuning) {
        Some((name, octave)) => (name.to_owned(), octave),
        None => ("—".to_owned(), String::new()),
    };
    let letter = painter.layout_no_wrap(
        name,
        FontId::new(ts(50.0), FontFamily::Monospace),
        note_color,
    );
    let letter_width = letter.size().x;
    painter.galley(rect.left_top(), letter, note_color);
    if lead.valid {
        painter_text(
            rect.left_top() + Vec2::new(letter_width + 2.0, 4.0),
            Align2::LEFT_TOP,
            &octave_text,
            19.0,
            FontFamily::Monospace,
            tint(MUTED),
        );
    }

    let target = if lead.valid {
        format!("{:.2} Hz", lead.target_hz)
    } else {
        "—".to_owned()
    };
    let detected = if lead.valid {
        format!("{:.2} Hz", lead.detected_hz)
    } else {
        "—".to_owned()
    };
    for (index, (label, value)) in [("TARGET", target), ("DETECTED", detected)]
        .iter()
        .enumerate()
    {
        let y = rect.top() + 62.0 + index as f32 * 16.0;
        painter_text(
            egui::pos2(rect.left(), y),
            Align2::LEFT_TOP,
            label,
            8.5,
            FontFamily::Proportional,
            tint(DIM),
        );
        painter_text(
            egui::pos2(rect.left() + 58.0, y),
            Align2::LEFT_TOP,
            value,
            9.5,
            FontFamily::Monospace,
            tint(MUTED),
        );
    }

    let cents = if lead.valid {
        signed_cents(lead.cents)
    } else {
        "—".to_owned()
    };
    painter_text(
        egui::pos2(rect.right(), rect.top() + 2.0),
        Align2::RIGHT_TOP,
        &cents,
        30.0,
        FontFamily::Monospace,
        color,
    );
    painter_text(
        egui::pos2(rect.right(), rect.top() + 44.0),
        Align2::RIGHT_TOP,
        caption(lead.state),
        8.5,
        FontFamily::Proportional,
        color,
    );
    paint_tag(
        painter,
        egui::pos2(rect.right(), rect.top() + 80.0),
        state_label(lead.state),
        color,
    );
}

/// The note as the preset spells it, flats included; chromatic falls back to
/// the sharp names.  The octave is the trailing digit of the preset name.
fn note_label(lead: &Lead, tuning: Option<&'static Tuning>) -> Option<(&'static str, String)> {
    if !lead.valid {
        return None;
    }
    if let (Some(index), Some(tuning)) = (lead.string, tuning) {
        if let Some((name, _)) = tuning.strings.get(index) {
            if let Some(split) = name.find(|character: char| character.is_ascii_digit()) {
                return Some((&name[..split], name[split..].to_owned()));
            }
        }
    }
    Some((note_name(lead.midi), octave(lead.midi).to_string()))
}

fn strings_section(
    ui: &mut egui::Ui,
    frame: &TunerFrame,
    tuning: Option<&'static Tuning>,
    lead: &Lead,
    active: bool,
) {
    ui.horizontal(|ui| {
        let title = match tuning {
            Some(tuning) => format!("STRINGS · {}", tuning.name.to_uppercase()),
            None => "STRINGS".to_owned(),
        };
        ui.label(micro_label(&title, DIM));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if tuning.is_some() {
                let (tuned, total) = tuning_count(
                    frame,
                    tuning.map(|tuning| tuning.strings.len()).unwrap_or(0),
                );
                ui.label(
                    RichText::new(format!("{tuned} / {total} in tune"))
                        .size(ts(9.0))
                        .color(TEAL),
                );
            }
        });
    });

    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 52.0), Sense::hover());
    let painter = ui.painter();
    let Some(tuning) = tuning else {
        painter.text(
            rect.left_center(),
            Align2::LEFT_CENTER,
            "Chromatic mode — pick a tuning to see each string.",
            FontId::new(ts(9.5), FontFamily::Proportional),
            DIM,
        );
        return;
    };

    let gap = 6.0;
    let count = tuning.strings.len();
    let width = ((rect.width() - gap * (count.saturating_sub(1)) as f32) / count as f32).max(36.0);
    for (index, (name, _)) in tuning.strings.iter().enumerate() {
        let left = rect.left() + index as f32 * (width + gap);
        if left + width > rect.right() + 0.5 {
            break;
        }
        let chip = Rect::from_min_size(
            egui::pos2(left, rect.top()),
            Vec2::new(width, rect.height()),
        );
        let row = frame.strings.get(index);
        let ghost = row.is_some_and(|row| row.harmonic_of.is_some());
        let state = row.map(|row| row.state).unwrap_or(TuneState::Silent);
        let color = if ghost { DIM } else { state_color(state) };
        let selected = lead.string == Some(index);
        painter.rect_filled(
            chip,
            CornerRadius::same(9),
            if selected { CARD_RAISED } else { CARD_DEEP },
        );
        painter.rect_stroke(
            chip,
            CornerRadius::same(9),
            Stroke::new(
                if selected { 1.4 } else { 1.0 },
                if selected {
                    color
                } else if ghost {
                    BORDER_SOFT
                } else {
                    with_alpha(color, 55)
                },
            ),
            egui::StrokeKind::Inside,
        );
        painter.text(
            egui::pos2(chip.center().x, chip.top() + 9.0),
            Align2::CENTER_TOP,
            *name,
            FontId::new(ts(11.0), FontFamily::Monospace),
            if !active {
                with_alpha(TEXT, 90)
            } else if ghost {
                DIM
            } else {
                TEXT
            },
        );
        let cents = row
            .filter(|row| row.sounding)
            .map(|row| signed_cents(row.cents))
            .unwrap_or_else(|| "—".to_owned());
        painter.text(
            egui::pos2(chip.center().x, chip.top() + 32.0),
            Align2::CENTER_TOP,
            cents,
            FontId::new(ts(9.0), FontFamily::Monospace),
            if row.is_some_and(|row| row.sounding) {
                color
            } else {
                DIM
            },
        );
    }
}

fn idle_panel(ui: &mut egui::Ui, state: EngineState, actions: &mut Actions) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 146.0), Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::same(10), CARD_DEEP);
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(10),
        Stroke::new(1.0, BORDER_SOFT),
        egui::StrokeKind::Inside,
    );
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(Layout::top_down(Align::Center)),
    );
    child.set_clip_rect(rect);
    let (label, color) = match state {
        EngineState::Starting => ("Linking…", BLUE),
        EngineState::Stopping => ("Stopping…", AMBER),
        _ => ("Start the bridge to tune", MUTED),
    };
    let can_start = matches!(state, EngineState::Idle | EngineState::Error);
    child.add_space(if can_start { 34.0 } else { 58.0 });
    child.label(RichText::new(label).size(ts(12.0)).color(color));
    if can_start {
        child.add_space(8.0);
        if primary_button(&mut child, "Start bridge", TEAL, 140.0) {
            actions.start = true;
        }
    }
}

fn settings_card(
    ui: &mut egui::Ui,
    view: &View,
    lead: &Lead,
    active: bool,
    muted: bool,
    actions: &mut Actions,
) {
    ui.spacing_mut().item_spacing.y = 1.0;
    ui.horizontal(|ui| {
        ui.label(micro_label("SETTINGS", DIM));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(mono(format!("A4 {:.0}", view.reference_hz), DIM, 8.0));
        });
    });

    ui.label(micro_label("TUNING", DIM));
    let selected = match view.tuning() {
        Some(tuning) => format!("{} · {}", group_prefix(tuning.group), tuning.name),
        None => "Chromatic · any note".to_owned(),
    };
    egui::Frame::new()
        .fill(CARD_DEEP)
        .stroke(Stroke::new(1.0, BORDER_SOFT))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(9, 2))
        .show(ui, |ui| {
            egui::ComboBox::from_id_salt("tuner_tuning")
                .width(ui.available_width())
                .height(360.0)
                .selected_text(RichText::new(selected).size(ts(11.5)).color(TEXT))
                .show_ui(ui, |ui| {
                    ui.set_min_width(280.0);
                    if ui
                        .selectable_label(view.preset.is_none(), "Chromatic · any note")
                        .clicked()
                    {
                        actions.preset = Some(None);
                    }
                    for group in TuningGroup::ALL {
                        ui.add_space(3.0);
                        ui.label(micro_label(group.label(), DIM));
                        for (index, tuning) in PRESETS
                            .iter()
                            .enumerate()
                            .filter(|(_, item)| item.group == group)
                        {
                            let label = format!("{} · {}", tuning.name, tuning.strings[0].0);
                            if ui
                                .selectable_label(view.preset == Some(index), label)
                                .clicked()
                            {
                                actions.preset = Some(Some(index));
                            }
                        }
                    }
                });
        });

    ui.label(micro_label("LISTEN", DIM));
    let tab_width = (ui.available_width() - 6.0) / 2.0;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        if wide_tab(
            ui,
            "Single note",
            view.listen == TunerListen::Single,
            TEAL,
            tab_width,
        ) {
            actions.listen = Some(TunerListen::Single);
        }
        ui.add_enabled_ui(view.preset.is_some(), |ui| {
            if wide_tab(
                ui,
                "All strings",
                view.listen == TunerListen::All,
                TEAL,
                tab_width,
            ) {
                actions.listen = Some(TunerListen::All);
            }
        });
    });

    ui.horizontal(|ui| {
        ui.label(micro_label("TOLERANCE", DIM));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(
                RichText::new(format!(
                    "Green band {} · center ±{:.0}",
                    view.tolerance.label(),
                    CENTER_CENTS
                ))
                .size(ts(8.5))
                .color(DIM),
            );
        });
    });
    let tab_width = (ui.available_width() - 12.0) / 3.0;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        for tolerance in TunerTolerance::ALL {
            if wide_tab(
                ui,
                tolerance.label(),
                view.tolerance == tolerance,
                TEAL,
                tab_width,
            ) {
                actions.tolerance = Some(tolerance);
            }
        }
    });

    ui.label(micro_label("REFERENCE", DIM));
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        if step_button(ui, "−") {
            actions.reference_hz =
                Some((view.reference_hz - 1.0).clamp(REFERENCE_MIN_HZ, REFERENCE_MAX_HZ));
        }
        ui.label(mono(format!("{:.1}", view.reference_hz), TEXT, 14.0));
        ui.label(micro_label("Hz", DIM));
        if step_button(ui, "+") {
            actions.reference_hz =
                Some((view.reference_hz + 1.0).clamp(REFERENCE_MIN_HZ, REFERENCE_MAX_HZ));
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(mono("A4", DIM, 8.0));
        });
    });

    ui.label(micro_label("INPUT", DIM));
    let channel_count = TunerChannel::ALL.len() as f32;
    let tab_width = (ui.available_width() - 6.0 * (channel_count - 1.0)) / channel_count;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        for channel in [TunerChannel::Left, TunerChannel::Right, TunerChannel::Sum] {
            if wide_tab(
                ui,
                channel.label(),
                view.channel == channel,
                TEAL,
                tab_width,
            ) {
                actions.channel = Some(channel);
            }
        }
    });

    ui.add_space(2.0);
    ui.horizontal(|ui| {
        ui.label(
            RichText::new("Auto-mute while tuning")
                .size(ts(10.0))
                .color(TEXT),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let mut on = view.auto_mute;
            if switch(ui, &mut on, TEAL) {
                actions.auto_mute = Some(on);
            }
        });
    });
    if muted {
        ui.horizontal(|ui| {
            muted_speaker(ui);
            ui.label(
                RichText::new("Output muted by tuner")
                    .size(ts(9.5))
                    .color(AMBER),
            );
        });
    }

    let level = lead.level_db;
    let (level_text, level_fraction) = if active && level.is_finite() {
        (
            format!("{level:.1} dBFS"),
            ((level + 60.0) / 60.0).clamp(0.0, 1.0),
        )
    } else {
        ("—".to_owned(), 0.0)
    };
    ui.add_space(2.0);
    progress_line(ui, "LEVEL", &level_text, level_fraction, BLUE);
    let confidence = if active {
        lead.confidence.clamp(0.0, 1.0)
    } else {
        0.0
    };
    progress_line(
        ui,
        "CONFIDENCE",
        &format!("{confidence:.2}"),
        confidence,
        TEAL,
    );
}

fn wide_tab(ui: &mut egui::Ui, label: &str, selected: bool, color: Color32, width: f32) -> bool {
    ui.add_sized(
        [width, 22.0],
        egui::Button::new(
            RichText::new(label)
                .size(ts(8.8))
                .color(if selected { color } else { DIM })
                .strong(),
        )
        .fill(if selected {
            with_alpha(color, 20)
        } else {
            Color32::TRANSPARENT
        })
        .stroke(Stroke::new(
            1.0,
            if selected {
                with_alpha(color, 60)
            } else {
                BORDER_SOFT
            },
        ))
        .corner_radius(CornerRadius::same(5)),
    )
    .clicked()
}

fn step_button(ui: &mut egui::Ui, label: &str) -> bool {
    ui.add_sized(
        [24.0, 24.0],
        egui::Button::new(RichText::new(label).size(ts(12.0)).color(TEXT))
            .fill(CARD_DEEP)
            .stroke(Stroke::new(1.0, BORDER_SOFT))
            .corner_radius(CornerRadius::same(6)),
    )
    .clicked()
}

fn muted_speaker(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
    let painter = ui.painter();
    let center = rect.center();
    painter.add(egui::Shape::convex_polygon(
        vec![
            egui::pos2(center.x - 4.5, center.y - 2.0),
            egui::pos2(center.x - 1.5, center.y - 2.0),
            egui::pos2(center.x + 1.5, center.y - 5.0),
            egui::pos2(center.x + 1.5, center.y + 5.0),
            egui::pos2(center.x - 1.5, center.y + 2.0),
            egui::pos2(center.x - 4.5, center.y + 2.0),
        ],
        AMBER,
        Stroke::NONE,
    ));
    painter.line_segment(
        [
            egui::pos2(center.x + 3.0, center.y - 3.0),
            egui::pos2(center.x + 6.0, center.y + 3.0),
        ],
        Stroke::new(1.2, AMBER),
    );
    painter.line_segment(
        [
            egui::pos2(center.x + 6.0, center.y - 3.0),
            egui::pos2(center.x + 3.0, center.y + 3.0),
        ],
        Stroke::new(1.2, AMBER),
    );
}

fn tuning_count(frame: &TunerFrame, total: usize) -> (usize, usize) {
    let tuned = frame
        .strings
        .iter()
        .filter(|row| {
            row.sounding
                && row.harmonic_of.is_none()
                && matches!(row.state, TuneState::Centered | TuneState::InTune)
        })
        .count();
    (tuned, total)
}

pub(super) fn state_color(state: TuneState) -> Color32 {
    match state {
        TuneState::Centered | TuneState::InTune => TEAL,
        TuneState::Flat => AMBER,
        TuneState::Sharp => BLUE,
        TuneState::Silent => DIM,
    }
}

pub(super) fn state_label(state: TuneState) -> &'static str {
    match state {
        TuneState::Centered => "Centered",
        TuneState::InTune => "In tune",
        TuneState::Flat => "Tune up",
        TuneState::Sharp => "Tune down",
        TuneState::Silent => "Listening",
    }
}

fn caption(state: TuneState) -> &'static str {
    match state {
        TuneState::Centered => "CENTS · CENTERED",
        TuneState::InTune => "CENTS · IN TUNE",
        TuneState::Flat => "CENTS · FLAT",
        TuneState::Sharp => "CENTS · SHARP",
        TuneState::Silent => "CENTS",
    }
}

fn group_prefix(group: TuningGroup) -> &'static str {
    match group {
        TuningGroup::Guitar6 => "Guitar",
        TuningGroup::Extended => "Extended",
        TuningGroup::Bass => "Bass",
    }
}

/// Absolute cents the readout prints as 0.0.  The strobe holds still in
/// the same band, so the pattern and the number agree on "centered".
const DISPLAYED_ZERO_CENTS: f32 = 0.05;

pub(super) fn signed_cents(cents: f32) -> String {
    if !cents.is_finite() {
        return "—".to_owned();
    }
    if cents.abs() < DISPLAYED_ZERO_CENTS {
        "0.0".to_owned()
    } else if cents > 0.0 {
        format!("+{cents:.1}")
    } else {
        format!("−{:.1}", cents.abs())
    }
}

/// A right-aligned pill, for the painted readout and string rows.
pub(super) fn paint_tag(
    painter: &egui::Painter,
    right_center: egui::Pos2,
    text: &str,
    color: Color32,
) {
    let galley = painter.layout_no_wrap(
        text.to_owned(),
        FontId::new(ts(9.0), FontFamily::Proportional),
        color,
    );
    let size = Vec2::new(galley.size().x + 18.0, 22.0);
    let rect = Rect::from_min_size(
        egui::pos2(right_center.x - size.x, right_center.y - size.y / 2.0),
        size,
    );
    painter.rect_filled(rect, CornerRadius::same(11), with_alpha(color, 22));
    painter.rect_stroke(
        rect,
        CornerRadius::same(11),
        Stroke::new(1.0, with_alpha(color, 60)),
        egui::StrokeKind::Inside,
    );
    painter.galley(rect.center() - galley.size() / 2.0, galley, color);
}

#[cfg(test)]
mod tests;
