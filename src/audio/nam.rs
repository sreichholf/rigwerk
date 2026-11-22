//! Stackable Neural Amp Modeler blocks, up to [`MAX_NAM_BLOCKS`] per signal
//! path.
//!
//! A `.nam` file is a frozen capture: there are no amp knobs.  The audio
//! callback never builds, parses, or drops a [`nam_rs::Model`].  The UI
//! thread (or the loader thread it owns) builds one, resets it, and prewarms
//! `receptive_field()` samples of silence before queueing it.  The callback
//! only `try_lock`s that queue at the start of a block.  A model it replaces
//! is moved into [`NamPark`] the same way, and only the UI thread drains that
//! park.
//!
//! [`NamStack`] is the published order of the blocks: each carries an `id`
//! (the UI's handle on one block), a position (before or after the plugin
//! effects), its trims and whether it matches the level of the block before
//! it.  A block's runtime state — its model, its fades, its warmth — stays
//! with the id, so reordering the stack never moves a model and never
//! restarts one.
//!
//! The path is stereo.  Each model hears `(L + R) * 0.5` and its mono result
//! is written to both channels.  Input, output and level-match gains ramp.
//! A model is causal, and the receptive field is only how much history it
//! needs, so one block adds no latency of its own.  A group of two or more
//! captures that can be oversampled runs every block at twice the output
//! rate through a fixed 2x up/down pair ([`OversampleFilters`]); such a
//! block delays its wet and dry output by the pair's group delay, and the
//! path reports that as latency.  A single capture, or a group of one, runs
//! the model as it was captured, with no resampler and no added latency.
//! Bypass plays the dry signal, delayed by the same amount when its block
//! is oversampled.
//!
//! A model is not heard before it has heard the signal: one taken from the
//! queue, or one leaving bypass, first runs `receptive_field()` samples of
//! the live input silently, then crossfades in.  A swap fades from the old
//! model (or the dry signal) to the new one over [`SWAP_FADE_SECONDS`]; a
//! clear fades back to dry.  One swap at a time: a new model or a clear
//! waits until the model the last one faded out is parked, so the callback
//! never lets go of a model by overwriting it.  The width dial of a slimmable capture is applied
//! by the UI, which queues a model built at that width.  A sample rate that
//! is not the file's rate does not install: the only resampling here is the
//! fixed 2x pair of an oversampled group, and it is never used to move a
//! capture to another rate.
//!
//! The stack changes without a click.  A block that leaves it keeps running
//! at its old place and fades to dry over the bypass fade; only then is its
//! model parked.  A reorder, or a move between `Pre` and `Post`, that changes
//! the order of blocks that are heard dips the groups it touches to silence
//! over the bypass fade, switches the order there, and fades back in.  The
//! models keep running through the dip, so none restarts.
//!
//! Level matching: a capture only reacts as it was trained to when its input
//! sits at the level the capture was made at.  The calibration levels are the
//! analog level, in dBu, that 0 dBFS stands for.  A signal at x dBFS out of
//! block A is `x + out_A` dBu, which block B reads as `x + out_A - in_B`
//! dBFS, so B gets `out_A - in_B` dB.  A is the nearest block before B in
//! the same group that runs: a bypassed block, or one without a model, passes
//! the signal dry and is skipped.  Without the metadata the gain is 0 dB.

use std::sync::{Arc, Mutex, PoisonError, TryLockError};

use serde::{Deserialize, Serialize};

use super::mixer::ramp::Ramp;

/// The most NAM blocks one path runs.  Memory and load-time protection only,
/// not a product decision: 16 models of one medium capture are already more
/// than the DSP budget of a small buffer.
pub const MAX_NAM_BLOCKS: usize = 16;

/// From this many NAM blocks on, the sheet asks once more before adding one.
pub const NAM_SOFT_LIMIT: usize = 5;

/// How long bypass, and leaving bypass, crossfade.  Long enough that the
/// switch does not click.
const BYPASS_FADE_SECONDS: f32 = 0.010;
/// How long replacing the model of one block crossfades.  Longer than a
/// bypass, because both sides are models with their own tails.
const SWAP_FADE_SECONDS: f32 = 0.035;
/// The widest level match two calibrations can ask for, dB.  A file with
/// nonsense levels must not blow the chain up.
pub const NAM_MAX_MATCH_DB: f32 = 36.0;

/// Soft limits for the trims and for [`NamBlockControls::sanitized`].
pub const NAM_MIN_TRIM_DB: f32 = -24.0;
pub const NAM_MAX_TRIM_DB: f32 = 24.0;

/// Where a NAM block sits relative to the plugin effects.  Blocks in the same
/// position run in stack order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum NamPosition {
    /// Before the effects.  The default.
    #[default]
    Pre,
    /// After the effects, before a post gate and the post EQ.
    Post,
}

impl NamPosition {
    /// The two groups, in signal order.
    pub const ALL: [Self; 2] = [Self::Pre, Self::Post];

    fn index(self) -> usize {
        match self {
            Self::Pre => 0,
            Self::Post => 1,
        }
    }
}

/// Numeric controls one block shares with the callback.  The `.nam` path is
/// not here: copying it into the callback would allocate, and [`NamStack`]
/// stays [`Copy`].
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NamBlockControls {
    /// The UI's handle on this block.  Unique within a path, and never
    /// reused, so a reorder or a removal cannot be mistaken for a swap.
    /// Not saved: a loaded path numbers its blocks itself.
    #[serde(skip)]
    pub id: u32,
    /// Header On/Bypass.  Off skips the model.
    pub enabled: bool,
    pub position: NamPosition,
    /// Gain into the model, dB.
    pub input_db: f32,
    /// Gain on the model output, dB.
    pub output_db: f32,
    /// SlimmableContainer width, 0..=1.  Ignored by other models.  The
    /// stage does not read it: the UI queues a model built at this width.
    pub slim: f32,
    /// Match this block's input to the block before it when both carry
    /// calibration levels.
    pub auto_level: bool,
    /// The capture's analog input level at 0 dBFS, dBu.  `None` when the
    /// file does not say.
    pub level_in_dbu: Option<f32>,
    /// The capture's analog output level at 0 dBFS, dBu.
    pub level_out_dbu: Option<f32>,
}

impl Default for NamBlockControls {
    fn default() -> Self {
        Self {
            id: 0,
            enabled: true,
            position: NamPosition::Pre,
            input_db: 0.0,
            output_db: 0.0,
            slim: 1.0,
            auto_level: true,
            level_in_dbu: None,
            level_out_dbu: None,
        }
    }
}

impl NamBlockControls {
    pub fn sanitized(self) -> Self {
        Self {
            enabled: self.enabled,
            position: self.position,
            input_db: finite_or(self.input_db, 0.0).clamp(NAM_MIN_TRIM_DB, NAM_MAX_TRIM_DB),
            output_db: finite_or(self.output_db, 0.0).clamp(NAM_MIN_TRIM_DB, NAM_MAX_TRIM_DB),
            slim: finite_or(self.slim, 1.0).clamp(0.0, 1.0),
            auto_level: self.auto_level,
            level_in_dbu: finite_level(self.level_in_dbu),
            level_out_dbu: finite_level(self.level_out_dbu),
            ..self
        }
    }

    /// Whether both sides of a level match are known.
    fn calibrated(&self) -> bool {
        self.level_in_dbu.is_some() && self.level_out_dbu.is_some()
    }
}

/// The published blocks of one path, in signal order: the `Pre` group first,
/// then the `Post` group.  Only the first `count` entries are live.
///
/// [`Copy`], so the callback can take it with one `try_lock` and the mixer
/// needs no allocation to pass it on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NamStack {
    blocks: [NamBlockControls; MAX_NAM_BLOCKS],
    count: u8,
}

impl Default for NamStack {
    fn default() -> Self {
        Self {
            blocks: [NamBlockControls::default(); MAX_NAM_BLOCKS],
            count: 0,
        }
    }
}

impl NamStack {
    pub fn blocks(&self) -> &[NamBlockControls] {
        &self.blocks[..self.count as usize]
    }

    pub fn count(&self) -> usize {
        self.count as usize
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn get(&self, index: usize) -> Option<&NamBlockControls> {
        self.blocks().get(index)
    }

    /// The index of the block with `id`, if it is in the stack.
    pub fn index_of(&self, id: u32) -> Option<usize> {
        self.blocks().iter().position(|block| block.id == id)
    }

    /// The blocks of one position, in signal order, with their stack index.
    pub fn in_position(
        &self,
        position: NamPosition,
    ) -> impl Iterator<Item = (usize, &NamBlockControls)> {
        self.blocks()
            .iter()
            .enumerate()
            .filter(move |(_, block)| block.position == position)
    }

    /// Adds `block` at the end of its position group.  `false` when the stack
    /// is full.
    pub fn push(&mut self, block: NamBlockControls) -> bool {
        if self.count() >= MAX_NAM_BLOCKS {
            return false;
        }
        let at = self.group_end(block.position);
        self.insert_at(at, block);
        true
    }

    /// Swaps the block at `index` with its neighbour in its group (`-1` is
    /// the one before it).  `false` at the edge of a group.
    pub fn move_block(&mut self, index: usize, by: isize) -> bool {
        let Some(block) = self.get(index).copied() else {
            return false;
        };
        let neighbour = if by < 0 {
            (0..index).rev().find(|&before| {
                self.get(before)
                    .is_some_and(|b| b.position == block.position)
            })
        } else if by > 0 {
            (index + 1..self.count()).find(|&after| {
                self.get(after)
                    .is_some_and(|b| b.position == block.position)
            })
        } else {
            None
        };
        match neighbour {
            Some(neighbour) => {
                self.blocks.swap(index, neighbour);
                true
            }
            None => false,
        }
    }

    /// Moves the block at `index` to the end of `position`'s group.
    /// `false` when it is already there.
    pub fn set_position(&mut self, index: usize, position: NamPosition) -> bool {
        if self
            .get(index)
            .is_none_or(|block| block.position == position)
        {
            return false;
        }
        let Some(mut block) = self.remove(index) else {
            return false;
        };
        block.position = position;
        let at = self.group_end(position);
        self.insert_at(at, block);
        true
    }

    pub fn remove(&mut self, index: usize) -> Option<NamBlockControls> {
        if index >= self.count() {
            return None;
        }
        let removed = self.blocks[index];
        for move_index in index..self.count() - 1 {
            self.blocks[move_index] = self.blocks[move_index + 1];
        }
        self.count -= 1;
        Some(removed)
    }

    /// The stack index one past the last block of `position`'s group.  A
    /// `Pre` block with nothing before the effects goes in front of them.
    fn group_end(&self, position: NamPosition) -> usize {
        match self
            .blocks()
            .iter()
            .rposition(|block| block.position == position)
        {
            Some(last) => last + 1,
            None if position == NamPosition::Pre => 0,
            None => self.count(),
        }
    }

    fn insert_at(&mut self, index: usize, block: NamBlockControls) {
        let count = self.count();
        let mut move_index = count;
        while move_index > index {
            self.blocks[move_index] = self.blocks[move_index - 1];
            move_index -= 1;
        }
        self.blocks[index] = block;
        self.count += 1;
    }

    /// Every value in range, and every id present once.
    pub fn sanitized(self) -> Self {
        let mut out = Self::default();
        for block in self.blocks() {
            let block = block.sanitized();
            if out.index_of(block.id).is_some() {
                continue;
            }
            if !out.push(block) {
                break;
            }
        }
        out
    }

    /// The level match each block applies, in dB, by stack index: the
    /// nearest running block before it in the same position group gives its
    /// output level, this one takes it as its input level
    /// (`out_previous - in_this`).  A bypassed block passes the signal dry:
    /// it gets 0 dB and the block after it matches the one before it.  A
    /// block without a running neighbour, without calibration data, or with
    /// auto level off gets 0 dB.
    ///
    /// `runs(id)` says whether a block has a model: one without passes the
    /// signal dry and is skipped like a bypassed one.  The stage knows which
    /// blocks have one; the stack does not.
    pub(crate) fn level_matches(&self, runs: impl Fn(u32) -> bool) -> [f32; MAX_NAM_BLOCKS] {
        let mut matches = [0.0f32; MAX_NAM_BLOCKS];
        for position in NamPosition::ALL {
            let mut previous: Option<&NamBlockControls> = None;
            for (index, block) in self.blocks().iter().enumerate() {
                if block.position != position || !block.enabled || !runs(block.id) {
                    continue;
                }
                let gain = previous
                    .filter(|previous| previous.calibrated() && block.calibrated())
                    .filter(|_| block.auto_level)
                    .and_then(|previous| Some(previous.level_out_dbu? - block.level_in_dbu?))
                    .map_or(0.0, |db| db.clamp(-NAM_MAX_MATCH_DB, NAM_MAX_MATCH_DB));
                matches[index] = gain;
                previous = Some(block);
            }
        }
        matches
    }
}

fn finite_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() {
        value
    } else {
        fallback
    }
}

