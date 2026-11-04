//! A small VST3 and CLAP insert chain for the master bus.
//!
//! Plugins are discovered and loaded through `vst3-host` or the CLAP host in
//! [`super::clap`]; the chain lives in [`super::shared::Shared`] so the playback
//! callback can process it.  The UI only ever holds descriptors, never the
//! plugin objects themselves.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, TryLockError};

use vst3_host::{AudioBuffers, Plugin, Vst3Host};

use super::clap::ClapPlugin;

/// Which plugin format an insert uses.  Saved inserts written before CLAP
/// support default to VST3.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginFormat {
    #[default]
    Vst3,
    Clap,
}

impl PluginFormat {
    pub fn label(self) -> &'static str {
        match self {
            Self::Vst3 => "VST3",
            Self::Clap => "CLAP",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            Self::Vst3 => "V3",
            Self::Clap => "CL",
        }
    }
}

/// A loaded plugin is shared between the audio callback (which processes it)
/// and the window (which may show the plugin's own editor), so both sides use
/// the same instance.
#[derive(Clone)]
pub(crate) enum PluginHandle {
    Vst3(Arc<Mutex<Plugin>>),
    Clap(Arc<Mutex<ClapPlugin>>),
}

impl PluginHandle {}

/// The largest block a plugin is asked to process, and the block size it is
/// set up with.  Many plugins allocate fixed internal buffers for the size
/// announced at setup and ignore the smaller `numSamples` passed later, so a
/// big 4096-frame announcement makes them write past those buffers and can
/// take the whole app down (Eye Scream Man does exactly this).  512 frames is
/// the size every VST3/CLAP effect can be expected to handle, and longer
/// bridge blocks are split into chunks of this size below.
const MAX_BLOCK: usize = 512;

/// A parameter value the person changed, kept with the insert so the plugin
/// opens with the same settings after a restart or a sample-rate change.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PluginParamPref {
    pub id: u32,
    pub value: f64,
}

/// One effect in the insert chain, in the form that is saved to preferences.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PluginPref {
    pub path: String,
    #[serde(default)]
    pub format: PluginFormat,
    #[serde(default)]
    pub class_id: Option<String>,
    pub name: String,
    #[serde(default)]
    pub vendor: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub bypass: bool,
    #[serde(default = "default_mix")]
    pub mix: f32,
    /// The controls the person adjusted, so the plugin reopens with the same
    /// settings after a restart or a sample-rate change.
    #[serde(default)]
    pub params: Vec<PluginParamPref>,
}

fn default_mix() -> f32 {
    1.0
}

impl Default for PluginPref {
    fn default() -> Self {
        Self {
            path: String::new(),
            format: PluginFormat::Vst3,
            class_id: None,
            name: String::new(),
            vendor: String::new(),
            category: String::new(),
            bypass: false,
            mix: 1.0,
            params: Vec::new(),
        }
    }
}

/// One adjustable control of a loaded plugin, normalized to 0..1 for the UI.
#[derive(Clone, Debug, Default)]
pub struct PluginParamView {
    pub id: u32,
    pub name: String,
    pub value: f64,
    pub default: f64,
    /// 0 for a continuous control, otherwise the number of steps (so a toggle
    /// reports 1).
    pub step_count: i32,
    pub unit: String,
    /// What the plugin itself would show, e.g. "440.00 Hz" or "Sine".
    pub display: String,
    pub read_only: bool,
}

/// What the UI shows for a slot; cheap to clone and independent of the plugin.
#[derive(Clone, Debug, Default)]
pub struct PluginView {
    pub path: String,
    pub format: PluginFormat,
    pub class_id: Option<String>,
    pub name: String,
    pub vendor: String,
    pub category: String,
    pub bypass: bool,
    pub mix: f32,
    pub error: Option<String>,
    pub parameters: Vec<PluginParamView>,
    /// Whether the plugin can draw its own graphical editor.
    pub has_editor: bool,
}

pub(crate) struct Slot {
    pref: PluginPref,
    plugin: Option<PluginHandle>,
    error: Option<String>,
    parameters: Vec<PluginParamView>,
}

impl Slot {
    fn empty(pref: PluginPref) -> Self {
        Self {
            pref,
            plugin: None,
            error: None,
            parameters: Vec::new(),
        }
    }

    fn loaded(pref: PluginPref, sample_rate: u32) -> Self {
        match load_plugin(&pref, sample_rate) {
            Ok(plugin) => {
                let parameters = host_parameters(&plugin);
                Self {
                    pref,
                    plugin: Some(plugin),
                    error: None,
                    parameters,
                }
            }
            Err(error) => Self {
                pref,
                plugin: None,
                error: Some(error),
                parameters: Vec::new(),
            },
        }
    }

