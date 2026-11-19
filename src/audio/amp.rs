//! Built-in Neural Amp Modeler stage, one per signal path.
//!
//! A `.nam` file is a frozen capture: there are no amp knobs.  The audio
//! callback never builds, parses, or drops a [`nam_rs::Model`].  The UI
//! thread builds one, resets it, and prewarms `receptive_field()` samples
//! of silence before queueing it.  The callback only `try_lock`s that queue
//! at the start of a block.  A model it replaces is moved into
//! [`AmpPark`] the same way, and only the UI thread drains that park.
//!
//! The path is stereo.  The model hears `(L + R) * 0.5` and its mono result
//! is written to both channels.  Input and output trims ramp.  The stage
//! adds no latency: a model is causal, and the receptive field is only how
//! much history it needs.  Bypass plays the dry signal as is.
//!
//! A model is not heard before it has heard the signal: one taken from the
//! queue, or one leaving bypass, first runs `receptive_field()` samples of
//! the live input silently, then crossfades in.  A swap fades from the old
//! model (or the dry signal) to the new one; a clear fades back to dry.
//! The width dial of a slimmable capture is applied by the UI, which
//! queues a model built at that width.  A sample rate that is not the
//! file's rate does not install, and nothing here resamples.

use std::sync::{Arc, Mutex, PoisonError, TryLockError};

use serde::{Deserialize, Serialize};

use super::mixer::ramp::Ramp;

/// How long bypass, a swap, and a clear crossfade.  Long enough that the
/// switch does not click.
const BYPASS_FADE_SECONDS: f32 = 0.005;

/// Soft limits for the trims and for [`AmpSettings::sanitized`].
pub const AMP_MIN_TRIM_DB: f32 = -24.0;
pub const AMP_MAX_TRIM_DB: f32 = 24.0;

/// Where the amp sits relative to the plugin effects.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AmpSlot {
    /// Before the effects.  The default.
    #[default]
    Pre,
    /// After the effects, before a post gate and the post EQ.
    Post,
}

/// Numeric controls the mix and the callback share.  The `.nam` path is
/// not here: copying it into the callback would allocate, and [`PathMix`]
/// stays [`Copy`].
///
/// [`PathMix`]: super::path::PathMix
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AmpControls {
    /// Header On/Bypass.  Off skips the model.
    pub enabled: bool,
    pub slot: AmpSlot,
    /// Gain into the model, dB.
    pub input_db: f32,
    /// Gain on the model output, dB.
    pub output_db: f32,
    /// SlimmableContainer width, 0..=1.  Ignored by other models.  The
    /// stage does not read it: the UI queues a model built at this width.
    pub slim: f32,
}

impl Default for AmpControls {
    fn default() -> Self {
        Self {
            enabled: false,
            slot: AmpSlot::Pre,
            input_db: 0.0,
            output_db: 0.0,
            slim: 1.0,
        }
    }
}

impl AmpControls {
    pub fn sanitized(self) -> Self {
        Self {
            enabled: self.enabled,
            slot: self.slot,
            input_db: finite_or(self.input_db, 0.0).clamp(AMP_MIN_TRIM_DB, AMP_MAX_TRIM_DB),
            output_db: finite_or(self.output_db, 0.0).clamp(AMP_MIN_TRIM_DB, AMP_MAX_TRIM_DB),
            slim: finite_or(self.slim, 1.0).clamp(0.0, 1.0),
        }
    }
}

/// Settings persistence and the app share, including the capture path.
/// Not [`Copy`]: the path is a [`String`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AmpSettings {
    pub enabled: bool,
    pub slot: AmpSlot,
    pub input_db: f32,
    pub output_db: f32,
    pub slim: f32,
    /// Empty means no capture.  Whitespace is cleared by [`Self::sanitized`].
    pub model_path: String,
}

impl Default for AmpSettings {
    fn default() -> Self {
        Self::from_controls(AmpControls::default(), String::new())
    }
}