fn finite_level(value: Option<f32>) -> Option<f32> {
    value.filter(|level| level.is_finite())
}

fn db_to_gain(db: f32) -> f32 {
    10.0f32.powf(db / 20.0)
}

fn rates_match(expected: f64, rate: u32) -> bool {
    expected.is_finite() && expected.round() as u32 == rate
}

/// One saved NAM block: the capture file and the controls it runs with.
/// The pair a [`PathPref`](crate::app::settings::PathPref) stores per block;
/// a path's blocks are in signal order.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NamBlockSettings {
    /// Empty means the block has no capture and passes its input through.
    pub model: String,
    pub controls: NamBlockControls,
}

impl NamBlockSettings {
    /// Trims the path and puts every value in range.
    pub fn sanitized(&self) -> Self {
        Self {
            model: self.model.trim().to_owned(),
            controls: self.controls.sanitized(),
        }
    }

    /// The settings that reproduce a legacy single stage.
    pub fn from_legacy(legacy: &LegacyNamSettings) -> Self {
        let legacy = legacy.sanitized();
        Self {
            model: legacy.model_path.clone(),
            controls: legacy.controls(),
        }
    }
}

/// The single NAM stage older saves and sessions have (`PathPref::amp`).
/// [`PathPref`](crate::app::settings::PathPref) reads it once and folds it
/// into the block list; nothing writes it again.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LegacyNamSettings {
    pub enabled: bool,
    /// What this field was called before the blocks: `slot`.
    #[serde(alias = "position")]
    pub slot: NamPosition,
    pub input_db: f32,
    pub output_db: f32,
    pub slim: f32,
    /// Empty means no capture.
    pub model_path: String,
}

impl Default for LegacyNamSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            slot: NamPosition::Pre,
            input_db: 0.0,
            output_db: 0.0,
            slim: 1.0,
            model_path: String::new(),
        }
    }
}

impl LegacyNamSettings {
    /// Whether an old save actually holds a stage worth migrating.
    pub fn is_unset(&self) -> bool {
        self == &Self::default()
    }

    pub fn sanitized(&self) -> Self {
        let controls = self.controls().sanitized();
        Self {
            enabled: controls.enabled,
            slot: controls.position,
            input_db: controls.input_db,
            output_db: controls.output_db,
            slim: controls.slim,
            model_path: self.model_path.trim().to_owned(),
        }
    }

    fn controls(&self) -> NamBlockControls {
        NamBlockControls {
            enabled: self.enabled,
            position: self.slot,
            input_db: self.input_db,
            output_db: self.output_db,
            slim: self.slim,
            ..NamBlockControls::default()
        }
    }
}

/// One `.nam` in a models folder, as the sheet lists it.  Built on the UI
/// thread by [`scan_nam_folder`]; the runtime [`nam_rs::Model`] is not.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct NamCatalogEntry {
    pub path: String,
    pub name: String,
    /// The file's `architecture`, e.g. `WaveNet`.
    pub arch: String,
    /// The rate the capture was made at, Hz.
    pub file_rate: u32,
    /// A capture whose width can be dialled (SlimmableContainer).
    pub slimmable: bool,
    /// The estimated share of one block it takes, before it is measured.
    pub load: NamLoad,
}

/// Lists `*.nam` files directly in `folder`.  Reads and parses each file for
/// its display name and architecture.  UI thread only: this does file I/O.
pub(crate) fn scan_nam_folder(folder: &str) -> Vec<NamCatalogEntry> {
    let folder = folder.trim();
    if folder.is_empty() {
        return Vec::new();
    }
    let Ok(dir) = std::fs::read_dir(folder) else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    for item in dir.flatten() {
        let path = item.path();
        let nam = path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("nam"));
        if !nam || !path.is_file() {
            continue;
        }
        let Ok(model) = nam_rs::NamModel::from_file(&path) else {
            continue;
        };
        let load = estimate_load(&model.architecture, model.expected_sample_rate());
        entries.push(NamCatalogEntry {
            path: path.to_string_lossy().into_owned(),
            name: display_name(&model, &path),
            arch: model.architecture.clone(),
            file_rate: model.expected_sample_rate().round() as u32,
            slimmable: matches!(model.config, nam_rs::ModelConfig::Slimmable(_)),
            load,
        });
    }
    entries.sort_by_key(|entry| entry.name.to_lowercase());
    entries
}

fn file_stem(path: &std::path::Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .filter(|stem| !stem.trim().is_empty())
        .unwrap_or_else(|| "Capture".to_owned())
}

fn display_name(model: &nam_rs::NamModel, path: &std::path::Path) -> String {
    model
        .metadata_typed()
        .name
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| file_stem(path))
}

fn short_error(error: &nam_rs::Error) -> String {
    match error {
        nam_rs::Error::Io(_) => "Couldn't read the capture.".to_owned(),
        nam_rs::Error::Json(_) => "This file is not a NAM capture.".to_owned(),
        nam_rs::Error::UnsupportedArchitecture(_) => {
            "This NAM architecture is not supported.".to_owned()
        }
        nam_rs::Error::UnsupportedActivation(_) => {
            "This capture uses an unsupported activation.".to_owned()
        }
        nam_rs::Error::UnsupportedFeature(_) => {
            "This capture uses an unsupported feature.".to_owned()
        }
        nam_rs::Error::WeightCountMismatch { .. } => {
            "This capture's weights do not match its config.".to_owned()
        }
        nam_rs::Error::ConfigTooLarge => "This capture is not usable.".to_owned(),
    }
}

/// What loading one capture produced.  [`NamFile::Ready`] is the only
/// variant that holds a runtime model, and it has already been prewarmed.
// The big variant is the finished capture; this crosses a channel to the UI
// thread once per load, never a per-block path.
#[allow(clippy::large_enum_variant)]
pub(crate) enum NamFile {
    Ready(LoadedNam),
    /// The file parsed, and its rate is not `rate`.  Nothing was installed.
    RateMismatch {
        file_rate: u32,
        name: String,
    },
    Failed(String),
}

/// What one capture costs, as a share of the block period: `peak` is the
/// 95th percentile of the measured buffers and `mean` the average of the
/// buffers up to it (see [`load_from_times`]).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct NamLoad {
    pub mean: f32,
    pub peak: f32,
}

impl NamLoad {
    /// The share of the block the block is expected to take: the mean, and
    /// at least the peak, so a projection does not flatter a model.
    pub(crate) fn share(self) -> f32 {
        self.peak.max(self.mean)
    }
}

/// A model the UI may queue.  `slim_steps` are the width thresholds of a
/// slimmable capture (empty otherwise), for [`slim_submodel`].
pub(crate) struct LoadedNam {
    pub prepared: PreparedNam,
    pub name: String,
    pub slim_steps: Vec<f32>,
    pub file_rate: u32,
    /// `metadata.gear_type`, trimmed.  Empty becomes [`None`].  UI state
    /// only: the audio thread never reads it.
    pub gear_type: Option<String>,
    /// The file's `architecture`.
    pub arch: String,
    /// Calibration levels of the capture, dBu at 0 dBFS, when the file has
    /// them.
    pub level_in_dbu: Option<f32>,
    pub level_out_dbu: Option<f32>,
    /// What the model measured as at this rate and block size.  `None` when
    /// the caller asked for no measurement.
    pub load: Option<NamLoad>,
    /// Whether every submodel is a WaveNet whose time constants a doubled
    /// rate scales.  The UI counts these to decide a group's factor.
    pub oversample: bool,
}

/// The submodel a slimmable capture runs at width `slim`: the first whose
/// threshold is above it, else the last (NAM Core's `SetSlimmableSize`).
/// 0 for a capture with no steps.
pub(crate) fn slim_submodel(steps: &[f32], slim: f32) -> usize {
    steps
        .iter()
        .position(|&max| max > slim)
        .unwrap_or(steps.len().saturating_sub(1))
}

/// A runtime model the audio thread may take.  Built only on the loader or
/// UI thread.
pub(crate) struct PreparedNam {
    pub model: nam_rs::Model,
    /// The file's rate.  A model is only built when this is the output rate;
    /// the factor is not a rate and never goes here.
    pub rate: u32,
    /// 1, or 2 for a model built with every layer dilation doubled for an
    /// oversampled group.  The slot runs it through the 2x pair.
    pub factor: u8,
}

/// What the UI queues for the next block.  `Clear` drops the live model out
/// of the processor without the callback destroying it.  Both name the block
/// they are for.
pub(crate) enum NamCommand {
    Install(PreparedNam),
    Clear,
}

/// Parses `json`, and builds a model only when its sample rate is `rate`.
/// UI thread only.  Tests build a capture without a file.
#[cfg(test)]
pub(crate) fn prepare_nam_json(json: &str, rate: u32, slim: f32) -> NamFile {
    prepare_nam_json_at(json, rate, slim, 1)
}

/// [`prepare_nam_json`] at an explicit oversampling factor.
#[cfg(test)]
pub(crate) fn prepare_nam_json_at(json: &str, rate: u32, slim: f32, factor: u8) -> NamFile {
    let model = match nam_rs::NamModel::from_json_str(json) {
        Ok(model) => model,
        Err(error) => return NamFile::Failed(short_error(&error)),
    };
    prepare_parsed(model, "Capture", rate, slim, None, factor)
}

/// Reads `path` and builds a model only when its sample rate is `rate`.
/// The loader thread only: this does file I/O and allocates the runtime.
/// `frames` is the block size to measure the model at; `None` skips the
/// measurement.  `factor` 2 builds the dilated copy an oversampled group
/// runs; a capture that cannot be oversampled stays at 1.
pub(crate) fn prepare_nam_file(
    path: &str,
    rate: u32,
    slim: f32,
    frames: Option<usize>,
    factor: u8,
) -> NamFile {
    let file = std::path::Path::new(path);
    let model = match nam_rs::NamModel::from_file(file) {
        Ok(model) => model,
        Err(error) => return NamFile::Failed(short_error(&error)),
    };
    let name = display_name(&model, file);
    prepare_parsed(model, &name, rate, slim, frames, factor)
}