    fn active(&self) -> bool {
        !self.pref.bypass && self.error.is_none() && self.plugin.is_some()
    }

    /// Opens a saved slot on demand (the configure window may be opened before
    /// the bridge has started) and refreshes its control list so the panel
    /// shows the values the plugin has right now.  A slot that failed stays
    /// failed.
    fn ensure_loaded(&mut self, sample_rate: u32) {
        if self.plugin.is_none() && self.error.is_none() {
            *self = Slot::loaded(self.pref.clone(), sample_rate);
        } else if let Some(parameters) = self.plugin.as_ref().map(host_parameters) {
            self.parameters = parameters;
        }
    }

    fn view(&self) -> PluginView {
        PluginView {
            path: self.pref.path.clone(),
            format: self.pref.format,
            class_id: self.pref.class_id.clone(),
            name: self.pref.name.clone(),
            vendor: self.pref.vendor.clone(),
            category: self.pref.category.clone(),
            bypass: self.pref.bypass,
            mix: self.pref.mix,
            error: self.error.clone(),
            parameters: self.parameters.clone(),
            has_editor: self.plugin.as_ref().map(host_has_editor).unwrap_or(false),
        }
    }
}

fn host_parameters(handle: &PluginHandle) -> Vec<PluginParamView> {
    match handle {
        PluginHandle::Vst3(plugin) => {
            let plugin = plugin.lock().unwrap_or_else(|poison| poison.into_inner());
            parameter_views(&plugin)
        }
        PluginHandle::Clap(plugin) => {
            let plugin = plugin.lock().unwrap_or_else(|poison| poison.into_inner());
            plugin.parameters()
        }
    }
}

fn host_has_editor(handle: &PluginHandle) -> bool {
    match handle {
        PluginHandle::Vst3(plugin) => plugin
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .has_editor(),
        PluginHandle::Clap(plugin) => plugin
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .has_editor(),
    }
}

fn host_set_parameter(handle: &PluginHandle, id: u32, value: f64) -> Result<(), String> {
    match handle {
        PluginHandle::Vst3(plugin) => plugin
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .set_parameter(id, value.clamp(0.0, 1.0))
            .map_err(|error| format!("Could not set parameter: {error}")),
        PluginHandle::Clap(plugin) => plugin
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .set_parameter(id, value),
    }
}

fn host_reset_parameters(handle: &PluginHandle) -> Result<Vec<PluginParamView>, String> {
    match handle {
        PluginHandle::Vst3(plugin) => {
            let mut plugin = plugin.lock().unwrap_or_else(|poison| poison.into_inner());
            let parameters = parameter_views(&plugin);
            for parameter in parameters.iter().filter(|parameter| !parameter.read_only) {
                let _ = plugin.set_parameter(parameter.id, parameter.default);
            }
            Ok(parameter_views(&plugin))
        }
        PluginHandle::Clap(plugin) => plugin
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .reset_parameters(),
    }
}

/// Reads the plugin's controls once, using the plugin's own value formatting.
fn parameter_views(plugin: &Plugin) -> Vec<PluginParamView> {
    let Ok(parameters) = plugin.get_parameters() else {
        return Vec::new();
    };
    parameters
        .into_iter()
        .map(|parameter| {
            let display = plugin
                .format_parameter(parameter.id, parameter.value)
                .unwrap_or_else(|_| parameter.format_value(parameter.value));
            PluginParamView {
                id: parameter.id,
                name: parameter.name,
                value: parameter.value,
                default: parameter.default,
                step_count: parameter.step_count,
                unit: parameter.unit,
                display,
                read_only: parameter.is_read_only,
            }
        })
        .collect()
}

fn load_plugin(pref: &PluginPref, sample_rate: u32) -> Result<PluginHandle, String> {
    match pref.format {
        PluginFormat::Vst3 => load_vst3(pref, sample_rate)
            .map(|plugin| PluginHandle::Vst3(Arc::new(Mutex::new(plugin)))),
        PluginFormat::Clap => {
            let plugin_id = pref
                .class_id
                .as_deref()
                .filter(|id| !id.trim().is_empty())
                .ok_or_else(|| "The CLAP plugin id is missing.".to_owned())?;
            let path = PathBuf::from(&pref.path);
            if !path.exists() {
                return Err(format!("Plugin not found: {}", path.display()));
            }
            super::clap::ClapPlugin::load(&path, plugin_id, sample_rate, MAX_BLOCK)
                .map(|plugin| PluginHandle::Clap(Arc::new(Mutex::new(plugin))))
        }
    }
}

