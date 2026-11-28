//! The callback never builds, parses, or drops a [`nam_rs::Model`].  The
//! UI thread builds one and queues it; the callback only `try_lock`s.
//! A model it replaces is parked, and only the UI thread drains [`NamPark`].
//!
//! Runtime state stays with the block id, so a reorder does not move a
//! model.
//!
//! An oversampled group of two or more delays wet and dry by the 2x
//! pair's group delay.  A single capture, or a group of one, adds none.
//!
//! One swap at a time.  The model fading out is parked before another is
//! written, so the callback never overwrites a model.
//!
//! Level match, dB: `out_A - in_B`, where calibration levels are the dBu
//! that 0 dBFS stands for.  A is the nearest running block before B in
//! the same group.  Missing metadata is 0 dB.

use std::sync::{Arc, Mutex, PoisonError, TryLockError};

use serde::{Deserialize, Serialize};

use super::mixer::ramp::Ramp;

/// 16 models of one medium capture already miss a small buffer.
pub const MAX_NAM_BLOCKS: usize = 16;

/// The sheet asks once more before adding another.
pub const NAM_SOFT_LIMIT: usize = 5;

/// Long enough that the switch does not click.
const BYPASS_FADE_SECONDS: f32 = 0.010;
/// Longer than a bypass: both sides are models with their own tails.
const SWAP_FADE_SECONDS: f32 = 0.035;
/// Nonsense levels must not blow the chain up.
pub const NAM_MAX_MATCH_DB: f32 = 36.0;

pub const NAM_MIN_TRIM_DB: f32 = -24.0;
pub const NAM_MAX_TRIM_DB: f32 = 24.0;

/// Normalised output target, dB RMS, not LUFS.  NeuralAmpModelerPlugin
/// `_SetOutputGain`, case 1 of `kOutputMode`,
/// `NeuralAmpModeler/NeuralAmpModeler.cpp`.
pub const NAM_NORMALIZED_LOUDNESS_DB: f32 = -18.0;

/// Before or after the plugin effects.  One position is one group.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum NamPosition {
    #[default]
    Pre,
    /// After the effects, before a post gate and the post EQ.
    Post,
}

impl NamPosition {
    pub const ALL: [Self; 2] = [Self::Pre, Self::Post];

    fn index(self) -> usize {
        match self {
            Self::Pre => 0,
            Self::Post => 1,
        }
    }
}

/// The callback's copy.  The `.nam` path is not here: copying it would
/// allocate, and [`NamStack`] stays [`Copy`].
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NamBlockControls {
    /// Unique in the path and never reused, so a reorder is not a swap.
    /// Not saved: a loaded path numbers its blocks.
    #[serde(skip)]
    pub id: u32,
    pub enabled: bool,
    pub position: NamPosition,
    pub input_db: f32,
    pub output_db: f32,
    /// 0..=1.  The stage ignores it; the UI queues a model at this width.
    pub slim: f32,
    pub auto_level: bool,
    /// dBu that 0 dBFS stands for.  `None` when the file does not say.
    pub level_in_dbu: Option<f32>,
    /// dBu that 0 dBFS stands for.  `None` when the file does not say.
    pub level_out_dbu: Option<f32>,
    /// File `loudness`: RMS, dBFS, not LUFS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loudness_db: Option<f32>,
    /// Only the last running block of a position group applies it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub normalize: bool,
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
            loudness_db: None,
            normalize: false,
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
            loudness_db: finite_level(self.loudness_db),
            ..self
        }
    }

    fn calibrated(&self) -> bool {
        self.level_in_dbu.is_some() && self.level_out_dbu.is_some()
    }
}