fn prepare_parsed(
    parsed: nam_rs::NamModel,
    name: &str,
    rate: u32,
    slim: f32,
    frames: Option<usize>,
    factor: u8,
) -> NamFile {
    let file_rate = parsed.expected_sample_rate().round() as u32;
    if !rates_match(parsed.expected_sample_rate(), rate) {
        return NamFile::RateMismatch {
            file_rate,
            name: name.to_owned(),
        };
    }
    let slim_steps = match &parsed.config {
        nam_rs::ModelConfig::Slimmable(config) => config
            .submodels
            .iter()
            .map(|submodel| submodel.max_value)
            .collect(),
        _ => Vec::new(),
    };
    let metadata = parsed.metadata_typed();
    let oversample = can_oversample(&parsed);
    let factor = if factor >= 2 && oversample { 2 } else { 1 };
    let scaled = (factor == 2).then(|| oversampled(&parsed));
    let source = scaled.as_ref().unwrap_or(&parsed);
    let mut model = match nam_rs::Model::from_nam(source) {
        Ok(model) => model,
        Err(error) => return NamFile::Failed(short_error(&error)),
    };
    model.reset();
    if let Some(container) = model.as_slimmable_mut() {
        container.set_slim_size(slim.clamp(0.0, 1.0));
    }
    warm_up(&mut model);
    let load = frames.map(|frames| measure_load(&mut model, rate, frames, factor));
    let gear_type = metadata.gear_type.and_then(|gear| {
        let gear = gear.trim();
        if gear.is_empty() {
            None
        } else {
            Some(gear.to_owned())
        }
    });
    NamFile::Ready(LoadedNam {
        prepared: PreparedNam {
            model,
            rate,
            factor,
        },
        name: name.to_owned(),
        slim_steps,
        file_rate,
        gear_type,
        arch: parsed.architecture.clone(),
        level_in_dbu: finite_level(metadata.input_level_dbu),
        level_out_dbu: finite_level(metadata.output_level_dbu),
        load,
        oversample,
    })
}

/// Whether a capture's time constants scale with a doubled rate, so a
/// dilated copy of it plays the same tone at twice the samples.
///
/// Only a WaveNet can be scaled.  Every layer array's dilations (and the
/// dilation of a nested `condition_dsp`) are the model's time constants.
/// A conv head — a `head_kernel_size` above 1, or a post-stack head with a
/// kernel above 1 — is a dilation-1 convolution inside `nam-rs`, so scaling
/// the layers alone would leave it at half its time; such a capture stays
/// at 1x.  A `SlimmableContainer` is eligible only when every submodel is.
/// An LSTM recurs in samples and has no dilation to scale.
pub(crate) fn can_oversample(parsed: &nam_rs::NamModel) -> bool {
    match &parsed.config {
        nam_rs::ModelConfig::WaveNet(config) => {
            let head_ok = config
                .post_stack_head
                .as_ref()
                .is_none_or(|head| head.kernel_sizes.iter().all(|&kernel| kernel <= 1));
            head_ok
                && config.layers.iter().all(|layer| {
                    layer.head_kernel_size <= 1
                        && layer
                            .dilations
                            .iter()
                            .all(|&dilation| dilation.checked_mul(2).is_some())
                })
                && config.condition_dsp.as_deref().is_none_or(can_oversample)
        }
        nam_rs::ModelConfig::Lstm(_) => false,
        nam_rs::ModelConfig::Slimmable(config) => {
            !config.submodels.is_empty()
                && config
                    .submodels
                    .iter()
                    .all(|submodel| can_oversample(&submodel.model))
        }
    }
}

/// `parsed` with every layer dilation doubled, recursively.  The weight blob
/// is left alone: a convolution's weight count depends on its kernel size,
/// not its dilation, so `Model::from_nam` still builds.  Call it only for a
/// capture [`can_oversample`] accepts.
fn oversampled(parsed: &nam_rs::NamModel) -> nam_rs::NamModel {
    let mut scaled = parsed.clone();
    double_dilations(&mut scaled);
    scaled
}

fn double_dilations(model: &mut nam_rs::NamModel) {
    match &mut model.config {
        nam_rs::ModelConfig::WaveNet(config) => {
            for layer in &mut config.layers {
                for dilation in &mut layer.dilations {
                    *dilation = dilation.saturating_mul(2);
                }
            }
            if let Some(nested) = config.condition_dsp.as_deref_mut() {
                double_dilations(nested);
            }
        }
        nam_rs::ModelConfig::Slimmable(config) => {
            for submodel in &mut config.submodels {
                double_dilations(&mut submodel.model);
            }
        }
        nam_rs::ModelConfig::Lstm(_) => {}
    }
}

/// Runs the model's receptive field of silence through it, so a live model
/// does not start from an empty history.
fn warm_up(model: &mut nam_rs::Model) {
    let warmup = model.receptive_field();
    if warmup > 0 {
        let mut silence = vec![0.0; warmup];
        model.process_buffer(&mut silence);
    }
}

/// How many buffers [`measure_load`] times, and how many it runs first
/// without timing (the first buffers pay for cache misses the steady state
/// does not).
const LOAD_BUFFERS: usize = 200;
const LOAD_WARMUP: usize = 8;
/// The least time [`measure_load`] runs the model before it starts timing.
/// The loader thread wakes for the job on a core that may be at its idle
/// clock; timed from the first buffer, a capture read up to twice its cost.
const LOAD_WARMUP_SECONDS: f64 = 0.05;
/// The percentile of the timed buffers that is the measured peak.  The
/// loader is not a realtime thread: the longest buffer is often a
/// preemption, not the model.
const LOAD_PEAK_PERCENTILE: usize = 95;

/// Runs a quiet sine through `model`, at least [`LOAD_WARMUP`] buffers and
/// [`LOAD_WARMUP_SECONDS`] untimed, then [`LOAD_BUFFERS`] timed, and returns
/// their 95th percentile and the mean below it as a share of the block
/// period
/// ([`load_from_times`]).  The loader thread only: this is the one place a
/// model is timed.  The model is left reset and prewarmed, so it can go live
/// as it is.
///
/// A factor-2 model runs `2 * frames` samples per output block, so it is
/// timed on that many samples against the same block period: the sheet then
/// shows the share it really takes at twice the rate.
pub(crate) fn measure_load(
    model: &mut nam_rs::Model,
    rate: u32,
    frames: usize,
    factor: u8,
) -> NamLoad {
    let frames = frames.max(1);
    let rate = rate.max(1);
    let samples = frames.saturating_mul(factor.max(1) as usize);
    let block_seconds = frames as f64 / rate as f64;
    let mut buffer: Vec<f32> = (0..samples)
        .map(|index| {
            let phase = index as f32 * 2.0 * std::f32::consts::PI * 220.0 / rate as f32;
            0.1 * phase.sin()
        })
        .collect();
    let warm_until =
        std::time::Instant::now() + std::time::Duration::from_secs_f64(LOAD_WARMUP_SECONDS);
    let mut warm = 0;
    while warm < LOAD_WARMUP || std::time::Instant::now() < warm_until {
        model.process_buffer(&mut buffer);
        warm += 1;
    }
    let mut times = [0.0f64; LOAD_BUFFERS];
    for time in &mut times {
        let started = std::time::Instant::now();
        model.process_buffer(&mut buffer);
        *time = started.elapsed().as_secs_f64();
    }
    model.reset();
    warm_up(model);
    load_from_times(&mut times, block_seconds)
}

/// The [`LOAD_PEAK_PERCENTILE`]th percentile of `times` (seconds per
/// buffer, nearest rank) and the mean of the buffers up to it, as shares of
/// `block_seconds`.  The slowest buffers above the percentile are left out
/// of both: on the loader thread they are preemptions, which the realtime
/// audio thread does not see.  Sorts `times`.
fn load_from_times(times: &mut [f64], block_seconds: f64) -> NamLoad {
    if times.is_empty() || block_seconds <= 0.0 {
        return NamLoad::default();
    }
    times.sort_by(f64::total_cmp);
    let rank = (times.len() * LOAD_PEAK_PERCENTILE).div_ceil(100).max(1);
    let kept = &times[..rank];
    let mean = kept.iter().sum::<f64>() / kept.len() as f64;
    let peak = kept[kept.len() - 1];
    NamLoad {
        mean: (mean / block_seconds) as f32,
        peak: (peak / block_seconds) as f32,
    }
}

/// The share of one block a capture will take before it is measured.  The
/// reference machine measured a standard WaveNet at about 12 % at 48 kHz
/// before the vendored nam-rs kernels; they made it 2.6 times faster and a
/// standard LSTM 3.5 times (7 % before), so about 5 % and 2 % there.  A
/// slower machine needs more; `measure_load` replaces this once the model
/// is loaded.  A model's cost is per sample, so its share of the block
/// follows the rate and not the buffer size.
pub(crate) fn estimate_load(arch: &str, rate: f64) -> NamLoad {
    let base = match arch {
        "LSTM" => 0.02,
        "SlimmableContainer" => 0.05,
        // WaveNet, and anything this build does not know: the safe guess.
        _ => 0.05,
    };
    let rate = if rate.is_finite() && rate > 0.0 {
        rate
    } else {
        48_000.0
    };
    let share = base * (rate as f32 / 48_000.0);
    NamLoad {
        mean: share,
        peak: share,
    }
}

/// How many models the park can hold without allocating.  The callback
/// refuses to grow it.  A swap that does not fit stays in the processor
/// until [`NamPark::flush`] has made room.
const PARK_SLOTS: usize = 2 * MAX_NAM_BLOCKS;

/// Models the audio side has let go of.  The UI thread is the only place
/// that drops them ([`NamPark::flush`]).
pub(crate) struct NamPark {
    models: Mutex<Vec<nam_rs::Model>>,
}

impl NamPark {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            models: Mutex::new(Vec::with_capacity(PARK_SLOTS)),
        })
    }

    /// Drops every parked model.  Call it from the UI thread.  The queue
    /// keeps its spare capacity, so the next retire does not allocate.
    pub(crate) fn flush(&self) {
        let mut queue = self.models.lock().unwrap_or_else(PoisonError::into_inner);
        let models = std::mem::replace(&mut *queue, Vec::with_capacity(PARK_SLOTS));
        drop(queue);
        drop(models);
    }

    /// How many models wait here, for the tests that watch a retirement.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.models
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Whether a model still fits.  `true` while the queue is busy, so a
    /// caller does not wait for an answer it cannot get.
    pub(crate) fn has_room(&self) -> bool {
        match self.models.try_lock() {
            Ok(queue) => queue.len() < queue.capacity(),
            Err(_) => true,
        }
    }

    /// Moves `model` into the park.  One try, and no allocation: a full
    /// queue or a busy lock hands the model back.  The callback holds it
    /// and tries again next block.
    pub(crate) fn try_retire(&self, model: nam_rs::Model) -> Result<(), nam_rs::Model> {
        match self.models.try_lock() {
            Ok(mut queue) => park_into(&mut queue, model),
            Err(TryLockError::WouldBlock) => Err(model),
            Err(TryLockError::Poisoned(poison)) => park_into(&mut poison.into_inner(), model),
        }
    }

    /// Moves `model` into the park, retrying while the UI holds the lock.
    ///
    /// Not for the realtime callback.  The UI holds this mutex only inside
    /// [`Self::flush`] and does not wait on the caller, so a busy lock
    /// ends.  A full queue cannot grow here: the model is leaked rather
    /// than dropped on this thread or spun on forever.  That is the bridge
    /// thread tearing a run down while the park is already full.
    pub(crate) fn retire(&self, model: nam_rs::Model) {
        let mut waiting = Some(model);
        while let Some(model) = waiting.take() {
            match self.models.try_lock() {
                Ok(mut queue) => match park_into(&mut queue, model) {
                    Ok(()) => return,
                    Err(returned) => {
                        std::mem::forget(returned);
                        return;
                    }
                },
                Err(TryLockError::WouldBlock) => {
                    waiting = Some(model);
                    std::thread::yield_now();
                }
                Err(TryLockError::Poisoned(poison)) => {
                    let mut queue = poison.into_inner();
                    match park_into(&mut queue, model) {
                        Ok(()) => return,
                        Err(returned) => {
                            std::mem::forget(returned);
                            return;
                        }
                    }
                }
            }
        }
    }
}