fn load_vst3(pref: &PluginPref, sample_rate: u32) -> Result<Plugin, String> {
    let path = PathBuf::from(&pref.path);
    if !path.exists() {
        return Err(format!("Plugin not found: {}", path.display()));
    }
    let rate = if sample_rate == 0 {
        48_000.0
    } else {
        sample_rate as f64
    };
    let mut host = Vst3Host::builder()
        .sample_rate(rate)
        .block_size(MAX_BLOCK)
        .build()
        .map_err(|error| format!("VST3 host: {error}"))?;
    let mut plugin = match &pref.class_id {
        Some(class_id) if !class_id.trim().is_empty() => host
            .load_plugin_class(&path, class_id)
            .map_err(|error| format!("Load failed: {error}"))?,
        _ => host
            .load_plugin(&path)
            .map_err(|error| format!("Load failed: {error}"))?,
    };
    // Restore the controls the person changed.  A stale parameter id from an
    // older plugin version is skipped rather than failing the whole insert.
    for saved in &pref.params {
        let _ = plugin.set_parameter(saved.id, saved.value.clamp(0.0, 1.0));
    }
    plugin
        .start_processing()
        .map_err(|error| format!("Start failed: {error}"))?;
    Ok(plugin)
}

/// The master insert chain.  The audio callback only try-locks the chain and
/// each loaded plugin, so an edit or an open editor skips that processing for
/// a block instead of stalling the callback; UI paths may still block on the
/// chain lock.
pub(crate) struct PluginChain {
    slots: Vec<Slot>,
    buffers: AudioBuffers,
    dry_l: Vec<f32>,
    dry_r: Vec<f32>,
    sample_rate: u32,
}

impl PluginChain {
    pub(crate) fn new(sample_rate: u32) -> Self {
        let rate = if sample_rate == 0 {
            48_000
        } else {
            sample_rate
        };
        Self {
            slots: Vec::new(),
            buffers: AudioBuffers::new(2, 2, MAX_BLOCK, rate as f64),
            dry_l: vec![0.0; MAX_BLOCK],
            dry_r: vec![0.0; MAX_BLOCK],
            sample_rate: rate,
        }
    }

    pub(crate) fn restore(&mut self, prefs: Vec<PluginPref>) {
        self.slots = prefs.into_iter().map(Slot::empty).collect();
    }

    /// Builds a fully opened slot without holding the chain lock.  Add it with
    /// [`Self::push_slot`] once it is ready, so a slow library load or
    /// `start_processing` never stalls the audio callback.  A failed load is
    /// returned as an error but remains a valid slot to push, so the failure
    /// stays visible in the insert list instead of disappearing.
    pub(crate) fn load_slot(pref: PluginPref, sample_rate: u32) -> (Slot, Result<(), String>) {
        let slot = Slot::loaded(pref, sample_rate);
        let result = match &slot.error {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        };
        (slot, result)
    }

    /// Splices an already loaded slot into the chain.  The caller holds the
    /// chain lock only for this push.
    pub(crate) fn push_slot(&mut self, slot: Slot) {
        self.slots.push(slot);
    }

    pub(crate) fn refresh_rate(&mut self, sample_rate: u32) {
        let rate = if sample_rate == 0 {
            48_000
        } else {
            sample_rate
        };
        let rate_changed = rate != self.sample_rate;
        self.sample_rate = rate;
        self.buffers.sample_rate = rate as f64;
        for slot in &mut self.slots {
            // Saved slots start empty: open them on the first start even when
            // the rate happens to match.  A rate change reloads them all.
            if !rate_changed && slot.plugin.is_some() {
                continue;
            }
            let pref = slot.pref.clone();
            *slot = Slot::loaded(pref, rate);
        }
        if rate_changed {
            self.buffers = AudioBuffers::new(2, 2, MAX_BLOCK, rate as f64);
        }
    }

    pub(crate) fn rate(&self) -> u32 {
        self.sample_rate
    }

    pub(crate) fn views(&self) -> Vec<PluginView> {
        self.slots.iter().map(Slot::view).collect()
    }

    pub(crate) fn prefs(&self) -> Vec<PluginPref> {
        self.slots.iter().map(|slot| slot.pref.clone()).collect()
    }

    pub(crate) fn active(&self) -> usize {
        self.slots.iter().filter(|slot| slot.active()).count()
    }