/// [`Copy`], so one `try_lock` hands it on with no allocation.  `Pre`
/// then `Post`; only the first `count` entries are live.
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

    pub fn index_of(&self, id: u32) -> Option<usize> {
        self.blocks().iter().position(|block| block.id == id)
    }

    pub fn in_position(
        &self,
        position: NamPosition,
    ) -> impl Iterator<Item = (usize, &NamBlockControls)> {
        self.blocks()
            .iter()
            .enumerate()
            .filter(move |(_, block)| block.position == position)
    }

    pub fn push(&mut self, block: NamBlockControls) -> bool {
        if self.count() >= MAX_NAM_BLOCKS {
            return false;
        }
        let at = self.group_end(block.position);
        self.insert_at(at, block);
        true
    }

    /// `-1` is the neighbour before it in the group.
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

    /// An empty `Pre` group starts at 0, in front of the effects.
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

    /// A repeated id is dropped.
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

    /// `runs` is whether the block has a model; the stack does not know.
    /// A bypassed block, or one that does not run, is skipped.
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

    /// Only the last running block of a group.  A middle one would change
    /// what the next capture hears.
    pub(crate) fn normalization_gains(&self, runs: impl Fn(u32) -> bool) -> [f32; MAX_NAM_BLOCKS] {
        let mut gains = [0.0f32; MAX_NAM_BLOCKS];
        for position in NamPosition::ALL {
            let last = self
                .blocks()
                .iter()
                .enumerate()
                .filter(|(_, block)| block.position == position && block.enabled && runs(block.id))
                .last();
            let Some((index, block)) = last else {
                continue;
            };
            if block.normalize {
                if let Some(loudness) = block.loudness_db.filter(|db| db.is_finite()) {
                    gains[index] = (NAM_NORMALIZED_LOUDNESS_DB - loudness)
                        .clamp(NAM_MIN_TRIM_DB, NAM_MAX_TRIM_DB);
                }
            }
        }
        gains
    }
}

fn is_false(value: &bool) -> bool {
    !*value
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

/// One block of a path's save, in signal order.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NamBlockSettings {
    /// Empty passes the input through.
    pub model: String,
    pub controls: NamBlockControls,
}

impl NamBlockSettings {
    pub fn sanitized(&self) -> Self {
        Self {
            model: self.model.trim().to_owned(),
            controls: self.controls.sanitized(),
        }
    }

    pub fn from_legacy(legacy: &LegacyNamSettings) -> Self {
        let legacy = legacy.sanitized();
        Self {
            model: legacy.model_path.clone(),
            controls: legacy.controls(),
        }
    }
}

/// An old save's single stage (`PathPref::amp`).  Read once into the
/// block list; nothing writes it again.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LegacyNamSettings {
    pub enabled: bool,
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
    /// Defaults have nothing to migrate.
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

/// Built on the UI thread.  The runtime model is not.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct NamCatalogEntry {
    pub path: String,
    pub name: String,
    pub arch: String,
    pub file_rate: u32,
    /// SlimmableContainer.
    pub slimmable: bool,
    /// Share of a block, before it is measured.
    pub load: NamLoad,
}

/// UI thread: this reads files.
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

/// [`Ready`] is prewarmed.  It crosses a channel once per load, not on
/// the block path, which is why the variant may be large.
#[allow(clippy::large_enum_variant)]
pub(crate) enum NamFile {
    Ready(LoadedNam),
    /// The file's rate is not the output rate, so nothing was installed.
    RateMismatch {
        file_rate: u32,
        name: String,
    },
    Failed(String),
}

/// Share of the block period.  `peak` leaves out the slowest buffers:
/// on the loader those are preemptions.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct NamLoad {
    pub mean: f32,
    pub peak: f32,
}

impl NamLoad {
    /// Not under the peak, so a projection does not flatter the model.
    pub(crate) fn share(self) -> f32 {
        self.peak.max(self.mean)
    }
}

/// `slim_steps` are the width thresholds, empty when it is not slimmable.
pub(crate) struct LoadedNam {
    pub prepared: PreparedNam,
    pub name: String,
    pub slim_steps: Vec<f32>,
    pub file_rate: u32,
    /// UI only.  The audio thread never reads it.
    pub gear_type: Option<String>,
    pub arch: String,
    /// dBu at 0 dBFS, when the file has them.
    pub level_in_dbu: Option<f32>,
    pub level_out_dbu: Option<f32>,
    /// RMS, dBFS, not LUFS.
    pub loudness_db: Option<f32>,
    /// `None` when the caller asked for no measurement.
    pub load: Option<NamLoad>,
    /// Every submodel is a WaveNet a doubled rate can scale.
    pub oversample: bool,
}