/// Pushes `model` only when `queue` has spare capacity, so `push` cannot
/// allocate.  `Err` hands it back when the queue is full.
fn park_into(queue: &mut Vec<nam_rs::Model>, model: nam_rs::Model) -> Result<(), nam_rs::Model> {
    if queue.len() == queue.capacity() {
        Err(model)
    } else {
        queue.push(model);
        Ok(())
    }
}

/// How many commands [`NamPending`] holds: one per live block, and as many
/// again for blocks that left the stack before the callback took theirs.
const PENDING_SLOTS: usize = 2 * MAX_NAM_BLOCKS;

/// The models the UI has queued for the next block, and which block each is
/// for.  The UI pushes (and may allocate); the callback only removes, which
/// moves the rest down in place.  One command per block is the most the UI
/// ever queues: a newer state replaces the older one.
#[derive(Default)]
pub(crate) struct NamPending {
    commands: Vec<(u32, NamCommand)>,
}

impl NamPending {
    /// Queues `command` for block `id`, replacing a command that block still
    /// has waiting.  Returns the replaced one, which the caller (the UI
    /// thread) drops.  `live` is the published stack: when the queue is
    /// full, a command for a block that left it goes first.
    pub(crate) fn queue(
        &mut self,
        id: u32,
        command: NamCommand,
        live: &NamStack,
    ) -> Option<NamCommand> {
        if let Some(slot) = self.commands.iter_mut().find(|(queued, _)| *queued == id) {
            return Some(std::mem::replace(&mut slot.1, command));
        }
        if self.commands.len() >= PENDING_SLOTS {
            // More ids wait than a stack can hold, so at least half of them
            // left it.  Only a callback that has not run for a while gets
            // here.  A block that left the stack loses its place; the
            // oldest command is the last resort.
            let stale = self
                .commands
                .iter()
                .position(|(queued, _)| live.index_of(*queued).is_none())
                .unwrap_or(0);
            let evicted = self.commands.remove(stale).1;
            self.commands.push((id, command));
            return Some(evicted);
        }
        self.commands.push((id, command));
        None
    }

    fn len(&self) -> usize {
        self.commands.len()
    }

    fn at(&self, index: usize) -> Option<(u32, &NamCommand)> {
        self.commands.get(index).map(|(id, command)| (*id, command))
    }

    /// Takes the command at `index`.  `remove` does not allocate.
    fn take(&mut self, index: usize) -> Option<(u32, NamCommand)> {
        (index < self.commands.len()).then(|| self.commands.remove(index))
    }

    /// Empties the queue, handing every command back to the caller.
    pub(crate) fn drain(&mut self) -> Vec<(u32, NamCommand)> {
        std::mem::take(&mut self.commands)
    }

    /// Puts a command the callback could not apply back at the end.  One
    /// entry just left the queue, so the `Vec` has room and does not grow.
    fn requeue(&mut self, id: u32, command: NamCommand) {
        self.commands.push((id, command));
    }
}

/// Stopband attenuation the 2x pair is designed for, dB.  The same figure
/// as the clock bridge's resampler.
const OVERSAMPLE_STOPBAND_DB: f64 = 80.0;

/// The most taps the 2x pair designs.  A rate near 44.1 kHz needs about 215;
/// the clamp only ever bites on a rate far below any device's.  513 is 1
/// modulo 4, so a clamped kernel still has an integer output-sample delay.
const OVERSAMPLE_MAX_TAPS: usize = 513;

/// Taps per polyphase half.  One half is one of the two 2x phases; together
/// they are the whole kernel.
const OVERSAMPLE_HALF: usize = OVERSAMPLE_MAX_TAPS.div_ceil(2);

/// The longest delay one block's pair can add, output samples: two filters
/// of `(length - 1) / 4` each.
const OVERSAMPLE_MAX_DELAY: usize = (OVERSAMPLE_MAX_TAPS - 1) / 2;

/// The fixed 2x up/down pair an oversampled block runs through.
///
/// Both directions use the same linear-phase Kaiser-windowed sinc at twice
/// the output rate: passband to 20 kHz (or 0.45 of the output rate when that
/// is lower), stopband from the output Nyquist, 80 dB down.  It is not
/// [`FrameResampler`](super::resampler::FrameResampler): that one's stopband
/// starts at the lower rate minus 20 kHz, so a harmonic between that and the
/// output Nyquist would fold back into the audio band.
///
/// The kernel is odd with `(length - 1)` divisible by 4, so each direction
/// delays by a whole number of output samples and the two together delay by
/// `(length - 1) / 2`.  At 48 kHz that is 60 samples, kernel 121.
///
/// The coefficients are designed in [`Self::retune`], off the sample loop;
/// [`Self::upsample`] and [`Self::downsample`] allocate nothing.  Everything
/// here lives on the heap: `NamStage` sits inline in `OutputMixer`, and a
/// debug stack does not carry its kernels.
struct OversampleFilters {
    /// The two polyphase halves.  `even[0]` multiplies the current input
    /// sample, `odd[0]` the half sample between it and the last one.
    even: Vec<f32>,
    odd: Vec<f32>,
    /// Design scratch, so [`Self::retune`] does not put the f64 kernel on the
    /// stack.  Never read outside that call.
    kernel: Vec<f64>,
    /// Taps per half.
    taps: usize,
    /// Delay of the whole pair, output samples: one filter each way, each
    /// `(length - 1) / 4`.
    delay: usize,
}

impl OversampleFilters {
    fn new(rate: u32) -> Self {
        let mut filters = Self {
            even: vec![0.0; OVERSAMPLE_HALF],
            odd: vec![0.0; OVERSAMPLE_HALF],
            kernel: vec![0.0; OVERSAMPLE_MAX_TAPS],
            taps: 0,
            delay: 0,
        };
        filters.retune(rate);
        filters
    }

    /// Designs the kernel for `rate`.  Called from [`NamStage::new`] and
    /// where the stage recomputes its fade lengths, so never in a sample
    /// loop.  Reuses its buffers, so a rate change allocates nothing.
    fn retune(&mut self, rate: u32) {
        let rate = rate.max(1) as f64;
        let pass = 20_000.0_f64.min(0.45 * rate);
        let stop = 0.5 * rate;
        let cutoff = 0.5 * (pass + stop);
        let transition = (stop - pass).max(1.0);
        let beta = 0.1102 * (OVERSAMPLE_STOPBAND_DB - 8.7);
        // Kaiser's estimate for the taps a window of this attenuation needs
        // across that transition, then up to the next length of 1 mod 4.
        let estimate = (OVERSAMPLE_STOPBAND_DB - 8.0)
            / (2.285 * std::f64::consts::TAU * transition / (2.0 * rate));
        let boot = estimate.ceil().max(0.0) as usize;
        let length = (4 * boot.saturating_sub(1).div_ceil(4) + 1).clamp(5, OVERSAMPLE_MAX_TAPS);
        // `scale` is the cutoff as a fraction of the 2x rate: the ideal
        // lowpass, whose windowed form is normalised to unity DC gain.
        let scale = cutoff / rate;
        let center = (length - 1) as f64 * 0.5;
        let mut sum = 0.0;
        for (index, tap) in self.kernel.iter_mut().enumerate().take(length) {
            let x = index as f64 - center;
            let angle = std::f64::consts::PI * scale * x;
            let sinc = if angle.abs() < 1.0e-12 {
                1.0
            } else {
                angle.sin() / angle
            };
            let window = kaiser_window(beta, x / center);
            *tap = scale * sinc * window;
            sum += *tap;
        }
        let gain = if sum != 0.0 { 1.0 / sum } else { 1.0 };
        let per_half = length.div_ceil(2);
        for index in 0..OVERSAMPLE_HALF {
            self.even[index] = if index < per_half {
                (self.kernel[2 * index] * gain) as f32
            } else {
                0.0
            };
            self.odd[index] = if 2 * index + 1 < length {
                (self.kernel[2 * index + 1] * gain) as f32
            } else {
                0.0
            };
        }
        self.taps = per_half;
        self.delay = (length - 1) / 2;
    }

    /// `N` input samples become `2N` in `wide`, whose first `2N` must be
    /// writable.  The interpolation gain is 2, as the zeroes halve the
    /// average; `history` keeps the last `taps - 1` input samples.
    fn upsample(&self, input: &[f32], wide: &mut [f32], history: &mut [f32]) {
        let frames = input.len();
        debug_assert!(history.len() + 1 == self.taps, "up history length");
        for n in 0..frames {
            let mut even = 0.0f32;
            let mut odd = 0.0f32;
            for j in 0..self.taps {
                let sample = if n >= j {
                    input[n - j]
                } else {
                    history[history.len() + n - j]
                };
                even += self.even[j] * sample;
                odd += self.odd[j] * sample;
            }
            wide[2 * n] = even * 2.0;
            wide[2 * n + 1] = odd * 2.0;
        }
        push_history(history, input);
    }

    /// `2N` samples of `wide` become `N` in `output`.  `history` keeps the
    /// last `taps - 1` input pairs.
    fn downsample(&self, wide: &[f32], output: &mut [f32], history: &mut [f32]) {
        let frames = output.len();
        let pairs = history.len() / 2;
        debug_assert_eq!(history.len(), 2 * (self.taps - 1), "down history length");
        // `even_at(p)` is the sample at 2x index `2p`, `odd_at(p)` the one
        // after it.  A tap `j` pairs `even(n - j)` with `odd(n - j - 1)`,
        // because the kernel is symmetric about a half sample at 2x.
        let even_at = |p: isize| -> f32 {
            if p >= 0 {
                wide[2 * p as usize]
            } else {
                history[2 * (pairs as isize + p) as usize]
            }
        };
        let odd_at = |p: isize| -> f32 {
            if p >= 0 {
                wide[2 * p as usize + 1]
            } else if p >= -(pairs as isize) {
                history[2 * (pairs as isize + p) as usize + 1]
            } else {
                0.0
            }
        };
        for (n, sample) in output.iter_mut().enumerate() {
            let mut sum = 0.0f32;
            for j in 0..self.taps {
                let p = n as isize - j as isize;
                sum += self.even[j] * even_at(p) + self.odd[j] * odd_at(p - 1);
            }
            *sample = sum;
        }
        push_pairs(history, wide, frames);
    }
}

