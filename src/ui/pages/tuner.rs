//! Tuner: one screen (`docs/ui-spec.md` §3.4).  The note very large, the
//! cents and the state, the needle or the three strobe discs, the string
//! cells, and one bottom bar with every setting.  The title bar and the
//! status bar stay, so the tuner is entered and left like any other screen.
//!
//! The analysis and the frame types live in `audio::tuner`; this module only
//! draws the frame and edits the screen's settings.  While auto-mute holds
//! the outputs, a strip under the title bar says so.

mod gauge;

use eframe::egui;
use egui::{Align, Align2, Color32, CornerRadius, Layout, Rect, RichText, Sense, Stroke, Vec2};

use crate::app::tuner::{
    StringFace, TunerDisplay, TunerListen, REFERENCE_MAX_HZ, REFERENCE_MIN_HZ,
};
use crate::app::RigWerkApp;
use crate::audio::{
    cents_to_ratio, note_name, octave, EngineState, TuneState, TunerChannel, TunerFrame,
    TunerTolerance, Tuning, TuningGroup, TUNINGS,
};
use crate::ui::fonts::Face;
use crate::ui::theme::{font, palette, Palette, TypeRole};
use crate::ui::widgets::{chip, row_info, segmented, OutlineButton};

/// The settings the page was drawn with.  Copying them lets the frame stay
/// borrowed while the bar collects edits for after the draw.
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
        self.preset.and_then(|index| TUNINGS.get(index))
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
    /// A string cell the user tapped.  Applied once the frame borrow ends.
    pin: Option<usize>,
    start: bool,
}

/// What the big readout shows.  In All strings mode this is the string the
/// tuner is following, or the one the user pinned.  The mono reading stays
/// off that face: a strummed chord's pitch can be a subharmonic of the chord.
struct Lead {
    valid: bool,
    string: Option<usize>,
    /// The face string is a pin.  Single-note listen leaves this false.
    pinned: bool,
    /// The All-strings follow target, even when a pin covers the face.
    /// `None` in single-note listen.
    following: Option<usize>,
    midi: i16,
    cents: f32,
    target_hz: f32,
    detected_hz: f32,
    /// The analysed window's level and the reading's own clarity, so the
    /// meters follow the same reading the big readout shows.
    level_db: f32,
    confidence: f32,
    state: TuneState,
}

impl Lead {
    fn silent() -> Self {
        Self {
            valid: false,
            string: None,
            pinned: false,
            following: None,
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
        let colours = palette(ui.ctx());
        let view = View {
            preset: self.tuner.tuning,
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

        if muted {
            muted_strip(ui, colours);
            ui.add_space(6.0);
        }

        {
            // Decide the face before borrowing the frame.  `follow_face`
            // writes the remembered string, and `frame` holds that borrow.
            let face = if view.listen == TunerListen::All && view.preset.is_some() {
                self.tuner.follow_face()
            } else {
                StringFace::default()
            };
            let frame = self.tuner.frame();
            let lead = lead_reading(frame, &view, face);
            readout(ui, &lead, view.tuning(), active, colours);
            ui.add_space(4.0);
            if active {
                match view.display {
                    TunerDisplay::Strobe => {
                        gauge::strobe(ui, lead.cents, lead.state, lead.valid, colours)
                    }
                    TunerDisplay::Needle => gauge::needle(
                        ui,
                        lead.cents,
                        lead.state,
                        lead.valid,
                        view.tolerance.cents(),
                        colours,
                    ),
                }
            } else {
                idle_panel(ui, state, &mut actions, colours);
            }
            ui.add_space(6.0);
            level_and_confidence(ui, &lead, active, colours);
            ui.add_space(8.0);
            strings_section(ui, frame, &view, &lead, active, &mut actions, colours);
        }

        ui.add_space(10.0);
        bottom_bar(ui, &view, active, &mut actions, colours);
        self.apply_tuner_actions(actions);
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
            if listen == TunerListen::Single || self.tuner.tuning.is_some() {
                self.tuner.listen = listen;
            }
        }
        if let Some(auto_mute) = actions.auto_mute {
            self.tuner.auto_mute = auto_mute;
        }
        if let Some(tolerance) = actions.tolerance {
            self.tuner.tolerance = tolerance;
        }
        if let Some(index) = actions.pin {
            self.tuner.toggle_pin(index);
        }
        if actions.start {
            self.start_bridge();
        }
    }
}

/// The strip under the title bar while auto-mute holds the outputs.
fn muted_strip(ui: &mut egui::Ui, colours: &Palette) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 30.0), Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::same(4), colours.accent_bg);
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(4),
        Stroke::new(1.0, colours.accent),
        egui::StrokeKind::Inside,
    );
    ui.painter().text(
        egui::pos2(rect.left() + 12.0, rect.center().y),
        Align2::LEFT_CENTER,
        "Outputs and the metronome are muted while you tune",
        font(Face::SansMedium, TypeRole::Secondary),
        colours.accent_text,
    );
}