/// First threshold above `slim`, else the last.  NAM Core's
/// `SetSlimmableSize`.  0 when there are no steps.
pub(crate) fn slim_submodel(steps: &[f32], slim: f32) -> usize {
    steps
        .iter()
        .position(|&max| max > slim)
        .unwrap_or(steps.len().saturating_sub(1))
}

/// Built on the loader or the UI thread.  The callback only takes it.
pub(crate) struct PreparedNam {
    pub model: nam_rs::Model,
    /// The file's rate.  The oversampling factor never goes here.
    pub rate: u32,
    /// 1, or 2 when every layer dilation was doubled.  The slot runs a
    /// 2 through the 2x pair.
    pub factor: u8,
}

/// `Clear` takes the live model out without the callback dropping it.
pub(crate) enum NamCommand {
    Install(PreparedNam),
    Clear,
}

/// UI thread.  Tests build a capture without a file.
#[cfg(test)]
pub(crate) fn prepare_nam_json(json: &str, rate: u32, slim: f32) -> NamFile {
    prepare_nam_json_at(json, rate, slim, 1)
}

/// [`prepare_nam_json`] at an oversampling factor.
#[cfg(test)]
pub(crate) fn prepare_nam_json_at(json: &str, rate: u32, slim: f32, factor: u8) -> NamFile {
    let model = match nam_rs::NamModel::from_json_str(json) {
        Ok(model) => model,
        Err(error) => return NamFile::Failed(short_error(&error)),
    };
    prepare_parsed(model, "Capture", rate, slim, None, factor)
}

/// Loader thread: file I/O and the runtime allocation.  `None` skips
/// the measurement.  A capture that cannot be oversampled stays at 1.
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
        loudness_db: finite_level(metadata.loudness),
        load,
        oversample,
    })
}

/// A doubled rate keeps the tone only when every time constant scales.
/// Only a WaveNet.  A conv head (`head_kernel_size` or a post-stack
/// kernel above 1) is a dilation-1 conv in `nam-rs`, so it stays at 1x.
/// A `SlimmableContainer` needs every submodel.  An LSTM has no dilation.
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

/// Weights are untouched: their count follows the kernel, not the
/// dilation, so `Model::from_nam` still builds.  Only for a capture
/// [`can_oversample`] accepts.
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

/// Silence for the receptive field, so a live model does not start empty.
fn warm_up(model: &mut nam_rs::Model) {
    let warmup = model.receptive_field();
    if warmup > 0 {
        let mut silence = vec![0.0; warmup];
        model.process_buffer(&mut silence);
    }
}

const LOAD_BUFFERS: usize = 200;
/// Untimed: those buffers pay for cache misses.
const LOAD_WARMUP: usize = 8;
/// The loader may wake on an idle clock.  Timing from the first buffer
/// read a capture at up to twice its cost.
const LOAD_WARMUP_SECONDS: f64 = 0.05;
/// The loader is not realtime: the longest buffer is often a preemption.
const LOAD_PEAK_PERCENTILE: usize = 95;

/// Loader thread, the one place a model is timed.  Left reset and
/// prewarmed, so it can go live as it is.  Factor 2 is timed on
/// `2 * frames` samples against the same block period.
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

/// Buffers above the percentile are left out of both figures: on the
/// loader they are preemptions the audio thread does not see.
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

/// Before [`measure_load`]: about 5% for a WaveNet and 2% for an LSTM
/// at 48 kHz.  Cost is per sample, so the share follows the rate.
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

/// The callback never grows this.  A swap that does not fit stays in
/// the processor until [`NamPark::flush`] has made room.
const PARK_SLOTS: usize = 2 * MAX_NAM_BLOCKS;

/// The UI thread is the only place that drops a model.
pub(crate) struct NamPark {
    models: Mutex<Vec<nam_rs::Model>>,
}