/// Kaiser's zero-order modified Bessel function, by its series.  The window
/// and its normalisation are wanted to a few parts in `1e-12`; the loop ends
/// when the next term is below that.
fn bessel_i0(x: f64) -> f64 {
    let half = 0.5 * x;
    let mut total = 1.0;
    let mut term = 1.0;
    for order in 1..64 {
        term *= (half / order as f64) * (half / order as f64);
        total += term;
        if term < total * 1.0e-12 {
            break;
        }
    }
    total
}

/// Kaiser's window at `position`, in `-1..=1` across the kernel.
fn kaiser_window(beta: f64, position: f64) -> f64 {
    let r = position.clamp(-1.0, 1.0);
    bessel_i0(beta * (1.0 - r * r).max(0.0).sqrt()) / bessel_i0(beta)
}

/// Keeps the last `history.len()` samples of `samples` (or all of them when
/// the block is shorter than the history).
fn push_history(history: &mut [f32], samples: &[f32]) {
    let len = history.len();
    if len == 0 {
        return;
    }
    if samples.len() >= len {
        history.copy_from_slice(&samples[samples.len() - len..]);
    } else {
        history.copy_within(samples.len().., 0);
        history[len - samples.len()..].copy_from_slice(samples);
    }
}

/// [`push_history`] for the downsampler's history of input pairs.
fn push_pairs(history: &mut [f32], wide: &[f32], frames: usize) {
    let pairs = history.len() / 2;
    if pairs == 0 {
        return;
    }
    if frames >= pairs {
        history.copy_from_slice(&wide[2 * (frames - pairs)..2 * frames]);
    } else {
        history.copy_within(2 * frames.., 0);
        history[2 * (pairs - frames)..].copy_from_slice(&wide[..2 * frames]);
    }
}

/// A fixed-length mono delay line.  Length 0 passes its input through and
/// keeps nothing.  The backing store is allocated once, when the slot is
/// built; retuning and resetting only touch its length and contents.
struct MonoDelay {
    samples: Vec<f32>,
    len: usize,
    at: usize,
}

impl MonoDelay {
    fn new(len: usize) -> Self {
        let mut samples = Vec::with_capacity(OVERSAMPLE_MAX_DELAY);
        samples.resize(len.min(OVERSAMPLE_MAX_DELAY), 0.0);
        let len = samples.len();
        Self {
            samples,
            len,
            at: 0,
        }
    }

    /// Changes the delay without allocating, when the new length fits the
    /// capacity the slot was built with.
    fn set_len(&mut self, len: usize) {
        let len = len.min(OVERSAMPLE_MAX_DELAY);
        self.samples.clear();
        self.samples.resize(len, 0.0);
        self.len = len;
        self.at = 0;
    }

    fn reset(&mut self) {
        self.samples.fill(0.0);
        self.at = 0;
    }

    /// Pushes `sample` and returns the one that left the line.
    fn push(&mut self, sample: f32) -> f32 {
        if self.len == 0 {
            return sample;
        }
        let out = self.samples[self.at];
        self.samples[self.at] = sample;
        self.at += 1;
        if self.at == self.len {
            self.at = 0;
        }
        out
    }

    /// Advances the line without reading it.
    fn feed(&mut self, samples: &[f32]) {
        if self.len == 0 {
            return;
        }
        for &sample in samples {
            self.push(sample);
        }
    }
}

/// One side's resampler history: the live model's, or the model fading out.
/// It moves with the model when a swap starts, so a side keeps its filter
/// and alignment history exactly as long as it keeps its history of sound.
struct OversampleState {
    up: Vec<f32>,
    down: Vec<f32>,
    /// The output's delay while the block carries the pair's latency.  Fed
    /// even when the block is not delayed, so a swap that starts one has
    /// the model's past output to place in front of it.
    align: MonoDelay,
}

impl OversampleState {
    fn new(filters: &OversampleFilters) -> Self {
        let mut state = Self {
            up: Vec::with_capacity(OVERSAMPLE_HALF - 1),
            down: Vec::with_capacity(2 * (OVERSAMPLE_HALF - 1)),
            align: MonoDelay::new(filters.delay),
        };
        state.retune(filters);
        state
    }

    fn retune(&mut self, filters: &OversampleFilters) {
        let taps = filters.taps.saturating_sub(1);
        self.up.clear();
        self.up.resize(taps, 0.0);
        self.down.clear();
        self.down.resize(2 * taps, 0.0);
        self.align.set_len(filters.delay);
    }

    fn reset(&mut self) {
        self.up.fill(0.0);
        self.down.fill(0.0);
        self.align.reset();
    }
}

/// One model's pass over one block, either through the 2x pair or as it was
/// captured.  `input` holds the side's mono signal.  A factor-2 side's
/// output is the pair's delayed one, in place.  A factor-1 side's output is
/// the model's own, and when `crossfade` asks for it its delayed copy goes
/// to `aligned`: the mix moves between the two when the time base changes.
#[allow(clippy::too_many_arguments)]
fn run_model(
    model: &mut nam_rs::Model,
    factor: u8,
    filters: &OversampleFilters,
    input: &mut [f32],
    wide: &mut [f32],
    state: &mut OversampleState,
    aligned: &mut [f32],
    crossfade: bool,
) {
    let frames = input.len();
    if factor >= 2 {
        filters.upsample(input, wide, &mut state.up);
        model.process_buffer(&mut wide[..2 * frames]);
        filters.downsample(&wide[..2 * frames], input, &mut state.down);
    } else {
        model.process_buffer(input);
        if crossfade {
            for (out, &sample) in aligned.iter_mut().zip(input.iter()) {
                *out = state.align.push(sample);
            }
        } else {
            state.align.feed(input);
        }
    }
}

/// The buffers one slot's block may use, shared by every block of a path
/// because they run one after the other.  The first three are twice a block;
/// the aligned copies are one block each, one per side.
struct NamScratch<'a> {
    live: &'a mut [f32],
    outgoing: &'a mut [f32],
    wide: &'a mut [f32],
    live_aligned: &'a mut [f32],
    out_aligned: &'a mut [f32],
}

/// One block's runtime: the model it runs, the one it fades out of, and the
/// gains that take the block from one to the other.  Kept with its id, so a
/// reorder moves nothing.
///
/// Every slot is built with the stage and reused in place.  A released slot
/// keeps its buffers for the next block, so the callback never frees one.
struct NamSlot {
    id: u32,
    /// The slot holds a block.  A released slot waits for the next id.
    live: bool,
    model: Option<nam_rs::Model>,
    /// The model fading out during a swap or a clear.  `None` while a swap
    /// fades in from the dry signal.  Never overwritten: the next swap or
    /// clear waits until this one is parked.
    outgoing: Option<nam_rs::Model>,
    /// A model [`NamPark::try_retire`] could not take.  Held until a later
    /// block.  Two slots: the one already waiting, and the one just replaced.
    held: [Option<nam_rs::Model>; 2],
    /// The rate `model` was built for.  0 when it must not run: a restart
    /// could not park it, and the next block keeps trying.
    model_rate: u32,
    controls: NamBlockControls,
    adopted: bool,
    input_gain: Ramp,
    output_gain: Ramp,
    /// The level match this block applies, from [`NamStack::level_matches`].
    level_gain: Ramp,
    /// Model against dry: 0 is bypass.
    wet: Ramp,
    wet_target: f32,
    /// New side against old side of a swap: 1 when no swap runs.
    swap: Ramp,
    /// Samples `model` still runs unheard before it fades in.  Model samples:
    /// a factor-2 block runs two per output frame.
    warm: usize,
    /// Leaving bypass waits for [`Self::warm`] before the wet fade starts.
    enabling: bool,
    /// The block left the published stack.  It keeps running at its old
    /// place in [`NamRun`] while `wet` fades to 0, and is released after.
    departing: bool,
    /// The dilation scale the live model was built with: 1, or 2 when its
    /// group is oversampled.  The UI decided it when it queued the model.
    factor: u8,
    /// The same, for the model fading out.
    out_factor: u8,
    /// The block's output, wet and dry alike, carries [`OversampleFilters`]'
    /// delay this block: it runs at factor 2, or a fade to or from one has
    /// not settled yet.
    delayed: bool,
    /// 0 is the direct time base, 1 the pair's full delay.  It moves between
    /// them over the bypass fade when [`Self::delayed`] changes, so the
    /// output never jumps by the delay in one sample.
    delay_mix: Ramp,
    /// The input's delay, one line per channel, so a bypassed or fading
    /// block plays dry at the same place as its wet side.  Fed every block
    /// the slot runs, even while it does not delay, so the change of time
    /// base has history to crossfade into.
    dry_l: MonoDelay,
    dry_r: MonoDelay,
    /// The resampler and alignment history of each side.  The live model's
    /// state moves to `out` when a swap starts, so a fading model keeps the
    /// history it was running with.
    live_state: OversampleState,
    out_state: OversampleState,
}

impl NamSlot {
    fn new(id: u32, filters: &OversampleFilters) -> Self {
        Self {
            id,
            live: false,
            model: None,
            outgoing: None,
            held: [None, None],
            model_rate: 0,
            controls: NamBlockControls {
                id,
                ..NamBlockControls::default()
            },
            adopted: false,
            input_gain: Ramp::new(1.0),
            output_gain: Ramp::new(1.0),
            level_gain: Ramp::new(1.0),
            wet: Ramp::new(0.0),
            wet_target: 0.0,
            swap: Ramp::new(1.0),
            warm: 0,
            enabling: false,
            departing: false,
            factor: 1,
            out_factor: 1,
            delayed: false,
            delay_mix: Ramp::new(0.0),
            dry_l: MonoDelay::new(filters.delay),
            dry_r: MonoDelay::new(filters.delay),
            live_state: OversampleState::new(filters),
            out_state: OversampleState::new(filters),
        }
    }

    /// Takes an id and starts over, keeping the buffers.  Only a released
    /// slot is reused, so it holds no model.
    fn reset(&mut self, id: u32) {
        debug_assert!(
            self.model.is_none()
                && self.outgoing.is_none()
                && self.held.iter().all(Option::is_none),
            "a reused slot still holds a model"
        );
        self.id = id;
        self.live = true;
        self.model = None;
        self.outgoing = None;
        self.model_rate = 0;
        self.controls = NamBlockControls {
            id,
            ..NamBlockControls::default()
        };
        self.adopted = false;
        self.input_gain = Ramp::new(1.0);
        self.output_gain = Ramp::new(1.0);
        self.level_gain = Ramp::new(1.0);
        self.wet = Ramp::new(0.0);
        self.wet_target = 0.0;
        self.swap = Ramp::new(1.0);
        self.warm = 0;
        self.enabling = false;
        self.departing = false;
        self.factor = 1;
        self.out_factor = 1;
        self.delayed = false;
        self.delay_mix = Ramp::new(0.0);
        self.dry_l.reset();
        self.dry_r.reset();
        self.live_state.reset();
        self.out_state.reset();
    }

    /// Rebuilds the resampler history for a new filter length.  A rate
    /// change; every model restarts, so nothing here is heard through it.
    fn retune(&mut self, filters: &OversampleFilters) {
        self.dry_l.set_len(filters.delay);
        self.dry_r.set_len(filters.delay);
        self.live_state.retune(filters);
        self.out_state.retune(filters);
    }