/// The note, its octave, the target and heard frequencies, the cents and
/// the state.
fn readout(
    ui: &mut egui::Ui,
    lead: &Lead,
    tuning: Option<&'static Tuning>,
    active: bool,
    colours: &Palette,
) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 150.0), Sense::hover());
    let painter = ui.painter();
    let tint = |colour: Color32| {
        if active {
            colour
        } else {
            colour.gamma_multiply(0.55)
        }
    };
    let colour = tint(state_colour(colours, lead.state, lead.cents));
    let (name, octave_text) = match note_label(lead, tuning) {
        Some((name, octave)) => (name.to_owned(), octave),
        None => ("—".to_owned(), String::new()),
    };
    let letter = painter.layout_no_wrap(
        name,
        egui::FontId::new(104.0, Face::MonoMedium.family()),
        tint(colours.text),
    );
    let letter_width = letter.size().x;
    painter.galley(rect.left_top(), letter, tint(colours.text));
    if lead.valid {
        painter.text(
            rect.left_top() + Vec2::new(letter_width + 4.0, 18.0),
            Align2::LEFT_TOP,
            octave_text,
            font(Face::Mono, TypeRole::TunerString),
            tint(colours.text3),
        );
    }

    let target = if lead.valid {
        format!("{:.2} Hz", lead.target_hz)
    } else {
        "—".to_owned()
    };
    let heard = if lead.valid {
        format!("{:.2} Hz", lead.detected_hz)
    } else {
        "—".to_owned()
    };
    for (index, (label, value)) in [("Target", target), ("Heard", heard)].iter().enumerate() {
        let y = rect.top() + 108.0 + index as f32 * 18.0;
        painter.text(
            egui::pos2(rect.left(), y),
            Align2::LEFT_TOP,
            *label,
            font(Face::Sans, TypeRole::GridHeader),
            tint(colours.text3),
        );
        painter.text(
            egui::pos2(rect.left() + 52.0, y),
            Align2::LEFT_TOP,
            value,
            font(Face::Mono, TypeRole::Secondary),
            tint(colours.text2),
        );
    }

    let cents = if lead.valid {
        signed_cents(lead.cents)
    } else {
        "—".to_owned()
    };
    painter.text(
        egui::pos2(rect.right(), rect.top()),
        Align2::RIGHT_TOP,
        cents,
        font(Face::Mono, TypeRole::TunerCents),
        colour,
    );
    painter.text(
        egui::pos2(rect.right(), rect.top() + 56.0),
        Align2::RIGHT_TOP,
        state_text(lead.state),
        font(Face::SansMedium, TypeRole::Title),
        colour,
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

fn lead_reading(frame: &TunerFrame, view: &View, face: StringFace) -> Lead {
    if view.listen == TunerListen::All && view.preset.is_some() {
        let mut lead = if let Some(index) = face.string {
            match frame.strings.iter().find(|row| row.string == index) {
                Some(row) => {
                    let playable = row.sounding && row.harmonic_of.is_none();
                    Lead {
                        valid: playable,
                        string: Some(row.string),
                        pinned: face.pinned,
                        following: None,
                        midi: row.midi,
                        cents: if playable { row.cents } else { 0.0 },
                        target_hz: row.target_hz,
                        detected_hz: if playable {
                            row.target_hz * cents_to_ratio(row.cents)
                        } else {
                            0.0
                        },
                        level_db: row.level_db,
                        confidence: if playable { row.confidence } else { 0.0 },
                        state: if playable {
                            row.state
                        } else {
                            TuneState::Silent
                        },
                    }
                }
                None => {
                    let mut lead = Lead::silent();
                    lead.string = Some(index);
                    lead.pinned = face.pinned;
                    lead.level_db = frame.mono.level_db;
                    lead
                }
            }
        } else {
            let mut lead = Lead::silent();
            lead.level_db = frame.mono.level_db;
            lead
        };
        lead.following = face.following;
        return lead;
    }
    let mono = &frame.mono;
    Lead {
        valid: mono.valid,
        string: mono.string,
        pinned: false,
        following: None,
        midi: mono.midi,
        cents: mono.cents,
        target_hz: mono.target_hz,
        detected_hz: mono.frequency,
        level_db: mono.level_db,
        confidence: mono.confidence,
        state: mono.state,
    }
}

/// Widest level the readout column holds: a signed `xxx.x` plus the unit.
/// `100.3` and `99.2` then share one right edge.  The row stays the width
/// of the page instead of pushing the string cells.
const METER_VALUE_SAMPLE: &str = "-100.0 dBFS";

/// Name column shared by the level and confidence rows.
const METER_NAME_WIDTH: f32 = 88.0;

fn level_readout(level_db: f32) -> String {
    format!("{level_db:.1} dBFS")
}

/// The level and the confidence as two thin bars under the scale.
fn level_and_confidence(ui: &mut egui::Ui, lead: &Lead, active: bool, colours: &Palette) {
    let level = if active && lead.level_db.is_finite() {
        ((lead.level_db + 60.0) / 60.0).clamp(0.0, 1.0)
    } else {
        0.0
    };
    bar(
        ui,
        "Level",
        &if active && lead.level_db.is_finite() {
            level_readout(lead.level_db)
        } else {
            "—".to_owned()
        },
        level,
        colours.text2,
        colours,
    );
    let confidence = if active {
        lead.confidence.clamp(0.0, 1.0)
    } else {
        0.0
    };
    bar(
        ui,
        "Confidence",
        &format!("{confidence:.2}"),
        confidence,
        colours.text2,
        colours,
    );
}

fn bar(
    ui: &mut egui::Ui,
    name: &str,
    value: &str,
    fraction: f32,
    colour: egui::Color32,
    colours: &Palette,
) {
    let value_font = font(Face::Mono, TypeRole::GridHeader);
    let value_width = ui
        .painter()
        .layout_no_wrap(
            METER_VALUE_SAMPLE.to_owned(),
            value_font.clone(),
            colours.text2,
        )
        .size()
        .x
        .ceil();
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        let (name_rect, _) =
            ui.allocate_exact_size(Vec2::new(METER_NAME_WIDTH, 16.0), Sense::hover());
        ui.painter().text(
            name_rect.left_center(),
            Align2::LEFT_CENTER,
            name,
            font(Face::Sans, TypeRole::GridHeader),
            colours.text3,
        );
        // The value keeps a fixed column.  A label sized to the digits
        // would grow the row past the page, and the string cells follow
        // that width.
        let gap = ui.spacing().item_spacing.x;
        let room = ui.available_width();
        let value_width = value_width.min((room - gap).max(0.0));
        let bar_width = (room - gap - value_width).max(0.0);
        let (rect, response) = ui.allocate_exact_size(Vec2::new(bar_width, 6.0), Sense::hover());
        ui.painter()
            .rect_filled(rect, CornerRadius::same(2), colours.well);
        if fraction > 0.0 {
            let filled =
                Rect::from_min_size(rect.min, Vec2::new(rect.width() * fraction, rect.height()));
            ui.painter()
                .rect_filled(filled, CornerRadius::same(2), colour);
        }
        row_info(
            &response,
            egui::Role::Meter,
            true,
            name,
            None,
            Some(value.to_owned()),
        );
        let (value_rect, _) = ui.allocate_exact_size(Vec2::new(value_width, 16.0), Sense::hover());
        ui.painter().text(
            value_rect.right_center(),
            Align2::RIGHT_CENTER,
            value,
            value_font,
            colours.text2,
        );
    });
}

