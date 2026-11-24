//! Match paths: one loudness trim per path, measured all at once.
//!
//! A path's loudness compared with the other paths is the same on every
//! output, so the trim lives on the path, not on a path and output pair.
//! "Match paths" measures every path alone, at the same point its meter
//! reads (after the post EQ, before the level and the match), then proposes
//! one trim per path.  Nothing moves until Apply writes them; the mixer
//! ramps the new gains like a level change.
//!
//! Every path is measured, muted or not: the meter sits before the gain the
//! mute and solo use, so a muted path reads what it would play.  The
//! reference is either a target LUFS or one path: that path keeps its trim
//! and the others are matched to it.  Returns follow their senders, so they
//! are unticked by default; the user can tick them.

use serde::{Deserialize, Serialize};

use crate::app::screen::Sheet;
use crate::app::{LogTone, RigWerkApp};
use crate::audio::{
    EngineState, LOUDNESS_TARGET_MAX_LUFS, LOUDNESS_TARGET_MIN_LUFS, PATH_MATCH_MAX_DB,
    SILENCE_LUFS,
};

/// How long every path is measured before its match is proposed.
pub(crate) const MATCH_PATHS_SECONDS: f32 = 4.0;
/// About 3 s of gated 400 ms blocks, as for Learn.
pub(crate) const MATCH_PATHS_MIN_BLOCKS: u32 = 27;
/// Where NAM's own normalization lands, with headroom before the outputs.
pub(crate) const MATCH_PATHS_TARGET_DEFAULT_LUFS: f32 = -18.0;
/// A reading more than this far under the reference is noise, not program.
const MATCH_PATHS_GATE_BELOW_REFERENCE_LU: f32 = 20.0;

/// The saved Match paths reference.  A NaN target becomes the default; the
/// target is clamped to the sheet's range and a stale path is dropped.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct MatchPathsSettings {
    /// Target integrated loudness, LUFS.
    pub(crate) target_lufs: f32,
    /// One path's position in the saved path list, as the reference.  The
    /// app maps it to a slot when it loads; `None` is the target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) reference_path: Option<usize>,
}

impl Default for MatchPathsSettings {
    fn default() -> Self {
        Self {
            target_lufs: MATCH_PATHS_TARGET_DEFAULT_LUFS,
            reference_path: None,
        }
    }
}

/// Whether this is the default, so a save can leave the field out and keep
/// an older file's bytes unchanged.
pub(crate) fn is_default(settings: &MatchPathsSettings) -> bool {
    *settings == MatchPathsSettings::default()
}

impl MatchPathsSettings {
    pub(crate) fn sanitized(self) -> Self {
        let target_lufs = if self.target_lufs.is_nan() {
            MATCH_PATHS_TARGET_DEFAULT_LUFS
        } else {
            self.target_lufs
                .clamp(LOUDNESS_TARGET_MIN_LUFS, LOUDNESS_TARGET_MAX_LUFS)
        };
        Self {
            target_lufs,
            reference_path: self
                .reference_path
                .filter(|path| *path < crate::audio::MAX_PATHS),
        }
    }
}

/// Why a path's proposed trim is not ticked by default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MatchPathReason {
    /// No usable reading: silence or too short a window.
    NotHeard,
    /// More than 20 dB under the reference.
    TooQuiet,
    /// A return follows its sends; tickable, but not by default.
    Return,
}

impl MatchPathReason {
    /// The note in the results table.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::NotHeard => "not heard",
            Self::TooQuiet => "too quiet",
            Self::Return => "a return follows its sends",
        }
    }

    /// Whether the row can be ticked at all.
    pub(crate) fn blocks_apply(self) -> bool {
        !matches!(self, Self::NotHeard | Self::TooQuiet)
    }
}

/// One path's row after a measurement.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MatchPathRow {
    pub(crate) slot: usize,
    /// Gated integrated reading, LUFS.
    pub(crate) measured_lufs: f32,
    /// Gated 400 ms blocks behind the reading.
    pub(crate) blocks: u32,
    pub(crate) current_db: f32,
    /// The proposed trim, already clamped.
    pub(crate) new_db: f32,
    /// `new_db - current_db`.
    pub(crate) change_db: f32,
    pub(crate) reason: Option<MatchPathReason>,
    pub(crate) ticked: bool,
    /// Whether the proposal hit the ±24 dB limit.
    pub(crate) clamped: bool,
}

/// The finished measurement the sheet shows.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MatchPathsResult {
    pub(crate) rows: Vec<MatchPathRow>,
    /// The reference the proposals were worked out against.
    pub(crate) reference_lufs: f32,
    /// The reference path, when one was used.
    pub(crate) reference_path: Option<usize>,
}