    pub(crate) fn remove(&mut self, index: usize) {
        if index < self.slots.len() {
            self.slots.remove(index);
        }
    }

    pub(crate) fn move_slot(&mut self, index: usize, delta: isize) {
        if index >= self.slots.len() {
            return;
        }
        let target = (index as isize + delta).clamp(0, self.slots.len() as isize - 1) as usize;
        if target != index {
            self.slots.swap(index, target);
        }
    }

    pub(crate) fn set_bypass(&mut self, index: usize, bypass: bool) {
        if let Some(slot) = self.slots.get_mut(index) {
            slot.pref.bypass = bypass;
        }
    }

    pub(crate) fn set_mix(&mut self, index: usize, mix: f32) {
        if let Some(slot) = self.slots.get_mut(index) {
            slot.pref.mix = mix.clamp(0.0, 1.0);
        }
    }

    /// Opens a saved slot so its controls can be shown before playback starts.
    pub(crate) fn ensure_loaded(&mut self, index: usize) {
        let rate = self.sample_rate;
        if let Some(slot) = self.slots.get_mut(index) {
            slot.ensure_loaded(rate);
        }
    }

    /// The plugin instance behind one slot, shared with the window that may
    /// show the plugin's own editor.
    pub(crate) fn handle(&self, index: usize) -> Option<PluginHandle> {
        self.slots.get(index).and_then(|slot| slot.plugin.clone())
    }

    /// Gives CLAP plugins their main-thread callbacks and timers even when
    /// their own interface is not open.  A busy audio callback simply skips
    /// this frame; the work is picked up next time.
    pub(crate) fn service_main_threads(&mut self) {
        for slot in &mut self.slots {
            let Some(PluginHandle::Clap(handle)) = &slot.plugin else {
                continue;
            };
            match handle.try_lock() {
                Ok(mut plugin) => plugin.service_main_thread(),
                Err(std::sync::TryLockError::Poisoned(poison)) => {
                    poison.into_inner().service_main_thread()
                }
                Err(std::sync::TryLockError::WouldBlock) => {}
            }
        }
    }

    /// Copies the controls back from the plugin after its own editor changed
    /// them, so the saved insert keeps those settings across a restart.
    pub(crate) fn sync_from_plugin(&mut self, index: usize) -> Result<(), String> {
        let Some(slot) = self.slots.get_mut(index) else {
            return Err("That insert is no longer in the chain".to_owned());
        };
        let Some(plugin) = &slot.plugin else {
            return Err(slot
                .error
                .clone()
                .unwrap_or_else(|| "The plugin is not loaded".to_owned()));
        };
        if let PluginHandle::Clap(clap) = plugin {
            let clap = clap.lock().unwrap_or_else(|poison| poison.into_inner());
            if clap.has_pending_parameters() {
                // A control moved in RigWerk's panel is waiting for the next
                // audio block; reading the plugin now would show the old value.
                return Ok(());
            }
        }
        let parameters = host_parameters(plugin);
        slot.pref.params = parameters
            .iter()
            .filter(|parameter| (parameter.value - parameter.default).abs() > 0.0005)
            .map(|parameter| PluginParamPref {
                id: parameter.id,
                value: parameter.value,
            })
            .collect();
        slot.parameters = parameters;
        Ok(())
    }

    /// Sets one normalized plugin control and keeps the saved insert in step.
    pub(crate) fn set_parameter(
        &mut self,
        index: usize,
        id: u32,
        value: f64,
    ) -> Result<(), String> {
        let rate = self.sample_rate;
        let Some(slot) = self.slots.get_mut(index) else {
            return Err("That insert is no longer in the chain".to_owned());
        };
        slot.ensure_loaded(rate);
        let Some(plugin) = slot.plugin.as_ref() else {
            return Err(slot
                .error
                .clone()
                .unwrap_or_else(|| "The plugin is not loaded".to_owned()));
        };
        let value = value.clamp(0.0, 1.0);
        host_set_parameter(plugin, id, value)?;
        match plugin {
            PluginHandle::Vst3(_) => {
                slot.parameters = host_parameters(plugin);
            }
            PluginHandle::Clap(clap) => {
                let display = clap
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .display_for(id, value);
                if let Some(parameter) = slot
                    .parameters
                    .iter_mut()
                    .find(|parameter| parameter.id == id)
                {
                    parameter.value = value;
                    parameter.display = display;
                }
            }
        }
        if let Some(saved) = slot.pref.params.iter_mut().find(|saved| saved.id == id) {
            saved.value = value;
        } else {
            slot.pref.params.push(PluginParamPref { id, value });
        }
        Ok(())
    }

