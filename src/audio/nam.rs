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
//! The blocks add no latency: a model is causal, and the receptive field is
//! only how much history it needs.  Bypass plays the dry signal as is.
//!
//! A model is not heard before it has heard the signal: one taken from the
//! queue, or one leaving bypass, first runs `receptive_field()` samples of
//! the live input silently, then crossfades in.  A swap fades from the old
//! model (or the dry signal) to the new one over [`SWAP_FADE_SECONDS`]; a
//! clear fades back to dry.  One swap at a time: a new model or a clear
//! waits until the model the last one faded out is parked, so the callback
//! never lets go of a model by overwriting it.  The width dial of a slimmable capture is applied
//! by the UI, which queues a model built at that width.  A sample rate that
//! is not the file's rate does not install, and nothing here resamples.
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

/// What one capture costs, as a share of the block period: `mean` is the
/// average of the measured buffers, `peak` the longest one.
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
    pub rate: u32,
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
    let model = match nam_rs::NamModel::from_json_str(json) {
        Ok(model) => model,
        Err(error) => return NamFile::Failed(short_error(&error)),
    };
    prepare_parsed(model, "Capture", rate, slim, None)
}

/// Reads `path` and builds a model only when its sample rate is `rate`.
/// The loader thread only: this does file I/O and allocates the runtime.
/// `frames` is the block size to measure the model at; `None` skips the
/// measurement.
pub(crate) fn prepare_nam_file(path: &str, rate: u32, slim: f32, frames: Option<usize>) -> NamFile {
    let file = std::path::Path::new(path);
    let model = match nam_rs::NamModel::from_file(file) {
        Ok(model) => model,
        Err(error) => return NamFile::Failed(short_error(&error)),
    };
    let name = display_name(&model, file);
    prepare_parsed(model, &name, rate, slim, frames)
}