impl AmpSettings {
    pub fn from_controls(controls: AmpControls, model_path: String) -> Self {
        Self {
            enabled: controls.enabled,
            slot: controls.slot,
            input_db: controls.input_db,
            output_db: controls.output_db,
            slim: controls.slim,
            model_path,
        }
    }

    /// The controls the callback reads.  The path stays on the UI side.
    pub fn controls(&self) -> AmpControls {
        AmpControls {
            enabled: self.enabled,
            slot: self.slot,
            input_db: self.input_db,
            output_db: self.output_db,
            slim: self.slim,
        }
    }

    pub fn sanitized(&self) -> Self {
        let controls = self.controls().sanitized();
        let model_path = self.model_path.trim();
        Self::from_controls(controls, model_path.to_owned())
    }

    /// Whether a save can omit this value.  The default is what an old
    /// session already means.
    pub fn is_unset(&self) -> bool {
        self == &Self::default()
    }
}

fn finite_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() {
        value
    } else {
        fallback
    }
}

fn db_to_gain(db: f32) -> f32 {
    10.0f32.powf(db / 20.0)
}

fn rates_match(expected: f64, rate: u32) -> bool {
    expected.is_finite() && expected.round() as u32 == rate
}

/// One `.nam` in a models folder.  Built on the UI thread by
/// [`scan_amp_folder`]; the runtime [`nam_rs::Model`] is not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AmpCatalogEntry {
    pub path: String,
    pub name: String,
}

