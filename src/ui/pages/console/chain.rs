//! A path's chain on its strip (spec §4.2, §5.3): *Pitch* (not on a
//! return), *Pre EQ*, the gate when it sits before the effects, the NAM
//! blocks that do, the effects group, the NAM blocks after them, the
//! cabinet, the gate when after, and *Post EQ*.
//!
//! The built-in rows stay pinned; only the effects group scrolls, on its
//! own for each path so the faders stay on one line.  On overflow it shows
//! a 3 pt thumb, a fade at the bottom edge and *n more · scroll* in it.
//! The stage whose sheet is open is scrolled into view when it opens.
//! Dragging an effect reorders it, and so does dragging a NAM block inside
//! its group; the list scrolls at its edges.

use eframe::egui;
use egui::{Rect, Stroke, Vec2};

use super::DensityStep;
use crate::app::chain::ChainStage;
use crate::app::screen::{format_ms, Sheet};
use crate::app::RigWerkApp;
use crate::audio::{GateSlot, NamPosition, MAX_EFFECTS};
use crate::ui::fonts::Face;
use crate::ui::theme::{font, palette, TypeRole};
use crate::ui::widgets::{
    add_effect_row, chain_event, chain_row, effects_header, format_db, ChainEvent, ChainMark,
    ChainRow, EffectDrag, NamDrag, CHAIN_ROW_H,
};

/// A row and the gap under it.
const ROW_PITCH: f32 = CHAIN_ROW_H + 4.0;
/// The fade over the last visible rows when more follow.
const FADE_H: f32 = 22.0;
/// How close to an edge a dragged effect scrolls the list.
const EDGE: f32 = 16.0;

/// The scroll offset that shows `top`..`bottom` in a viewport `viewport`
/// tall now at `offset`: unchanged when it is already in view, else the
/// smallest move that brings it in.  The console's paths group uses it
/// sideways too.
pub(super) fn offset_to_show(top: f32, bottom: f32, offset: f32, viewport: f32) -> f32 {
    if top < offset {
        top.max(0.0)
    } else if bottom > offset + viewport {
        (bottom - viewport).max(0.0)
    } else {
        offset
    }
}

/// How many rows lie at least partly below the viewport.
pub(super) fn rows_below(content: f32, offset: f32, viewport: f32) -> usize {
    let hidden = content - offset - viewport;
    if hidden <= 1.0 {
        0
    } else {
        (hidden / ROW_PITCH).ceil() as usize
    }
}

/// One NAM row as the console draws it: what the block is called, what it
/// costs, and where it sits.
#[derive(Clone, Debug)]
struct NamRow {
    index: usize,
    shown: Option<String>,
    mark: String,
    failed: bool,
    reason: Option<String>,
    position: NamPosition,
}