fn prepare_parsed(
    parsed: nam_rs::NamModel,
    name: &str,
    rate: u32,
    slim: f32,
    frames: Option<usize>,
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
    let mut model = match nam_rs::Model::from_nam(&parsed) {
        Ok(model) => model,
        Err(error) => return NamFile::Failed(short_error(&error)),
    };
    model.reset();
    if let Some(container) = model.as_slimmable_mut() {
        container.set_slim_size(slim.clamp(0.0, 1.0));
    }
    warm_up(&mut model);
    let load = frames.map(|frames| measure_load(&mut model, rate, frames));
    let gear_type = metadata.gear_type.and_then(|gear| {
        let gear = gear.trim();
        if gear.is_empty() {
            None
        } else {
            Some(gear.to_owned())
        }
    });
    NamFile::Ready(LoadedNam {
        prepared: PreparedNam { model, rate },
        name: name.to_owned(),
        slim_steps,
        file_rate,
        gear_type,
        arch: parsed.architecture.clone(),
        level_in_dbu: finite_level(metadata.input_level_dbu),
        level_out_dbu: finite_level(metadata.output_level_dbu),
        load,
    })
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

/// How many buffers [`measure_load`] runs, and how many it throws away first
/// (the first buffers pay for cache misses the steady state does not).
const LOAD_BUFFERS: usize = 200;
const LOAD_WARMUP: usize = 8;

/// Runs [`LOAD_BUFFERS`] buffers of a quiet sine through `model` and returns
/// the mean and longest time as a share of the block period.  The loader
/// thread only: this is the one place a model is timed.  The model is left
/// reset and prewarmed, so it can go live as it is.
pub(crate) fn measure_load(model: &mut nam_rs::Model, rate: u32, frames: usize) -> NamLoad {
    let frames = frames.max(1);
    let rate = rate.max(1);
    let block_seconds = frames as f64 / rate as f64;
    let mut buffer: Vec<f32> = (0..frames)
        .map(|index| {
            let phase = index as f32 * 2.0 * std::f32::consts::PI * 220.0 / rate as f32;
            0.1 * phase.sin()
        })
        .collect();
    let mut total = 0.0f64;
    let mut longest = 0.0f64;
    for run in 0..LOAD_BUFFERS + LOAD_WARMUP {
        let started = std::time::Instant::now();
        model.process_buffer(&mut buffer);
        let elapsed = started.elapsed().as_secs_f64();
        if run < LOAD_WARMUP {
            continue;
        }
        total += elapsed;
        longest = longest.max(elapsed);
    }
    model.reset();
    warm_up(model);
    NamLoad {
        mean: (total / LOAD_BUFFERS as f64 / block_seconds) as f32,
        peak: (longest / block_seconds) as f32,
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

/// One block's runtime: the model it runs, the one it fades out of, and the
/// gains that take the block from one to the other.  Kept with its id, so a
/// reorder moves nothing.
struct NamSlot {
    id: u32,
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
    /// Samples `model` still runs unheard before it fades in.
    warm: usize,
    /// Leaving bypass waits for [`Self::warm`] before the wet fade starts.
    enabling: bool,
    /// The block left the published stack.  It keeps running at its old
    /// place in [`NamRun`] while `wet` fades to 0, and is released after.
    departing: bool,
}

impl NamSlot {
    fn new(id: u32) -> Self {
        Self {
            id,
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
        }
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
        self.model = Some(prepared.model);
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
            self.swap.snap(0.0);
        } else if !self.retire_active(park) {
            return Some(NamCommand::Install(prepared));
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
                self.swap.snap(0.0);
            }
            self.model_rate = 0;
            self.warm = 0;
        } else if !self.retire_active(park) {
            return Some(NamCommand::Clear);
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
    /// `scratch` and `outgoing` are the stage's shared mono buffers: blocks
    /// run one after the other, so one pair serves them all.
    fn process_block(
        &mut self,
        block: &mut [[f32; 4]],
        frames: usize,
        scratch: &mut [f32],
        outgoing: &mut [f32],
    ) {
        let frames = frames
            .min(block.len())
            .min(scratch.len())
            .min(outgoing.len());
        let run = self.model_runs();
        let fade_out = self.outgoing.is_some() && !self.swap.is_settled_at(1.0);
        if !run && !fade_out && self.wet.is_settled_at(0.0) {
            return;
        }
        for (index, frame) in block.iter().enumerate().take(frames) {
            let input =
                (frame[0] + frame[1]) * 0.5 * self.input_gain.next() * self.level_gain.next();
            scratch[index] = input;
            outgoing[index] = input;
        }
        if run {
            if let Some(model) = self.model.as_mut() {
                model.process_buffer(&mut scratch[..frames]);
            }
            self.warm = self.warm.saturating_sub(frames);
        }
        if fade_out {
            if let Some(model) = self.outgoing.as_mut() {
                model.process_buffer(&mut outgoing[..frames]);
            }
        }
        let new_side = run && self.model.is_some();
        for (index, frame) in block.iter_mut().enumerate().take(frames) {
            let gain = self.output_gain.next();
            let swap = self.swap.next();
            let mix = self.wet.next();
            for sample in frame.iter_mut().take(2) {
                let dry = *sample;
                let new = if new_side { scratch[index] * gain } else { dry };
                let old = if fade_out {
                    outgoing[index] * gain
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
        self.model_rate = prepared.rate;
        self.model = Some(prepared.model);
        self.warm = 0;
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
/// Scratch buffers are sized in [`Self::reserve`].  [`Self::begin`] is the
/// only place the callback touches the queues, and it only `try_lock`s.
/// [`Self::process`] runs [`NamRun`], not the published stack: the two
/// differ while a removed block fades out and while a reorder waits for
/// its dip.
pub(crate) struct NamStage {
    /// One entry per block that has (or had) runtime state, in any order;
    /// [`Self::run`] says which order they run in.
    slots: [Option<NamSlot>; RUN_SLOTS],
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
    scratch: Vec<f32>,
    outgoing_scratch: Vec<f32>,
}

impl NamStage {
    pub(crate) fn new(rate: u32) -> Self {
        let rate = rate.max(1);
        Self {
            slots: std::array::from_fn(|_| None),
            stack: NamStack::default(),
            run: NamRun::default(),
            order_dirty: false,
            dip: [Ramp::new(1.0); 2],
            version: 0,
            park: None,
            rate,
            xfade: BYPASS_FADE_SECONDS * rate as f32,
            swap_xfade: SWAP_FADE_SECONDS * rate as f32,
            scratch: Vec::new(),
            outgoing_scratch: Vec::new(),
        }
    }

    /// Grows the scratch once, outside the sample loop.  A larger block than
    /// prepared allocates here, once.
    pub(crate) fn reserve(&mut self, frames: usize) {
        if self.scratch.len() < frames {
            self.scratch.resize(frames, 0.0);
            self.outgoing_scratch.resize(frames, 0.0);
        }
    }

    /// Forgets the applied controls so the next [`Self::begin`] reads them
    /// again.  Does not touch the models.
    pub(crate) fn invalidate(&mut self) {
        self.version = 0;
        for slot in self.slots.iter_mut().flatten() {
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
        let park = self.park.clone();
        for slot in self.slots.iter_mut().flatten() {
            slot.restart(park.as_ref());
        }
        self.dip = [Ramp::new(1.0); 2];
        self.version = 0;
    }

    /// Takes the queued models, applies the published stack and follows the
    /// controls.  Callback: `try_lock` only.
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
        for slot in self.slots.iter_mut().flatten() {
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
                    .flatten()
                    .any(|slot| slot.id == id && slot.has_live_model())
            })
        };
        // Retire the model a swap or a clear faded out, and settle the fades.
        for slot in self.slots.iter_mut().flatten() {
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
        for slot in self.slots.iter_mut().flatten() {
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
            ..
        } = self;
        for &(id, group) in run.entries() {
            if group != position {
                continue;
            }
            let Some(slot) = slots.iter_mut().flatten().find(|slot| slot.id == id) else {
                continue;
            };
            slot.process_block(block, frames, scratch, outgoing_scratch);
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

    fn slot(&self, id: u32) -> Option<&NamSlot> {
        self.slots.iter().flatten().find(|slot| slot.id == id)
    }

    fn slot_mut(&mut self, id: u32) -> Option<&mut NamSlot> {
        self.slots.iter_mut().flatten().find(|slot| slot.id == id)
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
        for slot in self.slots.iter_mut().flatten() {
            let gone = stack.index_of(slot.id).is_none();
            if gone && !slot.departing {
                slot.depart(xfade);
            } else if !gone {
                slot.departing = false;
            }
        }
    }

    /// Drops runtime state for blocks that left the stack once they have
    /// faded to dry and their models are parked.  A block that cannot be
    /// parked yet stays, silent, and is retried next block.
    fn release_departed(&mut self) {
        let park = self.park.clone();
        for index in 0..self.slots.len() {
            let Some(slot) = self.slots[index].as_mut() else {
                continue;
            };
            if !slot.departing {
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
                self.slots[index] = None;
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
            let Some(free) = self.slots.iter().position(Option::is_none) else {
                break;
            };
            self.slots[free] = Some(NamSlot::new(block.id));
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
            if let Some(free) = self.slots.iter().position(Option::is_none) {
                self.slots[free] = Some(NamSlot::new(id));
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
            .flatten()
            .any(|slot| slot.id == id && slot.model.is_some())
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
        for slot in self.slots.iter_mut().flatten() {
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