impl NamPark {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            models: Mutex::new(Vec::with_capacity(PARK_SLOTS)),
        })
    }

    /// UI thread.  The spare capacity stays, so the next retire does not
    /// allocate.
    pub(crate) fn flush(&self) {
        let mut queue = self.models.lock().unwrap_or_else(PoisonError::into_inner);
        let models = std::mem::replace(&mut *queue, Vec::with_capacity(PARK_SLOTS));
        drop(queue);
        drop(models);
    }

    /// For the tests that watch a retirement.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.models
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// `true` while the queue is busy, so the caller does not wait.
    pub(crate) fn has_room(&self) -> bool {
        match self.models.try_lock() {
            Ok(queue) => queue.len() < queue.capacity(),
            Err(_) => true,
        }
    }

    /// One try, no allocation.  A full queue or a busy lock hands it back.
    pub(crate) fn try_retire(&self, model: nam_rs::Model) -> Result<(), nam_rs::Model> {
        match self.models.try_lock() {
            Ok(mut queue) => park_into(&mut queue, model),
            Err(TryLockError::WouldBlock) => Err(model),
            Err(TryLockError::Poisoned(poison)) => park_into(&mut poison.into_inner(), model),
        }
    }

    /// Not for the realtime callback.  A full queue leaks the model rather
    /// than dropping it here or spinning: the bridge is tearing a run
    /// down while the park is already full.
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

/// `push` cannot allocate.  `Err` hands the model back when the queue
/// is full.
fn park_into(queue: &mut Vec<nam_rs::Model>, model: nam_rs::Model) -> Result<(), nam_rs::Model> {
    if queue.len() == queue.capacity() {
        Err(model)
    } else {
        queue.push(model);
        Ok(())
    }
}

/// One per live block, and as many again for blocks that left before
/// the callback took their command.
const PENDING_SLOTS: usize = 2 * MAX_NAM_BLOCKS;

/// The UI pushes and may allocate.  The callback only removes, in
/// place.  One command per block: a newer one replaces the older.
#[derive(Default)]
pub(crate) struct NamPending {
    commands: Vec<(u32, NamCommand)>,
}

impl NamPending {
    /// The replaced command is returned for the UI thread to drop.  When
    /// the queue is full, a block that left the stack loses its place first.
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
            // More ids than a stack holds, so at least half have left.
            // Only a callback that has not run for a while gets here.
            // A departed block goes first; the oldest command is last.
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

    /// `remove` does not allocate.
    fn take(&mut self, index: usize) -> Option<(u32, NamCommand)> {
        (index < self.commands.len()).then(|| self.commands.remove(index))
    }

    /// Hands every command back.  The caller drops them.
    pub(crate) fn drain(&mut self) -> Vec<(u32, NamCommand)> {
        std::mem::take(&mut self.commands)
    }

    /// One entry just left, so the `Vec` has room and does not grow.
    fn requeue(&mut self, id: u32, command: NamCommand) {
        self.commands.push((id, command));
    }
}

/// Same figure as the clock bridge's resampler.
const OVERSAMPLE_STOPBAND_DB: f64 = 80.0;

/// 1 mod 4, so a clamped kernel still has an integer sample delay.
/// The clamp only bites far below any device rate.
const OVERSAMPLE_MAX_TAPS: usize = 513;

/// One half is one of the two 2x phases.
const OVERSAMPLE_HALF: usize = OVERSAMPLE_MAX_TAPS.div_ceil(2);

/// Two filters of `(length - 1) / 4`, in output samples.
const OVERSAMPLE_MAX_DELAY: usize = (OVERSAMPLE_MAX_TAPS - 1) / 2;