impl RigWerkApp {
    /// The chain of path `slot`, at most `height` tall.
    pub(super) fn chain_column(
        &mut self,
        ui: &mut egui::Ui,
        slot: usize,
        height: f32,
        step: DensityStep,
    ) {
        // A NAM row shows its capture across the row, like an effect shows
        // its plugin; a block with none reads "Off".  The cabinet row does
        // the same for the impulse response.
        let Some((mix, nam, cab_shown, cab_mark, cab_failed, cab_reason)) =
            self.paths[slot].as_ref().map(|path| {
                let nam: Vec<NamRow> = path
                    .nam
                    .iter()
                    .enumerate()
                    .map(|(index, block)| NamRow {
                        index,
                        shown: (!block.name.is_empty()).then(|| block.name.clone()),
                        mark: if block.model.trim().is_empty() {
                            "Off".to_owned()
                        } else {
                            format!("{:.0} %", block.load_share(self.nam_rate()) * 100.0)
                        },
                        failed: block.error.is_some(),
                        reason: block.error.clone(),
                        position: block.controls.position,
                    })
                    .collect();
                let cab_none = path.cab_file.trim().is_empty();
                let cab_shown =
                    (!cab_none && !path.cab_name.is_empty()).then(|| path.cab_name.clone());
                let cab_mark = if cab_none { "Off" } else { "" };
                (
                    path.mix,
                    nam,
                    cab_shown,
                    cab_mark,
                    path.cab_error.is_some(),
                    path.cab_error.clone(),
                )
            })
        else {
            return;
        };
        let rate = match self.snapshot.output_rate {
            0 => self.sample_rate.max(1),
            rate => rate,
        };
        let show_curve = matches!(step, DensityStep::Normal | DensityStep::CompactOutputs);
        let open = self.chrome.sheet;
        let gate_text = format_db(mix.gate.threshold_db, 0);
        let pitch_text = crate::ui::sheets::pitch::row_mark(mix.pitch);
        let top = ui.cursor().top();
        let views = self.engine_plugin_views(slot);
        let count = views.len();
        let pre: Vec<NamRow> = nam
            .iter()
            .filter(|row| row.position == NamPosition::Pre)
            .cloned()
            .collect();
        let post: Vec<NamRow> = nam
            .iter()
            .filter(|row| row.position == NamPosition::Post)
            .cloned()
            .collect();
        let overloaded =
            self.issue_state.dsp.severity() == Some(crate::app::issues::IssueSeverity::Error);

        if !mix.is_return() {
            self.builtin_row(
                ui,
                slot,
                ChainStage::Pitch,
                "Pitch",
                None,
                ChainMark::Label(&pitch_text),
                false,
                None,
                show_curve,
                rate,
            );
        }
        self.builtin_row(
            ui,
            slot,
            ChainStage::PreEq,
            "Pre EQ",
            None,
            ChainMark::Eq(&mix.eqs[0]),
            false,
            None,
            show_curve,
            rate,
        );
        if mix.gate.slot == GateSlot::Pre {
            self.builtin_row(
                ui,
                slot,
                ChainStage::Gate,
                "Gate",
                None,
                ChainMark::Threshold(&gate_text),
                false,
                None,
                show_curve,
                rate,
            );
        }
        if pre.is_empty() {
            // The stack's way in: the row opens the sheet's add view.
            self.builtin_row(
                ui,
                slot,
                ChainStage::Nam(0),
                "NAM",
                None,
                ChainMark::Label("Off"),
                false,
                None,
                show_curve,
                rate,
            );
        }
        self.nam_rows(ui, slot, &pre, show_curve, rate, open, overloaded);

        let mut bypassed = mix.chain_bypass;
        let latency: u32 = views.iter().map(|view| view.latency).sum();
        let latency = if latency == 0 {
            String::new()
        } else {
            format_ms(latency as f32 * 1000.0 / rate as f32)
        };
        if effects_header(ui, count, &latency, &mut bypassed).changed() && self.select_path(slot) {
            self.set_chain_bypass(bypassed);
        }

        // What stays below the group: + Add effect, the NAM blocks after the
        // effects, the cabinet, and the gate after the effects.
        let gap = ui.spacing().item_spacing.y;
        let tail_rows =
            2.0 + post.len() as f32 + f32::from(u8::from(mix.gate.slot == GateSlot::Post));
        let used = ui.cursor().top() - top;
        let group = (height - used - gap - tail_rows * (CHAIN_ROW_H + gap)).max(ROW_PITCH);
        self.effect_rows(ui, slot, group, show_curve, rate, open);

        let add = add_effect_row(ui, count < MAX_EFFECTS);
        if add.clicked() {
            self.open_effect_browser(slot, count);
        }
        self.nam_rows(ui, slot, &post, show_curve, rate, open, overloaded);
        self.builtin_row(
            ui,
            slot,
            ChainStage::Cab,
            "Cab",
            cab_shown.as_deref(),
            ChainMark::Label(cab_mark),
            cab_failed,
            cab_reason.as_deref(),
            show_curve,
            rate,
        );
        if mix.gate.slot == GateSlot::Post {
            self.builtin_row(
                ui,
                slot,
                ChainStage::Gate,
                "Gate",
                None,
                ChainMark::Threshold(&gate_text),
                false,
                None,
                show_curve,
                rate,
            );
        }
        self.builtin_row(
            ui,
            slot,
            ChainStage::PostEq,
            "Post EQ",
            None,
            ChainMark::Eq(&mix.eqs[1]),
            false,
            None,
            show_curve,
            rate,
        );
    }