fn strings_section(
    ui: &mut egui::Ui,
    frame: &TunerFrame,
    view: &View,
    lead: &Lead,
    active: bool,
    actions: &mut Actions,
    colours: &Palette,
) {
    let tuning = view.tuning();
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(match tuning {
                Some(tuning) => format!("Strings · {}", tuning.name),
                None => "Strings".to_owned(),
            })
            .font(font(Face::SansMedium, TypeRole::Secondary))
            .color(colours.text3),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if tuning.is_some() {
                let (tuned, total) = tuning_count(
                    frame,
                    tuning.map(|tuning| tuning.strings.len()).unwrap_or(0),
                );
                ui.label(
                    RichText::new(format!("{tuned} / {total} in tune"))
                        .font(font(Face::Mono, TypeRole::Secondary))
                        .color(colours.path[2]),
                );
            }
        });
    });
    ui.add_space(4.0);

    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 104.0), Sense::hover());
    let Some(tuning) = tuning else {
        ui.painter().text(
            rect.left_center(),
            Align2::LEFT_CENTER,
            "Pick a tuning to see each string",
            font(Face::Sans, TypeRole::Body),
            colours.text3,
        );
        return;
    };

    let gap = 6.0;
    let count = tuning.strings.len();
    let width = ((rect.width() - gap * count.saturating_sub(1) as f32) / count as f32).max(36.0);
    for (index, (name, _)) in tuning.strings.iter().enumerate() {
        let left = rect.left() + index as f32 * (width + gap);
        if left + width > rect.right() + 0.5 {
            break;
        }
        let cell = Rect::from_min_size(
            egui::pos2(left, rect.top()),
            Vec2::new(width, rect.height()),
        );
        let row = frame.strings.get(index);
        let ghost = row.is_some_and(|row| row.harmonic_of.is_some());
        let state = row.map(|row| row.state).unwrap_or(TuneState::Silent);
        let cents_value = row.map(|row| row.cents).unwrap_or(0.0);
        let colour = if ghost {
            colours.text3
        } else {
            state_colour(colours, state, cents_value)
        };
        let pinned = lead.pinned && lead.string == Some(index);
        // All strings marks the follow target while a different pin owns the
        // face.  Single note marks the mono match, and never a pin.
        let followed = if view.listen == TunerListen::All {
            lead.following == Some(index) && !pinned
        } else {
            lead.string == Some(index)
        };
        let response = ui.interact(
            cell,
            ui.scope_id().with(("tuner_string", index)),
            if ghost {
                Sense::hover()
            } else {
                Sense::click()
            },
        );
        if response.clicked() {
            actions.pin = Some(index);
        }
        let painter = ui.painter();
        painter.rect_filled(cell, CornerRadius::same(4), colours.slot);
        painter.rect_stroke(
            cell,
            CornerRadius::same(4),
            Stroke::new(
                if pinned { 1.5 } else { 1.0 },
                if pinned {
                    colours.accent
                } else {
                    colours.line2
                },
            ),
            egui::StrokeKind::Inside,
        );
        if followed {
            painter.hline(
                (cell.left() + 6.0)..=(cell.right() - 6.0),
                cell.bottom() - 4.0,
                Stroke::new(2.0, colour),
            );
        }
        painter.text(
            egui::pos2(cell.center().x, cell.top() + 4.0),
            Align2::CENTER_TOP,
            row.map(|row| row.name).unwrap_or(*name),
            font(Face::Mono, TypeRole::TunerString),
            if !active || ghost {
                colours.text3
            } else {
                colours.text
            },
        );
        let cents = row
            .filter(|row| row.sounding)
            .map(|row| signed_cents(row.cents))
            .unwrap_or_else(|| "—".to_owned());
        painter.text(
            egui::pos2(cell.center().x, cell.top() + 30.0),
            Align2::CENTER_TOP,
            match row {
                Some(row) => format!("{:.1} Hz", row.target_hz),
                None => "—".to_owned(),
            },
            font(Face::Mono, TypeRole::GridHeader),
            colours.text3,
        );
        painter.text(
            egui::pos2(cell.center().x, cell.top() + 44.0),
            Align2::CENTER_TOP,
            cents,
            font(Face::Mono, TypeRole::GridHeader),
            if row.is_some_and(|row| row.sounding) {
                colour
            } else {
                colours.text3
            },
        );
        let mark = row
            .filter(|row| row.sounding && row.harmonic_of.is_none())
            .map(|row| row.cents);
        string_track(painter, cell, mark, view.tolerance.cents(), colour, colours);
        painter.text(
            egui::pos2(cell.center().x, cell.top() + 78.0),
            Align2::CENTER_TOP,
            if ghost {
                "Harmonic".to_owned()
            } else if row.is_some() {
                state_text(state).to_owned()
            } else {
                "—".to_owned()
            },
            font(Face::Sans, TypeRole::GridHeader),
            colour,
        );
        if ghost {
            painter.text(
                egui::pos2(cell.right() - 6.0, cell.top() + 4.0),
                Align2::RIGHT_TOP,
                "H",
                font(Face::SansMedium, TypeRole::GridHeader),
                colours.text3,
            );
        }
    }
}