    /// Drops the model and its fades, for a block that starts over: a new
    /// rate, a reactivated path.  Kept out of `Drop` because parking can
    /// fail and the block is retried.
    fn restart(&mut self, park: Option<&Arc<NamPark>>) {
        if !self.retire_active(park) {
            self.model_rate = 0;
        }
        self.retire_outgoing(park);
        self.adopted = false;
        self.input_gain = Ramp::new(1.0);
        self.output_gain = Ramp::new(1.0);
        self.level_gain = Ramp::new(1.0);
        self.wet = Ramp::new(0.0);
        self.wet_target = 0.0;
        self.warm = 0;
        self.enabling = false;
        self.factor = 1;
        self.out_factor = 1;
        self.delayed = false;
        self.delay_mix = Ramp::new(0.0);
        self.dry_l.reset();
        self.dry_r.reset();
        self.live_state.reset();
        self.out_state.reset();
    }

    fn model_runs(&self) -> bool {
        self.model.is_some()
            && self.model_rate != 0
            && (self.enabling || self.wet_target > 0.0 || !self.wet.is_settled_at(0.0))
    }

    /// Whether the model is heard now or about to be: a swap must fade.
    fn audible(&self) -> bool {
        self.wet_target > 0.0 || !self.wet.is_settled_at(0.0)
    }

    /// How long a model runs unheard before it fades in.
    fn warmup(&self) -> usize {
        self.model
            .as_ref()
            .map_or(0, nam_rs::Model::receptive_field)
    }

    /// Whether an install or a clear may be applied now without parking
    /// more than the block can hold.  One swap at a time: a command waits
    /// while a model still fades out, or is not parked yet, so `outgoing`
    /// is never overwritten.  An audible block also waits for a fade in
    /// from dry to end, which a new swap would cut.
    fn ready(&self, park_room: bool) -> bool {
        if self.outgoing.is_some() {
            return false;
        }
        if self.audible() {
            self.swap.is_settled_at(1.0)
        } else {
            self.model.is_none() || self.has_free_held() || park_room
        }
    }

    /// Whether the block has a model it will play, for the level match.
    fn has_live_model(&self) -> bool {
        !self.departing && self.model.is_some() && self.model_rate != 0
    }

    /// Starts the fade to dry of a block that left the stack.
    fn depart(&mut self, xfade: f32) {
        self.departing = true;
        self.enabling = false;
        self.wet_target = 0.0;
        self.wet.set_target(0.0, xfade.max(1.0));
    }

    fn has_free_held(&self) -> bool {
        self.held.iter().any(Option::is_none)
    }

    fn accept(&mut self, prepared: PreparedNam) {
        self.model_rate = prepared.rate;
        self.factor = if prepared.factor >= 2 { 2 } else { 1 };
        self.model = Some(prepared.model);
        if self.factor == 2 {
            // The pair delays the wet side from its first sample.  Report and
            // apply that from the start of the fade.
            self.delayed = true;
        }
    }

    /// Installs a prepared model: crossfades from the one it replaces when
    /// that one is heard, else swaps with a fade-in.  `None` when the model
    /// it would replace cannot be parked yet, or a model still fades out.
    fn install(
        &mut self,
        prepared: PreparedNam,
        park: Option<&Arc<NamPark>>,
    ) -> Option<NamCommand> {
        if self.outgoing.is_some() {
            return Some(NamCommand::Install(prepared));
        }
        if self.audible() {
            self.outgoing = self.model.take();
            self.out_factor = self.factor;
            self.swap.snap(0.0);
            // The state follows the model that fades out.
            std::mem::swap(&mut self.live_state, &mut self.out_state);
        } else if !self.retire_active(park) {
            return Some(NamCommand::Install(prepared));
        } else {
            self.live_state.reset();
            if prepared.factor < 2 {
                // Nothing fades, so a block that is no longer oversampled
                // drops the pair's delay at once.
                self.delayed = false;
            }
        }
        self.accept(prepared);
        self.warm = self.warmup();
        None
    }

    /// Fades the model out (or parks it when it is not heard).  Waits, like
    /// [`Self::install`], while a model still fades out.
    fn clear(&mut self, park: Option<&Arc<NamPark>>) -> Option<NamCommand> {
        if self.outgoing.is_some() {
            return Some(NamCommand::Clear);
        }
        if self.audible() {
            if let Some(model) = self.model.take() {
                self.outgoing = Some(model);
                self.out_factor = self.factor;
                self.swap.snap(0.0);
            }
            std::mem::swap(&mut self.live_state, &mut self.out_state);
            self.live_state.reset();
            self.model_rate = 0;
            self.factor = 1;
            self.warm = 0;
        } else if !self.retire_active(park) {
            return Some(NamCommand::Clear);
        } else {
            self.live_state.reset();
            self.factor = 1;
            self.delayed = false;
        }
        self.wet_target = 0.0;
        self.enabling = false;
        None
    }

    /// Moves the live model into the park (or a held slot).  `false` when
    /// both are busy and the model is still here.
    fn retire_active(&mut self, park: Option<&Arc<NamPark>>) -> bool {
        let Some(model) = self.model.take() else {
            self.model_rate = 0;
            return true;
        };
        match self.park_model(model, park) {
            Ok(()) => {
                self.model_rate = 0;
                true
            }
            Err(model) => {
                self.model = Some(model);
                false
            }
        }
    }

    /// Parks the model a swap faded out.  One that cannot be parked yet
    /// stays, silent, until a later block.
    fn retire_outgoing(&mut self, park: Option<&Arc<NamPark>>) {
        if let Some(model) = self.outgoing.take() {
            if let Err(model) = self.park_model(model, park) {
                self.outgoing = Some(model);
            }
        }
        self.swap.snap(1.0);
        if self.factor == 1 {
            // The fade back to 1x has settled: both sides are the unscaled
            // model now, and the block stops delaying its output.
            self.delayed = false;
        }
    }

    fn drain_held(&mut self, park: Option<&Arc<NamPark>>) {
        let Some(park) = park else {
            return;
        };
        for slot in &mut self.held {
            let Some(model) = slot.take() else {
                continue;
            };
            if let Err(model) = park.try_retire(model) {
                *slot = Some(model);
            }
        }
    }

    /// Moves models the block no longer runs into the park.  `true` when
    /// nothing is left here, so the block may be dropped.
    fn release(&mut self, park: Option<&Arc<NamPark>>) -> bool {
        self.drain_held(park);
        let active = self.retire_active(park);
        self.retire_outgoing(park);
        active && self.outgoing.is_none() && self.held.iter().all(Option::is_none)
    }

    /// Moves `model` into the park, or into a held slot.  `Err` returns the
    /// model when both are busy: the caller still owns it, and it is not
    /// dropped.
    fn park_model(
        &mut self,
        model: nam_rs::Model,
        park: Option<&Arc<NamPark>>,
    ) -> Result<(), nam_rs::Model> {
        if let Some(park) = park {
            match park.try_retire(model) {
                Ok(()) => return Ok(()),
                Err(model) => {
                    if let Some(slot) = self.held.iter_mut().find(|slot| slot.is_none()) {
                        *slot = Some(model);
                        return Ok(());
                    }
                    return Err(model);
                }
            }
        }
        // No park yet: this stage has not been attached to a run.  The
        // caller is the owner thread (a unit test).  The model waits in a
        // held slot and drops with the stage.
        if let Some(slot) = self.held.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(model);
            Ok(())
        } else {
            Err(model)
        }
    }

    /// Follows the published controls: trims, the level match, and the
    /// bypass fade with its warm-up.
    fn follow(
        &mut self,
        controls: &NamBlockControls,
        level_db: f32,
        ramp_samples: f32,
        xfade: f32,
    ) {
        self.controls = *controls;
        let input = db_to_gain(controls.input_db);
        let output = db_to_gain(controls.output_db);
        let level = db_to_gain(level_db);
        if !self.adopted {
            self.input_gain.snap(input);
            self.output_gain.snap(output);
            self.level_gain.snap(level);
            // A model taken this block has not heard the signal yet.
            let cold = controls.enabled && self.warm > 0;
            let wet = if controls.enabled && !cold { 1.0 } else { 0.0 };
            self.wet.snap(wet);
            self.wet_target = wet;
            self.enabling = cold;
            self.adopted = true;
            return;
        }
        self.input_gain.set_target(input, ramp_samples);
        self.output_gain.set_target(output, ramp_samples);
        self.level_gain.set_target(level, ramp_samples);
        if controls.enabled {
            if self.wet_target == 0.0 && !self.enabling {
                if self.model_runs() {
                    // Still fading out: its history is the live signal.
                    self.wet_target = 1.0;
                    self.wet.set_target(1.0, xfade.max(1.0));
                } else if self.model.is_some() {
                    self.enabling = true;
                    self.warm = self.warmup();
                }
            }
        } else {
            self.enabling = false;
            if self.wet_target != 0.0 {
                self.wet_target = 0.0;
                self.wet.set_target(0.0, xfade.max(1.0));
            }
        }
    }

    /// One block of one model, in place on channels 0 and 1.  No allocation.
    /// The stage's buffers are shared: blocks run one after the other, so one
    /// set serves them all.
    ///
    /// The block's input becomes the model's mono signal before the block's
    /// own channels are delayed, so a factor-2 model hears the undelayed
    /// signal and the pair contributes the delay, not the input.  When the
    /// time base moves, the dry and any factor-1 side mix the direct and
    /// delayed copies with [`NamSlot::delay_mix`], so no sample jumps by the
    /// pair's delay.
    fn process_block(
        &mut self,
        block: &mut [[f32; 4]],
        frames: usize,
        scratch: &mut NamScratch,
        filters: &OversampleFilters,
    ) {
        let frames = frames
            .min(block.len())
            .min(scratch.live.len() / 2)
            .min(scratch.outgoing.len() / 2)
            .min(scratch.wide.len() / 2)
            .min(scratch.live_aligned.len())
            .min(scratch.out_aligned.len());
        let run = self.model_runs();
        let fade_out = self.outgoing.is_some() && !self.swap.is_settled_at(1.0);
        if !run
            && !fade_out
            && self.wet.is_settled_at(0.0)
            && self.model.is_none()
            && self.outgoing.is_none()
            && !self.delayed
        {
            return;
        }
        // True while the time base is not settled on "no delay": both copies
        // are needed for the mix below.
        let sweep = !self.delay_mix.is_settled_at(0.0);
        for (index, frame) in block.iter().enumerate().take(frames) {
            let input =
                (frame[0] + frame[1]) * 0.5 * self.input_gain.next() * self.level_gain.next();
            scratch.live[index] = input;
            scratch.outgoing[index] = input;
        }
        if run {
            let mut warm = frames;
            if let Some(model) = self.model.as_mut() {
                if self.factor >= 2 {
                    warm = 2 * frames;
                }
                run_model(
                    model,
                    self.factor,
                    filters,
                    &mut scratch.live[..frames],
                    scratch.wide,
                    &mut self.live_state,
                    scratch.live_aligned,
                    sweep && self.factor < 2,
                );
            }
            self.warm = self.warm.saturating_sub(warm);
        }
        if fade_out {
            if let Some(model) = self.outgoing.as_mut() {
                run_model(
                    model,
                    self.out_factor,
                    filters,
                    &mut scratch.outgoing[..frames],
                    scratch.wide,
                    &mut self.out_state,
                    scratch.out_aligned,
                    sweep && self.out_factor < 2,
                );
            }
        }
        let new_side = run && self.model.is_some();
        let new_aligned = new_side && self.factor < 2 && sweep;
        let old_aligned = fade_out && self.out_factor < 2 && sweep;
        for (index, frame) in block.iter_mut().enumerate().take(frames) {
            let gain = self.output_gain.next();
            let swap = self.swap.next();
            let mix = self.wet.next();
            let time = self.delay_mix.next();
            let direct = [frame[0], frame[1]];
            // The lines always run, so a block that starts or stops
            // delaying has the input history to crossfade through.
            let delayed = [self.dry_l.push(direct[0]), self.dry_r.push(direct[1])];
            for (channel, sample) in frame.iter_mut().take(2).enumerate() {
                let dry = direct[channel] + (delayed[channel] - direct[channel]) * time;
                let new = if new_side {
                    let value = if new_aligned {
                        let direct = scratch.live[index];
                        direct + (scratch.live_aligned[index] - direct) * time
                    } else {
                        scratch.live[index]
                    };
                    value * gain
                } else {
                    dry
                };
                let old = if fade_out {
                    let value = if old_aligned {
                        let direct = scratch.outgoing[index];
                        direct + (scratch.out_aligned[index] - direct) * time
                    } else {
                        scratch.outgoing[index]
                    };
                    value * gain
                } else {
                    dry
                };
                let wet = old + (new - old) * swap;
                *sample = dry + (wet - dry) * mix;
            }
        }
    }

    /// Installs a prepared model directly, without a fade or a warm-up.
    /// Tests only; the caller is the owner thread.
    #[cfg(test)]
    fn install_prepared(&mut self, prepared: PreparedNam) {
        self.accept(prepared);
        self.warm = 0;
        self.live_state.reset();
    }
}