    /// Returns every control to the plugin's own defaults and forgets the
    /// saved overrides.
    pub(crate) fn reset_parameters(&mut self, index: usize) -> Result<(), String> {
        let rate = self.sample_rate;
        let Some(slot) = self.slots.get_mut(index) else {
            return Err("That insert is no longer in the chain".to_owned());
        };
        slot.ensure_loaded(rate);
        let Some(plugin) = slot.plugin.as_ref() else {
            return Err(slot
                .error
                .clone()
                .unwrap_or_else(|| "The plugin is not loaded".to_owned()));
        };
        slot.parameters = host_reset_parameters(plugin)?;
        slot.pref.params.clear();
        Ok(())
    }

    /// Runs the enabled effects over one stereo block.  A plugin whose lock is
    /// held (its editor is doing UI work) is skipped for this block, and a
    /// plugin that fails is left in the chain with its error so the person can
    /// see why it is silent.
    pub(crate) fn process(&mut self, left: &mut [f32], right: &mut [f32], frames: usize) {
        let frames = frames.min(left.len()).min(right.len());
        if frames == 0 {
            return;
        }
        let Self {
            slots,
            buffers,
            dry_l,
            dry_r,
            ..
        } = self;
        for index in 0..slots.len() {
            if !slots[index].active() {
                continue;
            }
            let mix = slots[index].pref.mix.clamp(0.0, 1.0);
            if mix <= 0.0 {
                continue;
            }
            let Some(handle) = slots[index].plugin.clone() else {
                continue;
            };
            match handle {
                PluginHandle::Vst3(plugin) => {
                    // The plugin's editor holds this lock while it runs UI work.
                    // Skipping the insert for this block is better than blocking
                    // the audio callback behind the UI thread.
                    let mut plugin = match plugin.try_lock() {
                        Ok(plugin) => plugin,
                        Err(TryLockError::Poisoned(poison)) => poison.into_inner(),
                        Err(TryLockError::WouldBlock) => continue,
                    };
                    let mut offset = 0;
                    while offset < frames {
                        let count = (frames - offset).min(MAX_BLOCK);
                        buffers.block_size = count;
                        // The VST3 host works out how many frames to run from the
                        // length of these vectors, not from `block_size`.  They
                        // are allocated at MAX_BLOCK, so without trimming them to
                        // `count` every short callback would still advance the
                        // plugin by a full 512 frames — the plugin would run too
                        // fast and time-based effects would sound detuned.  The
                        // vectors keep their capacity, so this does not allocate.
                        for channel in &mut buffers.inputs {
                            channel.resize(count, 0.0);
                        }
                        for channel in &mut buffers.outputs {
                            channel.resize(count, 0.0);
                        }
                        buffers.inputs[0].copy_from_slice(&left[offset..offset + count]);
                        buffers.inputs[1].copy_from_slice(&right[offset..offset + count]);
                        buffers.outputs[0].fill(0.0);
                        buffers.outputs[1].fill(0.0);
                        if mix < 1.0 {
                            dry_l[..count].copy_from_slice(&left[offset..offset + count]);
                            dry_r[..count].copy_from_slice(&right[offset..offset + count]);
                        }
                        match plugin.process_audio(buffers) {
                            Ok(()) => {
                                for sample in 0..count {
                                    let wet_l = buffers.outputs[0][sample];
                                    let wet_r = buffers.outputs[1][sample];
                                    if mix < 1.0 {
                                        let dry = 1.0 - mix;
                                        left[offset + sample] = dry_l[sample] * dry + wet_l * mix;
                                        right[offset + sample] = dry_r[sample] * dry + wet_r * mix;
                                    } else {
                                        left[offset + sample] = wet_l;
                                        right[offset + sample] = wet_r;
                                    }
                                }
                            }
                            Err(error) => {
                                drop(plugin);
                                slots[index].error = Some(format!("Processing failed: {error}"));
                                break;
                            }
                        }
                        offset += count;
                    }
                }
                PluginHandle::Clap(plugin) => {
                    let mut plugin = match plugin.try_lock() {
                        Ok(plugin) => plugin,
                        Err(TryLockError::Poisoned(poison)) => poison.into_inner(),
                        Err(TryLockError::WouldBlock) => continue,
                    };
                    if let Err(error) = plugin.process(left, right, frames, mix) {
                        drop(plugin);
                        slots[index].error = Some(format!("Processing failed: {error}"));
                    }
                }
            }
        }
    }
}