/// Lists `*.nam` files directly in `folder`.  Reads and parses each file
/// for its display name.  UI thread only: this does file I/O.
pub(crate) fn scan_amp_folder(folder: &str) -> Vec<AmpCatalogEntry> {
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
        let name = match nam_rs::NamModel::from_file(&path) {
            Ok(model) => display_name(&model, &path),
            Err(_) => file_stem(&path),
        };
        entries.push(AmpCatalogEntry {
            path: path.to_string_lossy().into_owned(),
            name,
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

/// What loading one capture produced.  [`AmpFile::Ready`] is the only
/// variant that holds a runtime model, and it has already been prewarmed.
pub(crate) enum AmpFile {
    Ready(LoadedAmp),
    /// The file parsed, and its rate is not `rate`.  Nothing was installed.
    RateMismatch {
        file_rate: u32,
        name: String,
    },
    Failed(String),
}

/// A model the UI may queue.  `slim_steps` are the width thresholds of a
/// slimmable capture (empty otherwise), for [`slim_submodel`].
pub(crate) struct LoadedAmp {
    pub prepared: PreparedAmp,
    pub name: String,
    pub slim_steps: Vec<f32>,
    pub file_rate: u32,
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

/// A runtime model the audio thread may take.  Built only on the UI thread.
pub(crate) struct PreparedAmp {
    pub model: nam_rs::Model,
    pub rate: u32,
}

/// What the UI queues for the next block.  `Clear` drops the live model
/// out of the processor without the callback destroying it.
pub(crate) enum AmpCommand {
    Install(PreparedAmp),
    Clear,
}

/// Parses `json`, and builds a model only when its sample rate is `rate`.
/// UI thread only.  Tests build a capture without a file.
#[cfg(test)]
pub(crate) fn prepare_amp_json(json: &str, rate: u32, slim: f32) -> AmpFile {
    let model = match nam_rs::NamModel::from_json_str(json) {
        Ok(model) => model,
        Err(error) => return AmpFile::Failed(short_error(&error)),
    };
    prepare_parsed(model, "Capture", rate, slim)
}

/// Reads `path` and builds a model only when its sample rate is `rate`.
/// UI thread only: this does file I/O and allocates the runtime.
pub(crate) fn prepare_amp_file(path: &str, rate: u32, slim: f32) -> AmpFile {
    let file = std::path::Path::new(path);
    let model = match nam_rs::NamModel::from_file(file) {
        Ok(model) => model,
        Err(error) => return AmpFile::Failed(short_error(&error)),
    };
    let name = display_name(&model, file);
    prepare_parsed(model, &name, rate, slim)
}

fn prepare_parsed(parsed: nam_rs::NamModel, name: &str, rate: u32, slim: f32) -> AmpFile {
    let file_rate = parsed.expected_sample_rate().round() as u32;
    if !rates_match(parsed.expected_sample_rate(), rate) {
        return AmpFile::RateMismatch {
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
    let mut model = match nam_rs::Model::from_nam(&parsed) {
        Ok(model) => model,
        Err(error) => return AmpFile::Failed(short_error(&error)),
    };
    model.reset();
    if let Some(container) = model.as_slimmable_mut() {
        container.set_slim_size(slim.clamp(0.0, 1.0));
    }
    let warmup = model.receptive_field();
    if warmup > 0 {
        let mut silence = vec![0.0; warmup];
        model.process_buffer(&mut silence);
    }
    AmpFile::Ready(LoadedAmp {
        prepared: PreparedAmp { model, rate },
        name: name.to_owned(),
        slim_steps,
        file_rate,
    })
}

/// How many models the park can hold without allocating.  The callback
/// refuses to grow it.  A swap that does not fit stays in the processor
/// until [`AmpPark::flush`] has made room.
const PARK_SLOTS: usize = 16;

/// Models the audio side has let go of.  The UI thread is the only place
/// that drops them ([`AmpPark::flush`]).
pub(crate) struct AmpPark {
    models: Mutex<Vec<nam_rs::Model>>,
}

impl AmpPark {
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

/// The amp stage inside one [`PathProcessor`](super::mixer::path::PathProcessor).
///
/// Scratch buffers are sized in [`Self::reserve`].  [`Self::begin`] is the
/// only place the callback touches the queues, and it only `try_lock`s.
pub(crate) struct AmpStage {
    model: Option<nam_rs::Model>,
    /// The model fading out during a swap or a clear.  `None` while a swap
    /// fades in from the dry signal.
    outgoing: Option<nam_rs::Model>,
    /// A model [`AmpPark::try_retire`] could not take.  Held until a later
    /// block.  Two slots: the one already waiting, and the one just replaced.
    held: [Option<nam_rs::Model>; 2],
    park: Option<Arc<AmpPark>>,
    /// The rate `model` was built for.  0 when it must not run: a restart
    /// could not park it, and [`Self::begin`] keeps trying.
    model_rate: u32,
    settings: AmpControls,
    /// The controls version last applied.  0 until the first block, which
    /// the published version never is.
    version: u64,
    adopted: bool,
    input_gain: Ramp,
    output_gain: Ramp,
    /// Model against dry: 0 is bypass.
    wet: Ramp,
    wet_target: f32,
    /// New side against old side of a swap: 1 when no swap runs.
    swap: Ramp,
    /// Samples `model` still runs unheard before it fades in.
    warm: usize,
    /// Leaving bypass waits for [`Self::warm`] before the wet fade starts.
    enabling: bool,
    xfade_samples: f32,
    scratch: Vec<f32>,
    outgoing_scratch: Vec<f32>,
}

impl AmpStage {
    pub(crate) fn new(rate: u32) -> Self {
        Self {
            model: None,
            outgoing: None,
            held: [None, None],
            park: None,
            model_rate: 0,
            settings: AmpControls::default(),
            version: 0,
            adopted: false,
            input_gain: Ramp::new(1.0),
            output_gain: Ramp::new(1.0),
            wet: Ramp::new(0.0),
            wet_target: 0.0,
            swap: Ramp::new(1.0),
            warm: 0,
            enabling: false,
            xfade_samples: BYPASS_FADE_SECONDS * rate.max(1) as f32,
            scratch: Vec::new(),
            outgoing_scratch: Vec::new(),
        }
    }

    /// Grows the scratch once, outside the sample loop.  A larger block
    /// than prepared allocates here, once.
    pub(crate) fn reserve(&mut self, frames: usize) {
        if self.scratch.len() < frames {
            self.scratch.resize(frames, 0.0);
            self.outgoing_scratch.resize(frames, 0.0);
        }
    }

    pub(crate) fn slot(&self) -> AmpSlot {
        self.settings.slot
    }

    /// Forgets the applied controls so the next [`Self::begin`] reads them
    /// again.  Does not touch the model.
    pub(crate) fn invalidate(&mut self) {
        self.adopted = false;
    }

    /// A slot was activated again.  The live model belongs to the path that
    /// just left: park it, and let the UI queue the new one.  A model that
    /// cannot be parked yet does not run; [`Self::begin`] parks it later.
    pub(crate) fn restart(&mut self, rate: u32) {
        if !self.retire_active() {
            self.model_rate = 0;
        }
        self.retire_outgoing();
        self.adopted = false;
        self.xfade_samples = BYPASS_FADE_SECONDS * rate.max(1) as f32;
        self.input_gain = Ramp::new(1.0);
        self.output_gain = Ramp::new(1.0);
        self.wet = Ramp::new(0.0);
        self.wet_target = 0.0;
        self.warm = 0;
        self.enabling = false;
    }

    /// Takes a queued model and applies controls.  Callback: `try_lock`
    /// only.
    pub(crate) fn begin(
        &mut self,
        controls: &super::path::PathControls,
        rate: u32,
        ramp_samples: f32,
    ) {
        if self.park.is_none() {
            self.park = Some(Arc::clone(controls.amp_park()));
        }
        let rate = rate.max(1);
        self.xfade_samples = BYPASS_FADE_SECONDS * rate as f32;
        self.drain_held();
        if self.swap.is_settled_at(1.0) {
            self.retire_outgoing();
        }
        if self.model.is_some() && self.model_rate != rate {
            // A rate change restarts the device: there is nothing to fade.
            self.retire_active();
            self.retire_outgoing();
        }
        self.take_handoff(controls);
        self.follow_settings(controls, ramp_samples);
        if self.warm == 0 {
            if self.swap.is_settled_at(0.0) {
                self.swap.set_target(1.0, self.xfade_samples.max(1.0));
            }
            if self.enabling {
                self.enabling = false;
                self.wet_target = 1.0;
                self.wet.set_target(1.0, self.xfade_samples.max(1.0));
            }
        }
    }

    /// One block of the path, in place on channels 0 and 1.  No allocation.
    pub(crate) fn process_block(&mut self, block: &mut [[f32; 4]], frames: usize) {
        let frames = frames.min(block.len()).min(self.scratch.len());
        let run = self.model_runs();
        let fade_out = self.outgoing.is_some() && !self.swap.is_settled_at(1.0);
        if !run && !fade_out && self.wet.is_settled_at(0.0) {
            return;
        }
        for (index, frame) in block.iter().enumerate().take(frames) {
            let input = (frame[0] + frame[1]) * 0.5 * self.input_gain.next();
            self.scratch[index] = input;
            self.outgoing_scratch[index] = input;
        }
        if run {
            if let Some(model) = self.model.as_mut() {
                model.process_buffer(&mut self.scratch[..frames]);
            }
            self.warm = self.warm.saturating_sub(frames);
        }
        if fade_out {
            if let Some(model) = self.outgoing.as_mut() {
                model.process_buffer(&mut self.outgoing_scratch[..frames]);
            }
        }
        let new_side = run && self.model.is_some();
        for (index, frame) in block.iter_mut().enumerate().take(frames) {
            let gain = self.output_gain.next();
            let swap = self.swap.next();
            let mix = self.wet.next();
            for sample in frame.iter_mut().take(2) {
                let dry = *sample;
                let new = if new_side {
                    self.scratch[index] * gain
                } else {
                    dry
                };
                let old = if fade_out {
                    self.outgoing_scratch[index] * gain
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
    pub(crate) fn install_prepared(&mut self, prepared: PreparedAmp) {
        self.retire_active();
        self.accept(prepared);
        self.warm = 0;
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

    fn follow_settings(&mut self, controls: &super::path::PathControls, ramp_samples: f32) {
        let version = controls.amp_version();
        if version == self.version && self.adopted {
            return;
        }
        let Some(settings) = controls.try_amp() else {
            return;
        };
        self.settings = settings;
        self.version = version;
        let input = db_to_gain(settings.input_db);
        let output = db_to_gain(settings.output_db);
        if !self.adopted {
            self.input_gain.snap(input);
            self.output_gain.snap(output);
            // A model taken this block has not heard the signal yet.
            let cold = settings.enabled && self.warm > 0;
            let wet = if settings.enabled && !cold { 1.0 } else { 0.0 };
            self.wet.snap(wet);
            self.wet_target = wet;
            self.enabling = cold;
            self.adopted = true;
            return;
        }
        self.input_gain.set_target(input, ramp_samples);
        self.output_gain.set_target(output, ramp_samples);
        if settings.enabled {
            if self.wet_target == 0.0 && !self.enabling {
                if self.model_runs() {
                    // Still fading out: its history is the live signal.
                    self.wet_target = 1.0;
                    self.wet.set_target(1.0, self.xfade_samples.max(1.0));
                } else {
                    self.enabling = true;
                    self.warm = self.warmup();
                }
            }
        } else {
            self.enabling = false;
            if self.wet_target != 0.0 {
                self.wet_target = 0.0;
                self.wet.set_target(0.0, self.xfade_samples.max(1.0));
            }
        }
    }

    /// How long a model runs unheard before it fades in.
    fn warmup(&self) -> usize {
        self.model
            .as_ref()
            .map_or(0, nam_rs::Model::receptive_field)
    }

    fn take_handoff(&mut self, controls: &super::path::PathControls) {
        let Ok(mut slot) = controls.pending_amp_mut() else {
            return;
        };
        let Some(command) = slot.take() else {
            return;
        };
        if self.audible() && !self.swap.is_settled_at(1.0) {
            // One swap at a time: this one waits until the last has faded.
            *slot = Some(command);
            return;
        }
        match command {
            AmpCommand::Install(prepared) => {
                if self.audible() {
                    self.outgoing = self.model.take();
                    self.swap.snap(0.0);
                } else if !self.retire_active() {
                    *slot = Some(AmpCommand::Install(prepared));
                    return;
                }
                self.accept(prepared);
                self.warm = self.warmup();
            }
            AmpCommand::Clear => {
                if self.audible() {
                    if let Some(model) = self.model.take() {
                        self.outgoing = Some(model);
                        self.swap.snap(0.0);
                    }
                    self.model_rate = 0;
                    self.warm = 0;
                } else if !self.retire_active() {
                    *slot = Some(AmpCommand::Clear);
                }
            }
        }
    }

    fn accept(&mut self, prepared: PreparedAmp) {
        self.model_rate = prepared.rate;
        self.model = Some(prepared.model);
    }

    /// `false` when the live model could not be parked and is still here.
    fn retire_active(&mut self) -> bool {
        let Some(model) = self.model.take() else {
            self.model_rate = 0;
            return true;
        };
        match self.park_model(model) {
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
    fn retire_outgoing(&mut self) {
        if let Some(model) = self.outgoing.take() {
            if let Err(model) = self.park_model(model) {
                self.outgoing = Some(model);
            }
        }
        self.swap.snap(1.0);
    }

    fn drain_held(&mut self) {
        let Some(park) = self.park.clone() else {
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

    /// Moves `model` into the park, or into a held slot.  `Err` returns
    /// the model when both are busy: the caller still owns it, and it is
    /// not dropped.
    fn park_model(&mut self, model: nam_rs::Model) -> Result<(), nam_rs::Model> {
        if let Some(park) = &self.park {
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
}

impl Drop for AmpStage {
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
        // dropped later by `AmpPark::flush` on the UI thread.  With no
        // park the stage never left the thread that built it (a unit
        // test); that thread may drop the model.
        let Some(park) = self.park.clone() else {
            return;
        };
        let models = [self.model.take(), self.outgoing.take()]
            .into_iter()
            .chain(self.held.iter_mut().map(Option::take))
            .flatten();
        for model in models {
            park.retire(model);
        }
    }
}

#[cfg(test)]
mod tests;