    /// The NAM blocks of one position group, one row each.  A dragged block
    /// reorders inside its group; the mark of the heaviest block turns to the
    /// warning colour while the DSP alarm is an error.
    #[allow(clippy::too_many_arguments)]
    fn nam_rows(
        &mut self,
        ui: &mut egui::Ui,
        slot: usize,
        rows: &[NamRow],
        show_curve: bool,
        rate: u32,
        open: Option<Sheet>,
        overloaded: bool,
    ) {
        let heaviest = if overloaded {
            self.nam_heaviest(slot).first().map(|(index, _, _)| *index)
        } else {
            None
        };
        let mut moved = None;
        for row in rows {
            let stage = ChainStage::Nam(row.index);
            let on = self.stage_on(slot, stage);
            let selected = open
                == Some(Sheet::Nam {
                    path: slot,
                    index: row.index,
                });
            let response = chain_row(
                ui,
                &ChainRow {
                    name: "NAM",
                    shown: row.shown.as_deref(),
                    index: row.index,
                    on,
                    selected,
                    failed: row.failed,
                    fail_reason: row.reason.as_deref(),
                    paused: false,
                    mark: ChainMark::Label(&row.mark),
                    show_curve,
                    sample_rate: rate,
                    menu: crate::ui::widgets::RowMenu::Nam,
                    warn: heaviest == Some(row.index),
                },
            );
            if let Some(drag) = response.dnd_hover_payload::<NamDrag>() {
                if drag.0 != row.index {
                    let y = if drag.0 < row.index {
                        response.rect.bottom() + 1.5
                    } else {
                        response.rect.top() - 1.5
                    };
                    ui.painter().hline(
                        response.rect.x_range(),
                        y,
                        Stroke::new(2.0, palette(ui.ctx()).accent),
                    );
                }
            }
            if let Some(drag) = response.dnd_release_payload::<NamDrag>() {
                if drag.0 != row.index {
                    moved = Some((drag.0, row.index));
                }
            }
            self.chain_row_event(slot, stage, chain_event(&response), on);
        }
        if let Some((from, to)) = moved {
            let index = self
                .move_nam_block(slot, from, to as isize - from as isize)
                .unwrap_or(from);
            self.open_sheet(Sheet::Nam { path: slot, index });
        }
    }

    /// One pinned row: the pitch shifter, an EQ, the gate, a NAM block's
    /// way in, or the cabinet.
    #[allow(clippy::too_many_arguments)]
    fn builtin_row(
        &mut self,
        ui: &mut egui::Ui,
        slot: usize,
        stage: ChainStage,
        name: &str,
        shown: Option<&str>,
        mark: ChainMark<'_>,
        failed: bool,
        fail_reason: Option<&str>,
        show_curve: bool,
        rate: u32,
    ) {
        let on = self.stage_on(slot, stage);
        let selected = self.chrome.sheet == Some(Sheet::for_stage(slot, stage));
        let response = chain_row(
            ui,
            &ChainRow {
                name,
                shown,
                index: 0,
                on,
                selected,
                failed,
                fail_reason,
                paused: false,
                mark,
                show_curve,
                sample_rate: rate,
                menu: if stage == ChainStage::Pitch {
                    crate::ui::widgets::RowMenu::Switch
                } else {
                    crate::ui::widgets::RowMenu::Builtin
                },
                warn: false,
            },
        );
        self.chain_row_event(slot, stage, chain_event(&response), on);
    }