/// How many blocks the stage keeps runtime state for: the published stack,
/// and as many again that left it and still fade out.
const RUN_SLOTS: usize = 2 * MAX_NAM_BLOCKS;

/// The order the stage runs its blocks in, by id and group: the published
/// stack, with the blocks that left it still at their old place.  [`Copy`]
/// and fixed-size, so the callback builds one without allocating.
#[derive(Clone, Copy)]
struct NamRun {
    entries: [(u32, NamPosition); RUN_SLOTS],
    count: usize,
}

impl Default for NamRun {
    fn default() -> Self {
        Self {
            entries: [(0, NamPosition::Pre); RUN_SLOTS],
            count: 0,
        }
    }
}

impl NamRun {
    fn entries(&self) -> &[(u32, NamPosition)] {
        &self.entries[..self.count]
    }

    fn index_of(&self, id: u32) -> Option<usize> {
        self.entries().iter().position(|entry| entry.0 == id)
    }

    /// The index of the first entry of `position`'s group.  `Pre` entries
    /// always come first.
    fn group_start(&self, position: NamPosition) -> usize {
        match position {
            NamPosition::Pre => 0,
            NamPosition::Post => self
                .entries()
                .iter()
                .position(|entry| entry.1 == NamPosition::Post)
                .unwrap_or(self.count),
        }
    }

    fn insert(&mut self, at: usize, entry: (u32, NamPosition)) {
        if self.count >= RUN_SLOTS {
            return;
        }
        let at = at.min(self.count);
        self.entries.copy_within(at..self.count, at + 1);
        self.entries[at] = entry;
        self.count += 1;
    }

    fn remove(&mut self, id: u32) {
        if let Some(at) = self.index_of(id) {
            self.entries.copy_within(at + 1..self.count, at);
            self.count -= 1;
        }
    }
}

/// The NAM blocks of one path, inside a
/// [`PathProcessor`](super::mixer::path::PathProcessor).
///
/// Scratch buffers are sized in [`Self::reserve`].  Every slot, with its
/// resampler and delay history, is built with the stage and reused in place,
/// so creating or letting go of a block never calls the heap on the audio
/// thread.  [`Self::begin`] is the only place the callback touches the
/// queues, and it only `try_lock`s.  [`Self::process`] runs [`NamRun`], not
/// the published stack: the two differ while a removed block fades out and
/// while a reorder waits for its dip.
pub(crate) struct NamStage {
    /// One entry per block that has (or had) runtime state, in any order;
    /// [`Self::run`] says which order they run in.  Every slot is built with
    /// the stage and reused in place, so creating or letting go of a block
    /// never calls the heap on the audio thread.  They are boxed because a
    /// stage sits inline in the mixer, and its slot history is far larger
    /// than a debug stack should carry.
    slots: [Box<NamSlot>; RUN_SLOTS],
    /// The published stack, kept so [`Self::process`] needs no lock.
    stack: NamStack,
    /// The order the blocks run in now.
    run: NamRun,
    /// The published stack changed and [`Self::run`] has not followed yet.
    order_dirty: bool,
    /// Per group, in [`NamPosition::index`] order: the gain on the group's
    /// output.  1 except while a reorder dips it.
    dip: [Ramp; 2],
    /// The controls version last applied.  0 until the first block, which
    /// the published version never is.
    version: u64,
    park: Option<Arc<NamPark>>,
    rate: u32,
    xfade: f32,
    swap_xfade: f32,
    /// The 2x pair an oversampled block runs through, designed for
    /// [`Self::rate`].
    filters: OversampleFilters,
    scratch: Vec<f32>,
    outgoing_scratch: Vec<f32>,
    /// Twice a block: where a factor-2 model's samples go.
    wide: Vec<f32>,
    /// One block each, one per side: the delayed copy of a factor-1 side
    /// while the time base moves.
    live_aligned: Vec<f32>,
    out_aligned: Vec<f32>,
}

impl NamStage {
    pub(crate) fn new(rate: u32) -> Self {
        let rate = rate.max(1);
        let filters = OversampleFilters::new(rate);
        Self {
            slots: std::array::from_fn(|_| Box::new(NamSlot::new(0, &filters))),
            stack: NamStack::default(),
            run: NamRun::default(),
            order_dirty: false,
            dip: [Ramp::new(1.0); 2],
            version: 0,
            park: None,
            rate,
            xfade: BYPASS_FADE_SECONDS * rate as f32,
            swap_xfade: SWAP_FADE_SECONDS * rate as f32,
            filters,
            scratch: Vec::new(),
            outgoing_scratch: Vec::new(),
            wide: Vec::new(),
            live_aligned: Vec::new(),
            out_aligned: Vec::new(),
        }
    }

    /// Grows the scratch once, outside the sample loop.  A larger block than
    /// prepared allocates here, once.
    pub(crate) fn reserve(&mut self, frames: usize) {
        let wide = frames.saturating_mul(2);
        if self.scratch.len() < wide {
            self.scratch.resize(wide, 0.0);
            self.outgoing_scratch.resize(wide, 0.0);
            self.wide.resize(wide, 0.0);
        }
        if self.live_aligned.len() < frames {
            self.live_aligned.resize(frames, 0.0);
            self.out_aligned.resize(frames, 0.0);
        }
    }

    /// Forgets the applied controls so the next [`Self::begin`] reads them
    /// again.  Does not touch the models.
    pub(crate) fn invalidate(&mut self) {
        self.version = 0;
        for slot in self.slots.iter_mut().filter(|slot| slot.live) {
            slot.adopted = false;
        }
    }

    /// A slot was activated again.  The live models belong to the path that
    /// just left: park them, and let the UI queue new ones.  A model that
    /// cannot be parked yet does not run; the next block parks it.
    pub(crate) fn restart(&mut self, rate: u32) {
        self.rate = rate.max(1);
        self.xfade = BYPASS_FADE_SECONDS * self.rate as f32;
        self.swap_xfade = SWAP_FADE_SECONDS * self.rate as f32;
        self.filters.retune(self.rate);
        let park = self.park.clone();
        for slot in self.slots.iter_mut() {
            slot.retune(&self.filters);
            slot.restart(park.as_ref());
        }
        self.dip = [Ramp::new(1.0); 2];
        self.version = 0;
    }

    /// Takes the queued models, applies the published stack and follows the
    /// controls.  Callback: `try_lock` only.
    ///
    /// An oversampled block's factor came with the model the UI queued; the
    /// stage never picks it.  It counts what is really installed for
    /// [`Self::latency_samples`], so a block that is still fading in or out
    /// of 2x is counted while it carries the pair's delay.
    pub(crate) fn begin(
        &mut self,
        controls: &super::path::PathControls,
        rate: u32,
        ramp_samples: f32,
    ) {
        if self.park.is_none() {
            self.park = Some(Arc::clone(controls.nam_park()));
        }
        let rate = rate.max(1);
        if rate != self.rate {
            self.rate = rate;
            self.xfade = BYPASS_FADE_SECONDS * rate as f32;
            self.swap_xfade = SWAP_FADE_SECONDS * rate as f32;
            self.filters.retune(rate);
            for slot in self.slots.iter_mut() {
                slot.retune(&self.filters);
            }
        }
        // The published stack first: a block that left it starts to fade
        // out, and a block that joined it gets its runtime state.
        if self.follow_published(controls) {
            self.order_dirty = true;
        }
        self.mark_departing();
        self.ensure_slots();
        // Park what the last swap or clear faded out before a new one may
        // take its place.  A block that is not heard has nothing to fade:
        // its old side goes now, so it cannot come back when the block is
        // heard again.
        let park = self.park.clone();
        for slot in self.slots.iter_mut().filter(|slot| slot.live) {
            slot.drain_held(park.as_ref());
            if slot.swap.is_settled_at(1.0) || !slot.audible() {
                slot.retire_outgoing(park.as_ref());
            }
        }
        self.take_commands(controls);
        let matches = {
            let slots = &self.slots;
            self.stack.level_matches(|id| {
                slots
                    .iter()
                    .filter(|slot| slot.live)
                    .any(|slot| slot.id == id && slot.has_live_model())
            })
        };
        // Retire the model a swap or a clear faded out, and settle the fades.
        for slot in self.slots.iter_mut().filter(|slot| slot.live) {
            if slot.model.is_some() && slot.model_rate != rate {
                // A rate change restarts the device: there is nothing to
                // fade, and a model built for another rate must not run.
                slot.restart(park.as_ref());
            }
            slot.drain_held(park.as_ref());
            if slot.swap.is_settled_at(1.0) {
                slot.retire_outgoing(park.as_ref());
            }
        }
        let stack = self.stack;
        for (index, block) in stack.blocks().iter().enumerate() {
            let level_db = matches.get(index).copied().unwrap_or(0.0);
            let xfade = self.xfade;
            if let Some(slot) = self.slot_mut(block.id) {
                slot.follow(block, level_db, ramp_samples, xfade);
            }
        }
        // A model that has heard its warm-up fades in.
        for slot in self.slots.iter_mut().filter(|slot| slot.live) {
            if slot.warm == 0 {
                if slot.swap.is_settled_at(0.0) {
                    slot.swap.set_target(1.0, self.swap_xfade.max(1.0));
                }
                if slot.enabling {
                    slot.enabling = false;
                    slot.wet_target = 1.0;
                    slot.wet.set_target(1.0, self.xfade.max(1.0));
                }
            }
        }
        // The time base follows the decision made above: a block that starts
        // or stops delaying moves between the two over the bypass fade, so
        // the output never jumps by the pair's delay.
        let xfade = self.xfade.max(1.0);
        for slot in self.slots.iter_mut().filter(|slot| slot.live) {
            let target = if slot.delayed { 1.0 } else { 0.0 };
            slot.delay_mix.set_target(target, xfade);
        }
        self.follow_order();
        self.release_departed();
    }