/// Kaiser-windowed sinc at 2x: passband to 20 kHz, stopband from the
/// output Nyquist.  Not [`FrameResampler`](super::resampler::FrameResampler):
/// that stopband starts at the lower rate minus 20 kHz, so a harmonic
/// in between would fold into the audio band.
///
/// `(length - 1)` is divisible by 4, so the pair delays by a whole
/// number of output samples.  `retune` is off the sample loop and
/// reuses its buffers.  The kernels are on the heap: the stage sits
/// inline in the mixer.
struct OversampleFilters {
    /// `even[0]` is the current input sample, `odd[0]` the half sample
    /// before it.
    even: Vec<f32>,
    odd: Vec<f32>,
    /// Scratch for [`Self::retune`], so the f64 kernel is not on the stack.
    kernel: Vec<f64>,
    taps: usize,
    /// Output samples: one filter each way, `(length - 1) / 4` each.
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

    /// Off the sample loop.  Reuses its buffers, so a rate change
    /// allocates nothing.
    fn retune(&mut self, rate: u32) {
        let rate = rate.max(1) as f64;
        let pass = 20_000.0_f64.min(0.45 * rate);
        let stop = 0.5 * rate;
        let cutoff = 0.5 * (pass + stop);
        let transition = (stop - pass).max(1.0);
        let beta = 0.1102 * (OVERSAMPLE_STOPBAND_DB - 8.7);
        // Kaiser's length for this attenuation and transition, then up
        // to the next length of 1 mod 4.
        let estimate = (OVERSAMPLE_STOPBAND_DB - 8.0)
            / (2.285 * std::f64::consts::TAU * transition / (2.0 * rate));
        let boot = estimate.ceil().max(0.0) as usize;
        let length = (4 * boot.saturating_sub(1).div_ceil(4) + 1).clamp(5, OVERSAMPLE_MAX_TAPS);
        // Cutoff as a fraction of the 2x rate.  The windowed sinc is
        // normalised to unity DC gain below.
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

    /// Gain 2: the inserted zeroes halve the average.
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

    /// `history` keeps the last `taps - 1` input pairs.
    fn downsample(&self, wide: &[f32], output: &mut [f32], history: &mut [f32]) {
        let frames = output.len();
        let pairs = history.len() / 2;
        debug_assert_eq!(history.len(), 2 * (self.taps - 1), "down history length");
        // The kernel is symmetric about a half sample at 2x, so tap `j`
        // pairs `even(n - j)` with `odd(n - j - 1)`.
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

/// The window is wanted to a few parts in `1e-12`.
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

/// `position` is `-1..=1` across the kernel.
fn kaiser_window(beta: f64, position: f64) -> f64 {
    let r = position.clamp(-1.0, 1.0);
    bessel_i0(beta * (1.0 - r * r).max(0.0).sqrt()) / bessel_i0(beta)
}

/// The tail of `samples`, or all of them when the block is shorter.
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

/// [`push_history`] for pairs.
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

/// Length 0 passes its input through.  Allocated once, with the slot.
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

    /// No allocation: the length stays inside the slot's capacity.
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

    /// The sample that left the line.
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

    /// Advances the line.  The samples that leave are discarded.
    fn feed(&mut self, samples: &[f32]) {
        if self.len == 0 {
            return;
        }
        for &sample in samples {
            self.push(sample);
        }
    }
}

/// Moves with the model when a swap starts, so the filter history
/// stays with the sound it belongs to.
struct OversampleState {
    up: Vec<f32>,
    down: Vec<f32>,
    /// Fed even while the block is not delayed, so a swap that starts
    /// one has the past output to place in front of it.
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

/// A factor-1 side's delayed copy goes to `aligned` when `crossfade`,
/// so the mix can move between the two time bases.
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

/// Shared: the blocks of a path run one after the other.  The first
/// three are twice a block.
struct NamScratch<'a> {
    live: &'a mut [f32],
    outgoing: &'a mut [f32],
    wide: &'a mut [f32],
    live_aligned: &'a mut [f32],
    out_aligned: &'a mut [f32],
}

/// Kept with its id, so a reorder moves nothing.  Built with the stage
/// and reused, so the callback never frees one.
struct NamSlot {
    id: u32,
    /// A released slot waits for the next id.
    live: bool,
    model: Option<nam_rs::Model>,
    /// Never overwritten.  `None` while a swap fades in from dry.
    outgoing: Option<nam_rs::Model>,
    /// [`NamPark::try_retire`] could not take it.  Two slots: one already
    /// waiting, and the one just replaced.
    held: [Option<nam_rs::Model>; 2],
    /// 0 when it must not run: a restart could not park it.
    model_rate: u32,
    controls: NamBlockControls,
    adopted: bool,
    input_gain: Ramp,
    output_gain: Ramp,
    level_gain: Ramp,
    /// 0 is bypass.
    wet: Ramp,
    wet_target: f32,
    /// 1 when no swap runs.
    swap: Ramp,
    /// Model samples still unheard.  Factor 2 runs two per output frame.
    warm: usize,
    /// The wet fade waits for [`Self::warm`].
    enabling: bool,
    /// Left the stack.  Keeps its old place in [`NamRun`] until `wet` is 0.
    departing: bool,
    /// 1, or 2.  The UI chose it when it queued the model.
    factor: u8,
    out_factor: u8,
    /// Wet and dry carry the pair's delay: factor 2, or a fade not settled.
    delayed: bool,
    /// 0 is direct, 1 is the pair's delay.  Moves over the bypass fade,
    /// so one sample never jumps by the delay.
    delay_mix: Ramp,
    /// Fed even while the block does not delay, so a time-base change
    /// has history to crossfade into.
    dry_l: MonoDelay,
    dry_r: MonoDelay,
    /// Moves to `out_state` when a swap starts.
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

    /// Keeps the buffers.  Only a released slot is reused.
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

    /// A rate change.  Every model restarts, so this is not heard.
    fn retune(&mut self, filters: &OversampleFilters) {
        self.dry_l.set_len(filters.delay);
        self.dry_r.set_len(filters.delay);
        self.live_state.retune(filters);
        self.out_state.retune(filters);
    }

    /// Not `Drop`: parking can fail, and the block is retried.
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

    /// Heard now or about to be, so a swap must fade.
    fn audible(&self) -> bool {
        self.wet_target > 0.0 || !self.wet.is_settled_at(0.0)
    }

    /// Samples the model runs unheard before it fades in.
    fn warmup(&self) -> usize {
        self.model
            .as_ref()
            .map_or(0, nam_rs::Model::receptive_field)
    }

    /// One swap at a time, so `outgoing` is never overwritten.  An
    /// audible fade in from dry is not cut either.
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

    /// Has a model it will play.  The level match skips the rest.
    fn has_live_model(&self) -> bool {
        !self.departing && self.model.is_some() && self.model_rate != 0
    }

    /// Fade to dry.  The block has left the stack.
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
            // The pair delays the wet side from its first sample.
            self.delayed = true;
        }
    }