/// The per-string track: a centre tick, the tolerance band across ±50 cents,
/// and a 3 px mark when the string is sounding and not a ghost.
fn string_track(
    painter: &egui::Painter,
    cell: Rect,
    cents: Option<f32>,
    tolerance: f32,
    mark: Color32,
    colours: &Palette,
) {
    let track = Rect::from_min_max(
        egui::pos2(cell.left() + 8.0, cell.top() + 58.0),
        egui::pos2(cell.right() - 8.0, cell.top() + 68.0),
    );
    if track.width() < 4.0 {
        return;
    }
    painter.rect_filled(track, CornerRadius::same(2), colours.well);
    let mid = track.center().x;
    let half = track.width() * 0.5;
    let band = half * (tolerance / 50.0).clamp(0.0, 1.0);
    painter.rect_filled(
        Rect::from_min_max(
            egui::pos2(mid - band, track.top()),
            egui::pos2(mid + band, track.bottom()),
        ),
        CornerRadius::same(2),
        colours.path[2],
    );
    painter.vline(
        mid,
        (track.top() - 2.0)..=(track.bottom() + 2.0),
        Stroke::new(1.0, colours.text),
    );
    if let Some(cents) = cents {
        let x = mid + mark_fraction(cents) * half;
        painter.rect_filled(
            Rect::from_center_size(egui::pos2(x, track.center().y), Vec2::new(3.0, 14.0)),
            0,
            mark,
        );
    }
}