impl RigWerkApp {
    /// Starts measuring every live path at once.  Nothing changes until
    /// [`Self::apply_match_paths`].
    pub(crate) fn start_match_paths(&mut self) {
        self.cancel_loudness_measurement();
        self.match_paths_result = None;
        self.match_paths_ticks.clear();
        self.match_paths_status = None;
        if self.engine.state != EngineState::Running {
            self.push_log(
                "Start the bridge before matching the paths.",
                LogTone::Amber,
            );
            return;
        }
        self.engine.start_match_paths();
        self.loudness_measurement = Some(crate::app::output_devices::LoudnessMeasurement {
            device: 0,
            kind: crate::app::output_devices::LoudnessMeasurementKind::Paths,
            started: std::time::Instant::now(),
        });
        self.push_log(
            "Matching paths… play the rig for four seconds.",
            LogTone::Blue,
        );
    }

    /// Stops a running Match paths measurement.  A finished result is kept
    /// so Cancel can leave the trims alone.
    pub(crate) fn cancel_match_paths(&mut self) {
        if self.loudness_measurement.is_some_and(|request| {
            request.kind == crate::app::output_devices::LoudnessMeasurementKind::Paths
        }) {
            self.engine.stop_match_paths();
            self.loudness_measurement = None;
        }
        self.match_paths_result = None;
        self.match_paths_ticks.clear();
        self.match_paths_status = None;
    }

    /// Whether the Match paths measurement is running.
    pub(crate) fn is_matching_paths(&self) -> bool {
        self.loudness_measurement.is_some_and(|request| {
            request.kind == crate::app::output_devices::LoudnessMeasurementKind::Paths
        })
    }

    /// How far the running measurement is, 0 to 1.
    pub(crate) fn match_paths_progress(&self) -> f32 {
        let Some(request) = self.loudness_measurement else {
            return 0.0;
        };
        if request.kind != crate::app::output_devices::LoudnessMeasurementKind::Paths {
            return 0.0;
        }
        (request.started.elapsed().as_secs_f32() / MATCH_PATHS_SECONDS).clamp(0.0, 1.0)
    }

    /// Points the reference at live slot `slot`, or back at the target.
    /// Clears the result: the proposals were for the old reference.
    pub(crate) fn set_match_reference(&mut self, slot: Option<usize>) {
        self.match_paths_reference = slot;
        self.match_paths.reference_path =
            slot.and_then(|slot| self.path_slots().iter().position(|&other| other == slot));
        self.match_paths_result = None;
        self.match_paths_ticks.clear();
        self.match_paths_status = None;
    }

    /// The saved settings: the reference slot becomes a position in the
    /// saved path order.
    pub(crate) fn saved_match_paths(&self, order: &[usize]) -> MatchPathsSettings {
        MatchPathsSettings {
            target_lufs: self.match_paths.target_lufs,
            reference_path: self
                .match_paths_reference
                .and_then(|slot| order.iter().position(|&other| other == slot)),
        }
    }

    /// Loads saved settings: the reference position becomes a slot in the
    /// current path order.
    pub(crate) fn install_match_paths(&mut self, settings: MatchPathsSettings) {
        self.match_paths = settings.sanitized();
        self.match_paths_reference = self
            .match_paths
            .reference_path
            .and_then(|position| self.path_slots().get(position).copied());
    }