    /// One group of the path's NAM blocks, in place on channels 0 and 1.  No
    /// allocation.
    pub(crate) fn process(&mut self, block: &mut [[f32; 4]], frames: usize, position: NamPosition) {
        let Self {
            slots,
            run,
            dip,
            scratch,
            outgoing_scratch,
            wide,
            live_aligned,
            out_aligned,
            filters,
            ..
        } = self;
        let mut scratch = NamScratch {
            live: scratch,
            outgoing: outgoing_scratch,
            wide,
            live_aligned,
            out_aligned,
        };
        for &(id, group) in run.entries() {
            if group != position {
                continue;
            }
            let Some(slot) = slots.iter_mut().find(|slot| slot.live && slot.id == id) else {
                continue;
            };
            slot.process_block(block, frames, &mut scratch, filters);
        }
        let dip = &mut dip[position.index()];
        if !dip.is_settled_at(1.0) {
            for frame in block.iter_mut().take(frames) {
                let gain = dip.next();
                frame[0] *= gain;
                frame[1] *= gain;
            }
        }
    }

    /// The delay this block's blocks add together, output samples: one
    /// block's pair delay for each block whose output is delayed this block
    /// ([`NamSlot::delayed`]).  A group that is not oversampling adds none.
    /// The path adds this to its plugin latency.
    pub(crate) fn latency_samples(&self) -> u32 {
        let delay = self.filters.delay as u32;
        let blocks = self
            .slots
            .iter()
            .filter(|slot| slot.live && slot.delayed)
            .count() as u32;
        delay.saturating_mul(blocks)
    }

    fn slot(&self, id: u32) -> Option<&NamSlot> {
        self.slots
            .iter()
            .find(|slot| slot.live && slot.id == id)
            .map(Box::as_ref)
    }

    fn slot_mut(&mut self, id: u32) -> Option<&mut NamSlot> {
        self.slots
            .iter_mut()
            .find(|slot| slot.live && slot.id == id)
            .map(Box::as_mut)
    }

    /// The slot of a block in the stack.  A departing block takes no new
    /// command: what is queued for it is parked like one for a block that
    /// is gone.
    fn live_slot_mut(&mut self, id: u32) -> Option<&mut NamSlot> {
        self.slot_mut(id).filter(|slot| !slot.departing)
    }

    /// Starts the fade-out of every block that left the stack, and takes
    /// back one that returned to it before it was released.
    fn mark_departing(&mut self) {
        let stack = self.stack;
        let xfade = self.xfade;
        for slot in self.slots.iter_mut().filter(|slot| slot.live) {
            let gone = stack.index_of(slot.id).is_none();
            if gone && !slot.departing {
                slot.depart(xfade);
            } else if !gone {
                slot.departing = false;
            }
        }
    }

    /// Lets go of blocks that left the stack once they have faded to dry and
    /// their models are parked.  A block that cannot be parked yet stays,
    /// silent, and is retried next block.  The slot keeps its buffers and
    /// waits for another id.
    fn release_departed(&mut self) {
        let park = self.park.clone();
        for index in 0..self.slots.len() {
            let slot = &mut self.slots[index];
            if !slot.live || !slot.departing {
                continue;
            }
            // A block that never ran (it left during a reorder's dip,
            // before it joined the run) has nothing to fade.
            let in_run = self.run.index_of(slot.id).is_some();
            if in_run && !slot.wet.is_settled_at(0.0) {
                continue;
            }
            let id = slot.id;
            if slot.release(park.as_ref()) {
                slot.live = false;
                self.run.remove(id);
            }
        }
    }

    /// Moves [`Self::run`] to the published order.  At once when the blocks
    /// that are heard keep their order; else the groups whose order changes
    /// dip to silence first, switch there, and fade back in.
    fn follow_order(&mut self) {
        let mut dipping = [false; 2];
        if self.order_dirty {
            let next = self.next_run();
            for position in NamPosition::ALL {
                dipping[position.index()] = self.order_changes(&next, position);
            }
            let silent = NamPosition::ALL.iter().all(|position| {
                !dipping[position.index()] || self.dip[position.index()].is_settled_at(0.0)
            });
            if silent {
                self.run = next;
                self.order_dirty = false;
                dipping = [false; 2];
            }
        }
        let xfade = self.xfade.max(1.0);
        for (dip, down) in self.dip.iter_mut().zip(dipping) {
            dip.set_target(if down { 0.0 } else { 1.0 }, xfade);
        }
    }

    /// The published stack in order, with each departing block put back
    /// after the block it followed (or at the start of its group).
    fn next_run(&self) -> NamRun {
        let mut next = NamRun::default();
        for block in self.stack.blocks() {
            next.insert(next.count, (block.id, block.position));
        }
        for (index, &(id, position)) in self.run.entries().iter().enumerate() {
            if self.stack.index_of(id).is_some() || self.slot(id).is_none() {
                continue;
            }
            let after = self.run.entries()[..index]
                .iter()
                .rev()
                .filter(|entry| entry.1 == position)
                .find_map(|entry| {
                    next.index_of(entry.0)
                        .filter(|&at| next.entries()[at].1 == position)
                });
            let at = after.map_or_else(|| next.group_start(position), |at| at + 1);
            next.insert(at, (id, position));
        }
        next
    }

    /// Whether `position`'s group plays the blocks it shares with `next`
    /// that are heard in another order, or loses or gains one to the other
    /// group.  A block that joins or leaves the stack does not count: it
    /// fades on its own.
    fn order_changes(&self, next: &NamRun, position: NamPosition) -> bool {
        let heard = |id: u32| self.slot(id).is_some_and(NamSlot::audible);
        let before = self
            .run
            .entries()
            .iter()
            .filter(|entry| entry.1 == position && heard(entry.0))
            .filter(|entry| next.index_of(entry.0).is_some())
            .map(|entry| entry.0);
        let after = next
            .entries()
            .iter()
            .filter(|entry| entry.1 == position && heard(entry.0))
            .filter(|entry| self.run.index_of(entry.0).is_some())
            .map(|entry| entry.0);
        !before.eq(after)
    }

    /// Reads the published stack when the UI changed it.  A busy lock keeps
    /// last block's order, which is correct: the models match it.  `true`
    /// when a new stack was taken.
    fn follow_published(&mut self, controls: &super::path::PathControls) -> bool {
        let version = controls.nam_version();
        if version == self.version {
            return false;
        }
        let Some(stack) = controls.try_nam() else {
            return false;
        };
        self.stack = stack;
        self.version = version;
        true
    }

    /// Keeps one runtime block per published id.
    fn ensure_slots(&mut self) {
        let blocks = self.stack;
        for block in blocks.blocks() {
            if self.slot_mut(block.id).is_some() {
                continue;
            }
            let Some(free) = self.slots.iter().position(|slot| !slot.live) else {
                break;
            };
            self.slots[free].reset(block.id);
        }
    }

    /// Whether the park takes one more model now.  `true` without a park
    /// (a unit test's stage) and while the UI holds the lock.
    fn park_room(&self) -> bool {
        self.park.as_ref().is_none_or(|park| park.has_room())
    }

    /// Applies the models the UI queued.  A command that cannot be applied
    /// yet (one swap at a time, a park that is full, a block in the stack
    /// that has no runtime slot yet) stays queued.
    ///
    /// One pass over the commands queued at entry, so it ends: a command
    /// that fails is put back at the end, past the pass, and after a
    /// failure the park counts as full for the rest of it.
    fn take_commands(&mut self, controls: &super::path::PathControls) {
        let Ok(mut pending) = controls.pending_nam_mut() else {
            return;
        };
        let mut room = self.park_room();
        let mut index = 0;
        for _ in 0..pending.len() {
            let Some((id, command)) = pending.at(index) else {
                break;
            };
            let in_stack = self.stack.index_of(id).is_some();
            let ready = match self.live_slot_mut(id) {
                Some(slot) => slot.ready(room),
                // In the stack, and no slot yet (every slot is held by a
                // block still letting go): the model waits for its slot.
                None if in_stack => false,
                // The block left the stack before its model arrived: park
                // it, or wait until the park has room.
                None => matches!(command, NamCommand::Clear) || room,
            };
            if !ready {
                index += 1;
                continue;
            }
            let Some((id, command)) = pending.take(index) else {
                break;
            };
            match self.apply(id, command) {
                None => room = self.park_room(),
                Some(command) => {
                    // A park that filled up (or got busy) between the check
                    // and the apply: the command waits a block.  One entry
                    // left the queue, so pushing it back cannot grow the
                    // `Vec`.
                    pending.requeue(id, command);
                    room = false;
                }
            }
        }
    }

    fn apply(&mut self, id: u32, command: NamCommand) -> Option<NamCommand> {
        let park = self.park.clone();
        let in_stack = self.stack.index_of(id).is_some();
        match command {
            NamCommand::Install(prepared) => match self.live_slot_mut(id) {
                Some(slot) => slot.install(prepared, park.as_ref()),
                // A block of the stack without its slot yet keeps it.
                None if in_stack => Some(NamCommand::Install(prepared)),
                // The block left the stack before its model arrived:
                // `ready` made sure the park has room.
                None => match park.as_ref() {
                    Some(park) => match park.try_retire(prepared.model) {
                        Ok(()) => None,
                        Err(model) => Some(NamCommand::Install(PreparedNam {
                            model,
                            rate: prepared.rate,
                            factor: prepared.factor,
                        })),
                    },
                    None => Some(NamCommand::Install(prepared)),
                },
            },
            NamCommand::Clear => self
                .live_slot_mut(id)
                .and_then(|slot| slot.clear(park.as_ref())),
        }
    }

    /// Puts a prepared model straight into block `id`, without a queue or a
    /// fade.  Tests only; the caller is the owner thread.
    #[cfg(test)]
    pub(crate) fn install_for_test(&mut self, id: u32, prepared: PreparedNam) {
        if self.slot_mut(id).is_none() {
            if let Some(free) = self.slots.iter().position(|slot| !slot.live) {
                self.slots[free].reset(id);
            }
        }
        if let Some(slot) = self.slot_mut(id) {
            slot.install_prepared(prepared);
        }
    }

    /// Whether block `id` has a live model, for the tests.
    #[cfg(test)]
    pub(crate) fn has_model(&self, id: u32) -> bool {
        self.slots
            .iter()
            .any(|slot| slot.live && slot.id == id && slot.model.is_some())
    }
}

impl Drop for NamStage {
    fn drop(&mut self) {
        // `OutputMixer` (and these processors) is moved into the output
        // callback: `output::build_output`, the PipeWire duplex
        // `ProcessData`, the native playback stream, and the ASIO / WASAPI
        // callback state.  That closure is dropped when the stream or
        // filter is dropped.  `bridge::run_bridge` drops the stream on the
        // `rigwerk-bridge` thread after the stop loop, and cpal stops the
        // callback before dropping the closure, so this `Drop` is not the
        // realtime callback.  It is also not the UI thread when a bridge
        // is running.  A model is therefore moved into the park here and
        // dropped later by `NamPark::flush` on the UI thread.  With no
        // park the stage never left the thread that built it (a unit
        // test); that thread may drop the model.
        let Some(park) = self.park.clone() else {
            return;
        };
        for slot in self.slots.iter_mut() {
            let models = [slot.model.take(), slot.outgoing.take()]
                .into_iter()
                .chain(slot.held.iter_mut().map(Option::take))
                .flatten();
            for model in models {
                park.retire(model);
            }
        }
    }
}

#[cfg(test)]
mod tests;