fn idle_panel(ui: &mut egui::Ui, state: EngineState, actions: &mut Actions, colours: &Palette) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 150.0), Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::same(4), colours.strip);
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(4),
        Stroke::new(1.0, colours.line),
        egui::StrokeKind::Inside,
    );
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(Layout::top_down(Align::Center)),
    );
    child.set_clip_rect(rect);
    let (label, colour) = match state {
        EngineState::Starting => ("Starting…", colours.accent_text),
        EngineState::Stopping => ("Stopping…", colours.warn),
        _ => ("Start the bridge to tune", colours.text2),
    };
    let can_start = matches!(state, EngineState::Idle | EngineState::Error);
    child.add_space(if can_start { 38.0 } else { 62.0 });
    child.label(
        RichText::new(label)
            .font(font(Face::SansMedium, TypeRole::Title))
            .color(colour),
    );
    if can_start {
        child.add_space(8.0);
        let response = child.add(
            egui::Button::new(
                RichText::new("Start")
                    .font(font(Face::SansMedium, TypeRole::Control))
                    .color(colours.bar),
            )
            .fill(colours.accent)
            .stroke(Stroke::NONE)
            .corner_radius(CornerRadius::same(4))
            .min_size(Vec2::new(120.0, 32.0)),
        );
        if response.clicked() {
            actions.start = true;
        }
    }
}