    /// `None` when the model it replaces cannot be parked yet.
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
            // The filter state follows the model that fades out.
            std::mem::swap(&mut self.live_state, &mut self.out_state);
        } else if !self.retire_active(park) {
            return Some(NamCommand::Install(prepared));
        } else {
            self.live_state.reset();
            if prepared.factor < 2 {
                // Nothing is fading, so a block no longer oversampled drops
                // the pair's delay at once.
                self.delayed = false;
            }
        }
        self.accept(prepared);
        self.warm = self.warmup();
        None
    }

    /// Waits, like [`Self::install`], while a model still fades out.
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

    /// `false` when the park and the held slots are both busy.
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

    /// One that cannot be parked yet stays, silent.
    fn retire_outgoing(&mut self, park: Option<&Arc<NamPark>>) {
        if let Some(model) = self.outgoing.take() {
            if let Err(model) = self.park_model(model, park) {
                self.outgoing = Some(model);
            }
        }
        self.swap.snap(1.0);
        if self.factor == 1 {
            // Both sides are the unscaled model, so the delay can stop.
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

    /// `true` when nothing is left, so the block may be dropped.
    fn release(&mut self, park: Option<&Arc<NamPark>>) -> bool {
        self.drain_held(park);
        let active = self.retire_active(park);
        self.retire_outgoing(park);
        active && self.outgoing.is_none() && self.held.iter().all(Option::is_none)
    }

    /// `Err` returns it.  The caller still owns it, so it is not dropped.
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
        // No park yet: a unit test, still on the thread that built it.
        // The model waits in a held slot and drops with the stage.
        if let Some(slot) = self.held.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(model);
            Ok(())
        } else {
            Err(model)
        }
    }

    /// Trims, the level match, and normalization, on the output gain.
    fn follow(
        &mut self,
        controls: &NamBlockControls,
        level_db: f32,
        normalization_db: f32,
        ramp_samples: f32,
        xfade: f32,
    ) {
        self.controls = *controls;
        let input = db_to_gain(controls.input_db);
        let output = db_to_gain(controls.output_db + normalization_db);
        let level = db_to_gain(level_db);
        if !self.adopted {
            self.input_gain.snap(input);
            self.output_gain.snap(output);
            self.level_gain.snap(level);
            // Taken this block: it has not heard the signal yet.
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
                    // Still fading out, so its history is the live signal.
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

    /// The mono sum is taken before the dry delay, so a factor-2 model
    /// hears the undelayed signal and the pair contributes the delay.
    /// [`NamSlot::delay_mix`] moves between the two time bases.
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
        // Not settled on "no delay": the mix below needs both copies.
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
            // Always fed, so starting or stopping the delay has history.
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

    /// No fade and no warm-up.  Tests only; the caller owns the thread.
    #[cfg(test)]
    fn install_prepared(&mut self, prepared: PreparedNam) {
        self.accept(prepared);
        self.warm = 0;
        self.live_state.reset();
    }
}

/// The published stack, and as many again that left it and still fade.
const RUN_SLOTS: usize = 2 * MAX_NAM_BLOCKS;

/// The published order, with a block that left still at its old place.
/// [`Copy`] and fixed-size, so the callback builds one without allocating.
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

    /// `Pre` entries always come first.
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

/// Slots are built with the stage and reused, so the audio thread
/// never allocates one.  [`Self::begin`] is the only place the callback
/// touches the queues, and it only `try_lock`s.  [`Self::process`] runs
/// [`NamRun`], which still holds a block that left until its fade ends.
pub(crate) struct NamStage {
    /// Boxed: the stage sits inline in the mixer, and a slot's history
    /// is larger than a debug stack should carry.
    slots: [Box<NamSlot>; RUN_SLOTS],
    /// Kept so [`Self::process`] needs no lock.
    stack: NamStack,
    run: NamRun,
    /// The published stack changed and [`Self::run`] has not followed.
    order_dirty: bool,
    /// Per group.  1 except while a reorder dips the group's output.
    dip: [Ramp; 2],
    /// 0 until the first block, which the published version never is.
    version: u64,
    park: Option<Arc<NamPark>>,
    rate: u32,
    xfade: f32,
    swap_xfade: f32,
    filters: OversampleFilters,
    scratch: Vec<f32>,
    outgoing_scratch: Vec<f32>,
    /// Twice a block: a factor-2 model's samples.
    wide: Vec<f32>,
    /// The delayed copy of a factor-1 side while the time base moves.
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

    /// Outside the sample loop.  A larger block allocates here, once.
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

    /// The next [`Self::begin`] reads the controls again.  Models stay.
    pub(crate) fn invalidate(&mut self) {
        self.version = 0;
        for slot in self.slots.iter_mut().filter(|slot| slot.live) {
            slot.adopted = false;
        }
    }

    /// The live models belong to the path that just left.  One that
    /// cannot be parked yet does not run.
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

    /// Callback: `try_lock` only.  The UI chose each model's factor.
    /// A block still fading to or from 2x counts while it carries the delay.
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
        // A block that left starts to fade; one that joined gets a slot.
        if self.follow_published(controls) {
            self.order_dirty = true;
        }
        self.mark_departing();
        self.ensure_slots();
        // Park the faded-out side before a new swap.  An unheard block
        // has nothing to fade, so its old side goes now.
        let park = self.park.clone();
        for slot in self.slots.iter_mut().filter(|slot| slot.live) {
            slot.drain_held(park.as_ref());
            if slot.swap.is_settled_at(1.0) || !slot.audible() {
                slot.retire_outgoing(park.as_ref());
            }
        }
        self.take_commands(controls);
        let normalizations = {
            let slots = &self.slots;
            self.stack.normalization_gains(|id| {
                slots
                    .iter()
                    .filter(|slot| slot.live)
                    .any(|slot| slot.id == id && slot.has_live_model())
            })
        };
        let matches = {
            let slots = &self.slots;
            self.stack.level_matches(|id| {
                slots
                    .iter()
                    .filter(|slot| slot.live)
                    .any(|slot| slot.id == id && slot.has_live_model())
            })
        };
        // Park what faded out, then settle the fades.
        for slot in self.slots.iter_mut().filter(|slot| slot.live) {
            if slot.model.is_some() && slot.model_rate != rate {
                // A rate change restarts the device.  Nothing to fade, and a
                // model built for another rate must not run.
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
            let normalization_db = normalizations.get(index).copied().unwrap_or(0.0);
            let xfade = self.xfade;
            if let Some(slot) = self.slot_mut(block.id) {
                slot.follow(block, level_db, normalization_db, ramp_samples, xfade);
            }
        }
        // Warm-up done: fade in.
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
        // Starting or stopping the delay moves over the bypass fade, so
        // the output never jumps by the pair's delay.
        let xfade = self.xfade.max(1.0);
        for slot in self.slots.iter_mut().filter(|slot| slot.live) {
            let target = if slot.delayed { 1.0 } else { 0.0 };
            slot.delay_mix.set_target(target, xfade);
        }
        self.follow_order();
        self.release_departed();
    }

    /// No allocation.
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

    /// One pair delay per block whose output is delayed.  A group that
    /// is not oversampling adds none.
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

    /// A departing block takes no new command.  What is queued for it
    /// is parked, as for a block that is already gone.
    fn live_slot_mut(&mut self, id: u32) -> Option<&mut NamSlot> {
        self.slot_mut(id).filter(|slot| !slot.departing)
    }

    /// A block that returned before it was released is taken back.
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

    /// One that cannot be parked yet stays, silent.  The slot keeps
    /// its buffers for another id.
    fn release_departed(&mut self) {
        let park = self.park.clone();
        for index in 0..self.slots.len() {
            let slot = &mut self.slots[index];
            if !slot.live || !slot.departing {
                continue;
            }
            // Left during a reorder's dip, before it joined: nothing to fade.
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

    /// At once when the heard blocks keep their order.  Otherwise the
    /// groups that change dip to silence, switch, and fade back in.
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

    /// A departing block stays after the one it followed.
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

    /// A block that joins or leaves fades on its own and does not count.
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

    /// A busy lock keeps last block's order: the models match it.
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

    /// One runtime block per published id.
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

    /// `true` with no park (a unit test) and while the UI holds the lock.
    fn park_room(&self) -> bool {
        self.park.as_ref().is_none_or(|park| park.has_room())
    }

    /// One pass over the commands queued at entry, so it ends.  A failure
    /// is put back past the pass, and the park counts as full after it.
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
                // In the stack, no slot yet: every slot is still letting go.
                None if in_stack => false,
                // Left the stack before its model arrived.
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
                    // The park filled between the check and the apply.  One
                    // entry left, so pushing it back cannot grow the `Vec`.
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
                // In the stack, and still waiting for a slot.
                None if in_stack => Some(NamCommand::Install(prepared)),
                // Left before its model arrived.  `ready` checked the park.
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

    /// No queue and no fade.  Tests only; the caller owns the thread.
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

    /// For the tests.
    #[cfg(test)]
    pub(crate) fn has_model(&self, id: u32) -> bool {
        self.slots
            .iter()
            .any(|slot| slot.live && slot.id == id && slot.model.is_some())
    }
}

impl Drop for NamStage {
    fn drop(&mut self) {
        // The mixer is dropped on the bridge thread after the callback
        // has returned, so this is not the realtime callback and not the
        // UI thread.  Models are parked for `NamPark::flush`.  With no
        // park the stage never left the thread that built it.
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