    /// Collects the measurement and proposes the trims.  Runs once when the
    /// window is over.
    pub(crate) fn finish_match_paths(&mut self) {
        self.engine.stop_match_paths();
        self.loudness_measurement = None;
        let settings = self.match_paths.sanitized();
        let mut readings = Vec::new();
        for slot in self.path_slots() {
            let Some(path) = self.paths[slot].as_ref() else {
                continue;
            };
            readings.push((
                slot,
                path.mix.match_db,
                path.mix.is_return(),
                self.engine.path_match_measurement(slot),
            ));
        }
        // The meter reads before the path gain, so a reading never includes
        // its own path's match or level.  A target is therefore reached with
        // `target - measured`; matching to a path keeps that path's trim.
        let reference_slot = self.match_paths_reference;
        let reference = reference_slot
            .and_then(|slot| readings.iter().find(|reading| reading.0 == slot))
            .map(|reading| (reading.0, reading.1, reading.3))
            .filter(|(_, _, (lufs, blocks))| {
                *lufs > SILENCE_LUFS && *blocks >= MATCH_PATHS_MIN_BLOCKS
            });
        let reference_lufs = reference
            .map(|(_, _, (lufs, _))| lufs)
            .unwrap_or(settings.target_lufs);
        let reference_match = reference.map(|(_, current, _)| current);
        // A path reference plays at its reading plus its own match.  A
        // target reference plays at the target.
        let reference_plays_lufs = reference_lufs + reference_match.unwrap_or(0.0);
        let reference_path = reference.map(|(slot, _, _)| slot);
        let mut rows = Vec::with_capacity(readings.len());
        let mut heard = 0;
        for (slot, current_db, is_return, (measured_lufs, blocks)) in readings {
            let reason = if measured_lufs <= SILENCE_LUFS || blocks < MATCH_PATHS_MIN_BLOCKS {
                Some(MatchPathReason::NotHeard)
            } else if measured_lufs < reference_plays_lufs - MATCH_PATHS_GATE_BELOW_REFERENCE_LU {
                Some(MatchPathReason::TooQuiet)
            } else if is_return {
                Some(MatchPathReason::Return)
            } else {
                None
            };
            let usable = reason.is_none_or(MatchPathReason::blocks_apply);
            if usable && measured_lufs > SILENCE_LUFS {
                heard += 1;
            }
            let unclamped = if reference_path == Some(slot) {
                current_db
            } else if let Some(reference_match) = reference_match {
                reference_lufs + reference_match - measured_lufs
            } else {
                settings.target_lufs - measured_lufs
            };
            let new_db = if usable {
                unclamped.clamp(-PATH_MATCH_MAX_DB, PATH_MATCH_MAX_DB)
            } else {
                current_db
            };
            let clamped = (unclamped - new_db).abs() > 0.05;
            rows.push(MatchPathRow {
                slot,
                measured_lufs,
                blocks,
                current_db,
                new_db,
                change_db: new_db - current_db,
                reason,
                ticked: reason.is_none(),
                clamped,
            });
        }
        if heard == 0 {
            self.push_log(
                "No path had a loudness reading; play the rig and match again.",
                LogTone::Amber,
            );
            self.match_paths_result = None;
            self.match_paths_ticks.clear();
            return;
        }
        let clamped = rows.iter().any(|row| row.ticked && row.clamped);
        self.match_paths_ticks = rows.iter().map(|row| row.ticked).collect();
        self.match_paths_status = None;
        self.match_paths_result = Some(MatchPathsResult {
            rows,
            reference_lufs: reference_plays_lufs,
            reference_path,
        });
        if clamped {
            self.push_log(
                "A path hit the match trim limit; adjust its level and match again.",
                LogTone::Amber,
            );
        } else {
            self.push_log(
                "Measured every path. Apply writes the trims.",
                LogTone::Blue,
            );
        }
    }

    /// Writes the ticked rows and pushes the paths; the mixer ramps them in.
    /// The result stays on screen so the sheet can report what it applied.
    pub(crate) fn apply_match_paths(&mut self, ticked: &[bool]) {
        let Some(result) = self.match_paths_result.clone() else {
            return;
        };
        let mut changed = 0;
        let mut applied = Vec::new();
        let mut applied_slots = Vec::new();
        for (row, ticked) in result.rows.iter().zip(ticked) {
            if !ticked || !row.reason.is_none_or(MatchPathReason::blocks_apply) {
                continue;
            }
            if let Some(path) = self.paths.get_mut(row.slot).and_then(Option::as_mut) {
                if (path.mix.match_db - row.new_db).abs() >= 0.005 {
                    path.mix.match_db = row.new_db;
                    changed += 1;
                    applied_slots.push(row.slot);
                    applied.push(format!("{} {}", path.name, signed_db(row.new_db)));
                }
            }
        }
        if let Some(result) = self.match_paths_result.as_mut() {
            for row in &mut result.rows {
                if applied_slots.contains(&row.slot) {
                    row.current_db = row.new_db;
                    row.change_db = 0.0;
                }
            }
        }
        if changed > 0 {
            self.push_paths_to_engine();
            self.match_paths_status = Some(format!("Applied: {}.", applied.join(", ")));
            self.push_log(
                format!(
                    "Matched {changed} path(s) to {:.1} LUFS.",
                    result.reference_lufs
                ),
                LogTone::Teal,
            );
        } else {
            self.match_paths_status = Some("Nothing to apply.".to_owned());
        }
    }

    /// Opens the Match paths sheet.
    pub(crate) fn open_match_paths(&mut self) {
        self.open_sheet(Sheet::MatchPaths);
    }
}

fn signed_db(db: f32) -> String {
    let tenths = (db * 10.0).round();
    if tenths == 0.0 {
        "0.0 dB".to_owned()
    } else if tenths < 0.0 {
        format!("−{:.1} dB", -tenths / 10.0)
    } else {
        format!("+{:.1} dB", tenths / 10.0)
    }
}

#[cfg(test)]
mod tests;