/// The bottom bar: tuning, A4, the in-tune band, the input, what to listen
/// to, auto-mute, and the display.
fn bottom_bar(
    ui: &mut egui::Ui,
    view: &View,
    active: bool,
    actions: &mut Actions,
    colours: &Palette,
) {
    egui::Frame::new()
        .fill(colours.strip)
        .stroke(Stroke::new(1.0, colours.line))
        .corner_radius(CornerRadius::same(4))
        .inner_margin(egui::Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 16.0;
            ui.horizontal_wrapped(|ui| {
                group(ui, "Tuning", colours, |ui| {
                    tuning_picker(ui, view, actions, colours);
                });
                group(ui, "A4", colours, |ui| {
                    let step = |ui: &mut egui::Ui, label: &str| {
                        let name = if label == "−" {
                            "Lower A4"
                        } else {
                            "Raise A4"
                        };
                        OutlineButton::new(label).name(name).show(ui).clicked()
                    };
                    if step(ui, "−") {
                        actions.reference_hz = Some(
                            (view.reference_hz - 1.0).clamp(REFERENCE_MIN_HZ, REFERENCE_MAX_HZ),
                        );
                    }
                    ui.label(
                        RichText::new(format!("{:.0}", view.reference_hz))
                            .font(font(Face::MonoMedium, TypeRole::Control))
                            .color(colours.text),
                    );
                    if step(ui, "+") {
                        actions.reference_hz = Some(
                            (view.reference_hz + 1.0).clamp(REFERENCE_MIN_HZ, REFERENCE_MAX_HZ),
                        );
                    }
                });
                group(ui, "In tune within", colours, |ui| {
                    let mut choice = TunerTolerance::ALL
                        .iter()
                        .position(|tolerance| *tolerance == view.tolerance)
                        .unwrap_or(1);
                    let labels: Vec<&str> = TunerTolerance::ALL
                        .iter()
                        .map(|tolerance| tolerance.label())
                        .collect();
                    if segmented(ui, &mut choice, &labels).changed() {
                        actions.tolerance = TunerTolerance::ALL.get(choice).copied();
                    }
                });
                group(ui, "Input", colours, |ui| {
                    let mut choice = TunerChannel::ALL
                        .iter()
                        .position(|channel| *channel == view.channel)
                        .unwrap_or(2);
                    let labels: Vec<&str> = TunerChannel::ALL
                        .iter()
                        .map(|channel| channel.label())
                        .collect();
                    if segmented(ui, &mut choice, &labels).changed() {
                        actions.channel = TunerChannel::ALL.get(choice).copied();
                    }
                });
                group(ui, "Listen", colours, |ui| {
                    let mut single = view.listen == TunerListen::Single;
                    if chip(ui, &mut single, "Single note").changed() {
                        actions.listen = Some(TunerListen::Single);
                    }
                    let mut all = view.listen == TunerListen::All;
                    let response = ui.add_enabled_ui(view.preset.is_some(), |ui| {
                        chip(ui, &mut all, "All strings")
                    });
                    if response.inner.changed() {
                        actions.listen = Some(TunerListen::All);
                    }
                    if view.preset.is_none() {
                        response
                            .inner
                            .on_disabled_hover_text("Pick a tuning to listen to each string");
                    }
                });
                group(ui, "Mute while tuning", colours, |ui| {
                    let mut on = view.auto_mute;
                    let label = if on { "On" } else { "Off" };
                    if chip(ui, &mut on, label).changed() {
                        actions.auto_mute = Some(on);
                    }
                });
                group(ui, "Display", colours, |ui| {
                    // `TunerDisplay` stores Strobe first.  The control lists
                    // Needle first, so the index is not the enum's order.
                    let mut choice = display_choice(view.display);
                    if segmented(ui, &mut choice, &["Needle", "Strobe"]).changed() {
                        actions.display = Some(display_from_choice(choice));
                    }
                });
                if !active {
                    ui.label(
                        RichText::new("The gauge starts with the bridge")
                            .font(font(Face::Sans, TypeRole::GridHeader))
                            .color(colours.text3),
                    );
                }
            });
        });
}