    /// The effects, in a list of their own that scrolls past `height`.
    fn effect_rows(
        &mut self,
        ui: &mut egui::Ui,
        slot: usize,
        height: f32,
        show_curve: bool,
        rate: u32,
        open: Option<Sheet>,
    ) {
        let views = self.engine_plugin_views(slot);
        if views.is_empty() {
            return;
        }
        let colours = palette(ui.ctx());
        let content = views.len() as f32 * ROW_PITCH;
        let viewport = content.min(height);
        let offset_id = egui::Id::unique(("console_chain_offset", slot));
        let offset: f32 = ui.data(|data| data.get_temp(offset_id)).unwrap_or(0.0);
        let mut scroll = egui::ScrollArea::vertical()
            .id_salt(("console_chain", slot))
            .max_height(viewport)
            .auto_shrink([false, true]);
        if self.chrome.follow_stage {
            if let Some(Sheet::Effect { path, index }) = open {
                if path == slot {
                    let top = index as f32 * ROW_PITCH;
                    scroll = scroll.vertical_scroll_offset(offset_to_show(
                        top,
                        top + CHAIN_ROW_H,
                        offset,
                        viewport,
                    ));
                }
            }
        }
        let mut events = Vec::new();
        let mut moved = None;
        let output = ui
            .scope(|ui| {
                let scroll_style = &mut ui.spacing_mut().scroll;
                scroll_style.bar_width = 3.0;
                scroll_style.floating = true;
                scroll_style.floating_width = 3.0;
                scroll_style.floating_allocated_width = 0.0;
                scroll.show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 4.0;
                    for (index, view) in views.iter().enumerate() {
                        let latency = if view.latency == 0 {
                            String::new()
                        } else {
                            format_ms(view.latency as f32 * 1000.0 / rate as f32)
                        };
                        let response = chain_row(
                            ui,
                            &ChainRow {
                                name: &view.name,
                                shown: None,
                                index,
                                on: !view.bypass,
                                selected: open == Some(Sheet::Effect { path: slot, index }),
                                failed: view.error.is_some(),
                                fail_reason: view.error.as_deref(),
                                paused: view.paused,
                                mark: ChainMark::Latency(&latency),
                                show_curve,
                                sample_rate: rate,
                                menu: crate::ui::widgets::RowMenu::Effect,
                                warn: false,
                            },
                        );
                        if let Some(drag) = response.dnd_hover_payload::<EffectDrag>() {
                            if drag.0 != index {
                                let y = if drag.0 < index {
                                    response.rect.bottom() + 1.5
                                } else {
                                    response.rect.top() - 1.5
                                };
                                ui.painter().hline(
                                    response.rect.x_range(),
                                    y,
                                    Stroke::new(2.0, colours.accent),
                                );
                            }
                        }
                        if let Some(drag) = response.dnd_release_payload::<EffectDrag>() {
                            if drag.0 != index {
                                moved = Some((drag.0, index));
                            }
                        }
                        if let Some(event) = chain_event(&response) {
                            events.push((index, event, !view.bypass));
                        }
                    }
                    // A dragged effect scrolls the list at its edges.
                    if egui::DragAndDrop::has_payload_of_type::<EffectDrag>(ui.ctx()) {
                        if let Some(pointer) = ui.ctx().pointer_hover_pos() {
                            let clip = ui.clip_rect();
                            if clip.x_range().contains(pointer.x) {
                                if pointer.y < clip.top() + EDGE {
                                    ui.scroll_with_delta(Vec2::new(0.0, 4.0));
                                } else if pointer.y > clip.bottom() - EDGE {
                                    ui.scroll_with_delta(Vec2::new(0.0, -4.0));
                                }
                            }
                        }
                    }
                })
            })
            .inner;
        ui.data_mut(|data| data.insert_temp(offset_id, output.state.clamped_offset().y));
        let below = rows_below(
            output.content_size.y,
            output.state.clamped_offset().y,
            output.inner_rect.height(),
        );
        if below > 0 {
            fade(ui, output.inner_rect, below);
        }
        for (index, event, on) in events {
            self.chain_row_event(slot, ChainStage::Effect(index), Some(event), on);
        }
        if let Some((from, to)) = moved {
            self.move_effect(slot, from, to as isize - from as isize);
        }
    }

    /// What a row's click, LED, double-click, menu or keys asked for.
    fn chain_row_event(
        &mut self,
        slot: usize,
        stage: ChainStage,
        event: Option<ChainEvent>,
        on: bool,
    ) {
        let Some(event) = event else {
            return;
        };
        match (event, stage) {
            (ChainEvent::Select, _) => self.open_sheet(Sheet::for_stage(slot, stage)),
            (ChainEvent::Bypass, _) => self.set_stage_on(slot, stage, !on),
            (ChainEvent::Open, ChainStage::Effect(index)) => self.open_effect(slot, index),
            (ChainEvent::Editor, ChainStage::Effect(index)) if self.select_path(slot) => {
                self.open_plugin_editor(index);
            }
            (ChainEvent::Controls, ChainStage::Effect(index)) => {
                self.open_sheet(Sheet::Effect { path: slot, index });
            }
            (ChainEvent::MoveUp, ChainStage::Effect(index)) => self.move_effect(slot, index, -1),
            (ChainEvent::MoveDown, ChainStage::Effect(index)) => self.move_effect(slot, index, 1),
            (ChainEvent::Remove, ChainStage::Effect(index)) => self.remove_effect(slot, index),
            (ChainEvent::MoveUp, ChainStage::Nam(index)) => {
                self.move_nam_block(slot, index, -1);
            }
            (ChainEvent::MoveDown, ChainStage::Nam(index)) => {
                self.move_nam_block(slot, index, 1);
            }
            (ChainEvent::Remove, ChainStage::Nam(index)) => {
                self.remove_nam_block(slot, index);
            }
            _ => {}
        }
    }
}

/// The fade at the bottom of an overflowing effects list, with *n more ·
/// scroll* in it.
fn fade(ui: &egui::Ui, viewport: Rect, below: usize) {
    let colours = palette(ui.ctx());
    let rect = Rect::from_min_max(
        egui::pos2(viewport.left(), viewport.bottom() - FADE_H),
        viewport.max,
    );
    let mut mesh = egui::Mesh::default();
    let clear = colours.strip.gamma_multiply(0.0);
    mesh.colored_vertex(rect.left_top(), clear);
    mesh.colored_vertex(rect.right_top(), clear);
    mesh.colored_vertex(rect.left_bottom(), colours.strip);
    mesh.colored_vertex(rect.right_bottom(), colours.strip);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(1, 3, 2);
    ui.painter().add(egui::Shape::mesh(mesh));
    ui.painter().text(
        egui::pos2(rect.center().x, rect.bottom() - 6.0),
        egui::Align2::CENTER_CENTER,
        format!("{below} more · scroll"),
        font(Face::Sans, TypeRole::GridHeader),
        colours.text2,
    );
}

#[cfg(test)]
mod tests;