/// Index into the Display control, `["Needle", "Strobe"]`.
fn display_choice(display: TunerDisplay) -> usize {
    match display {
        TunerDisplay::Needle => 0,
        TunerDisplay::Strobe => 1,
    }
}

fn display_from_choice(choice: usize) -> TunerDisplay {
    if choice == 0 {
        TunerDisplay::Needle
    } else {
        TunerDisplay::Strobe
    }
}

fn group(ui: &mut egui::Ui, label: &str, colours: &Palette, add: impl FnOnce(&mut egui::Ui)) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 4.0;
        ui.label(RichText::new(label).font(small_font()).color(colours.text3));
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            add(ui);
        });
    });
}

/// A label in the bottom bar's scale.  Kept apart from the content font so a
/// long bar never grows taller.
fn small_font() -> egui::FontId {
    font(Face::Sans, TypeRole::GridHeader)
}

fn tuning_picker(ui: &mut egui::Ui, view: &View, actions: &mut Actions, colours: &Palette) {
    let selected = match view.tuning() {
        Some(tuning) => format!("{} · {}", group_prefix(tuning.group), tuning.name),
        None => "Chromatic".to_owned(),
    };
    egui::ComboBox::from_id_salt("tuner_tuning")
        .width(220.0)
        .height(380.0)
        .selected_text(
            RichText::new(selected)
                .font(font(Face::Sans, TypeRole::Control))
                .color(colours.text),
        )
        .show_ui(ui, |ui| {
            ui.set_min_width(260.0);
            if ui
                .selectable_label(view.preset.is_none(), "Chromatic · any note")
                .clicked()
            {
                actions.preset = Some(None);
            }
            for group in TuningGroup::ALL {
                ui.add_space(3.0);
                ui.label(
                    RichText::new(group.label())
                        .font(font(Face::SansMedium, TypeRole::GridHeader))
                        .color(colours.text3),
                );
                for (index, tuning) in TUNINGS
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
}

/// Flat and sharp share one hue.  Within this many cents they use the
/// warning colour; past it they use the error colour.
const PITCH_WARN_CENTS: f32 = 25.0;

fn state_colour(colours: &Palette, state: TuneState, cents: f32) -> Color32 {
    match state {
        TuneState::Centered | TuneState::InTune => colours.path[2],
        TuneState::Flat | TuneState::Sharp => {
            if cents.abs() <= PITCH_WARN_CENTS {
                colours.warn
            } else {
                colours.error
            }
        }
        TuneState::Silent => colours.text3,
    }
}

fn state_text(state: TuneState) -> &'static str {
    match state {
        TuneState::Centered => "Centered",
        TuneState::InTune => "In tune",
        TuneState::Flat => "Tune up",
        TuneState::Sharp => "Tune down",
        TuneState::Silent => "Listening",
    }
}

/// Absolute cents the readout prints as 0.0.  The strobe holds on a wider
/// band, so a tenth of a cent can still be a still disc.
const DISPLAYED_ZERO_CENTS: f32 = 0.05;

/// Where a cents mark sits on a ±50 track.  0 is the centre, −1 the left
/// edge, +1 the right edge.  Values outside ±50 clamp, and a non-finite
/// reading sits at the centre.
fn mark_fraction(cents: f32) -> f32 {
    if !cents.is_finite() {
        0.0
    } else {
        (cents / 50.0).clamp(-1.0, 1.0)
    }
}

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

fn counts_in_tune(sounding: bool, ghost: bool, state: TuneState) -> bool {
    sounding && !ghost && matches!(state, TuneState::Centered | TuneState::InTune)
}

fn tuning_count(frame: &TunerFrame, total: usize) -> (usize, usize) {
    let tuned = frame
        .strings
        .iter()
        .filter(|row| counts_in_tune(row.sounding, row.harmonic_of.is_some(), row.state))
        .count();
    (tuned, total)
}

fn group_prefix(group: TuningGroup) -> &'static str {
    match group {
        TuningGroup::Guitar6 => "Guitar",
        TuningGroup::Extended => "Extended",
        TuningGroup::Bass => "Bass",
    }
}

#[cfg(test)]
mod tests;
