//! A small VST3 and CLAP insert chain for the master bus.
//!
//! Plugins are discovered and loaded through `vst3-host` or the CLAP host in
//! [`super::clap`]; the chain lives in [`super::shared::Shared`] so the playback
//! callback can process it.  The UI only ever holds descriptors, never the
//! plugin objects themselves.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, TryLockError};

use vst3_host::{AudioBuffers, Plugin, Vst3Host};

use super::clap::{ClapPlugin, HostState, StateSave, AUDIO_PORTS_CHANGED};

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

impl PluginHandle {
    /// Whether both handles point at the same plugin instance.
    pub(crate) fn same(&self, other: &PluginHandle) -> bool {
        match (self, other) {
            (PluginHandle::Vst3(a), PluginHandle::Vst3(b)) => Arc::ptr_eq(a, b),
            (PluginHandle::Clap(a), PluginHandle::Clap(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}

/// Reads a plugin's current controls for [`PluginChain::apply_synced_parameters`],
/// without the chain lock.  Returns `None` while a CLAP value (one moved in
/// RigWerk's panel, or a saved one restored into a plugin without
/// `params.flush`) is still waiting for the next audio block, because reading
/// the plugin then would show the old value and the sync would save it.
pub(crate) fn read_plugin_parameters(handle: &PluginHandle) -> Option<Vec<PluginParamView>> {
    if let PluginHandle::Clap(clap) = handle {
        let clap = clap.lock().unwrap_or_else(|poison| poison.into_inner());
        if clap.has_pending_parameters() {
            return None;
        }
    }
    Some(host_parameters(handle))
}

/// Runs one CLAP plugin's main-thread work.  Callbacks and timers run without
/// any lock; only a pending flush or rescan takes the instance lock, and only
/// when the audio callback is not holding it.
pub(crate) fn service_clap(host: &HostState, plugin: &Mutex<ClapPlugin>) {
    if !host.service_main_thread() {
        return;
    }
    match plugin.try_lock() {
        Ok(mut plugin) => plugin.service_pending(),
        Err(TryLockError::Poisoned(poison)) => poison.into_inner().service_pending(),
        Err(TryLockError::WouldBlock) => {}
    }
}

/// One plugin whose state [`PluginChain::state_requests`] wants captured.
pub(crate) struct StateRequest {
    pub(crate) index: usize,
    pub(crate) handle: PluginHandle,
    host: Option<Arc<HostState>>,
    /// The saved parameters at request time, kept with the blob as its
    /// snapshot.
    params: Vec<PluginParamPref>,
}

/// Captures one plugin's opaque state.  Call it on the UI thread without the
/// chain lock.  CLAP saves through the main-thread target and never takes the
/// instance lock, so the audio callback keeps processing the insert; VST3 has
/// to lock the instance, which is why only stale slots are requested.
pub(crate) fn capture_state(request: &StateRequest) -> Capture {
    let bytes = match (&request.handle, &request.host) {
        (PluginHandle::Clap(_), Some(host)) => match host.save_state() {
            StateSave::Saved(bytes) => bytes,
            StateSave::Unavailable => return Capture::Failed,
            StateSave::Deferred => return Capture::Deferred,
        },
        (PluginHandle::Clap(_), None) => return Capture::Failed,
        (PluginHandle::Vst3(plugin), _) => {
            match plugin
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .save_state()
            {
                Ok(bytes) => bytes,
                Err(_) => return Capture::Failed,
            }
        }
    };
    if bytes.is_empty() {
        Capture::Failed
    } else if bytes.len() > MAX_STATE_BLOB {
        eprintln!(
            "[plugins] not saving a plugin's state: it is larger than {} MB",
            MAX_STATE_BLOB / (1024 * 1024)
        );
        Capture::Failed
    } else {
        Capture::Saved(PluginState::capture(&bytes, &request.params))
    }
}

/// The largest state blob kept in the preferences file.
const MAX_STATE_BLOB: usize = 8 * 1024 * 1024;

/// The outcome of [`capture_state`].
#[derive(Debug)]
pub(crate) enum Capture {
    Saved(PluginState),
    /// The plugin cannot save its state; the previous blob is kept.
    Failed,
    /// Values are still queued for the plugin, so its state would be older
    /// than the saved parameters; the slot stays stale and the next save
    /// tries again.
    Deferred,
}

impl Capture {
    /// Whether the slot still needs a capture afterwards.
    fn leaves_stale(&self) -> bool {
        matches!(self, Capture::Deferred)
    }
}

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
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PluginParamPref {
    pub id: u32,
    pub value: f64,
}

/// The layout of [`PluginState::data`] this build writes and reads: the bytes
/// the format's own state call produced (CLAP `clap_plugin_state.save`, or
/// `vst3-host`'s `Plugin::save_state` envelope), base64-encoded.  A blob with
/// any other version is ignored and the insert falls back to its parameters.
pub(crate) const PLUGIN_STATE_VERSION: u32 = 1;

/// A plugin's opaque state, captured on the UI thread whenever preferences
/// are saved.  It covers what parameters cannot (loaded samples, internal
/// modes, a plugin's own preset), so it is restored before the parameters.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PluginState {
    pub version: u32,
    /// Base64 of the plugin's state bytes.
    pub data: String,
    /// [`PluginPref::params`] as it was when the state was captured.  A
    /// parameter changed after the capture differs from this snapshot, and is
    /// applied on top of the restored state so the newer value wins.
    #[serde(default)]
    pub params: Vec<PluginParamPref>,
}

impl PluginState {
    pub(crate) fn capture(bytes: &[u8], params: &[PluginParamPref]) -> Self {
        Self {
            version: PLUGIN_STATE_VERSION,
            data: base64_encode(bytes),
            params: params.to_vec(),
        }
    }

    /// The state bytes, or `None` when this build cannot use the blob.
    pub(crate) fn bytes(&self) -> Option<Vec<u8>> {
        if self.version != PLUGIN_STATE_VERSION {
            return None;
        }
        base64_decode(&self.data).filter(|bytes| !bytes.is_empty())
    }
}

/// Reads a saved state blob, treating a damaged one as absent: losing the
/// blob falls back to the parameter list, while failing here would drop the
/// whole insert from the preferences.
fn lenient_state<'de, D>(deserializer: D) -> Result<Option<PluginState>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum Stored {
        Good(Option<PluginState>),
        Bad(serde::de::IgnoredAny),
    }
    Ok(
        match <Stored as serde::Deserialize>::deserialize(deserializer)? {
            Stored::Good(state) => state,
            Stored::Bad(_) => None,
        },
    )
}

/// What a fresh instance gets restored with, in this order: the opaque state
/// blob, then the parameter values it does not already cover.
pub(crate) struct Restore<'a> {
    /// Decoded state bytes, if the pref holds a usable blob.
    pub(crate) state: Option<Vec<u8>>,
    pref: &'a PluginPref,
}

impl<'a> Restore<'a> {
    pub(crate) fn from_pref(pref: &'a PluginPref) -> Self {
        Self {
            state: pref.state.as_ref().and_then(PluginState::bytes),
            pref,
        }
    }

    /// The parameter values to apply once the state blob was (or was not)
    /// loaded.  `None` asks for the parameter's default.
    pub(crate) fn params(&self, state_loaded: bool) -> Vec<(u32, Option<f64>)> {
        let snapshot = if state_loaded {
            self.pref
                .state
                .as_ref()
                .map(|state| state.params.as_slice())
        } else {
            None
        };
        params_to_restore(&self.pref.params, snapshot)
    }
}

/// The values two parameter lists may differ by and still count as equal.
const PARAM_EPSILON: f64 = 0.0005;

/// Which saved parameters still need applying.  Without a restored state
/// blob that is every saved value.  After one, it is only what changed since
/// the blob was captured: a value that differs from (or is missing in) the
/// blob's snapshot, and a parameter that went back to its default (`None`),
/// which the saved list no longer names.  An unchanged list applies nothing,
/// so the blob takes precedence.
pub(crate) fn params_to_restore(
    saved: &[PluginParamPref],
    snapshot: Option<&[PluginParamPref]>,
) -> Vec<(u32, Option<f64>)> {
    let Some(snapshot) = snapshot else {
        return saved
            .iter()
            .map(|saved| (saved.id, Some(saved.value)))
            .collect();
    };
    let changed = saved.iter().filter(|saved| {
        !snapshot.iter().any(|captured| {
            captured.id == saved.id && (captured.value - saved.value).abs() <= PARAM_EPSILON
        })
    });
    let reset = snapshot
        .iter()
        .filter(|captured| !saved.iter().any(|saved| saved.id == captured.id));
    changed
        .map(|saved| (saved.id, Some(saved.value)))
        .chain(reset.map(|captured| (captured.id, None)))
        .collect()
}

/// The saved parameter list after reading the plugin's live controls: every
/// control away from its default, plus any saved entry the plugin does not
/// list (an id hidden or renamed by this plugin version), which a sync must
/// not silently delete.
pub(crate) fn merge_synced_params(
    previous: &[PluginParamPref],
    live: &[PluginParamView],
) -> Vec<PluginParamPref> {
    let unseen = previous
        .iter()
        .filter(|saved| !live.iter().any(|parameter| parameter.id == saved.id))
        .cloned();
    live.iter()
        .filter(|parameter| (parameter.value - parameter.default).abs() > PARAM_EPSILON)
        .map(|parameter| PluginParamPref {
            id: parameter.id,
            value: parameter.value,
        })
        .chain(unseen)
        .collect()
}

const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub(crate) fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (index, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if index <= chunk.len() {
                out.push(BASE64_ALPHABET[((n >> shift) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

pub(crate) fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let text = text.trim().as_bytes();
    if !text.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    for (chunk_index, chunk) in text.chunks(4).enumerate() {
        let last = chunk_index + 1 == text.len() / 4;
        let padding = chunk.iter().rev().take_while(|&&c| c == b'=').count();
        if padding > 2 || (padding > 0 && !last) {
            return None;
        }
        let mut n = 0u32;
        for &c in &chunk[..4 - padding] {
            let value = BASE64_ALPHABET.iter().position(|&a| a == c)? as u32;
            n = (n << 6) | value;
        }
        n <<= 6 * padding as u32;
        let decoded = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend_from_slice(&decoded[..3 - padding]);
    }
    Some(out)
}

/// One effect in the insert chain, in the form that is saved to preferences.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PluginPref {
    pub path: String,
    #[serde(default)]
    pub format: PluginFormat,
    #[serde(default)]
    pub class_id: Option<String>,
    /// Filled in from the file name when a saved entry lacks it.
    #[serde(default)]
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
    /// The plugin's opaque state from the last save.  When it restores, it
    /// takes precedence over `params` (see [`params_to_restore`]).  Older
    /// preferences have none, and a damaged one is read as none.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient_state"
    )]
    pub state: Option<PluginState>,
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
            state: None,
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
    /// Read once at load, so drawing the insert list never locks a plugin.
    has_editor: bool,
    /// A CLAP plugin's callback state, so its main-thread work can run
    /// without the chain lock or the instance lock.
    clap_host: Option<Arc<HostState>>,
    /// Whether `pref.state` may no longer describe the live plugin: it was
    /// missing or did not restore, or a control changed since.  The next
    /// preference save captures a fresh blob.
    state_stale: bool,
}

impl Slot {
    fn empty(pref: PluginPref) -> Self {
        Self {
            pref,
            plugin: None,
            error: None,
            parameters: Vec::new(),
            has_editor: false,
            clap_host: None,
            state_stale: false,
        }
    }

    fn loaded(pref: PluginPref, sample_rate: u32) -> Self {
        match load_plugin(&pref, sample_rate) {
            Ok((plugin, state_current)) => {
                let parameters = host_parameters(&plugin);
                let has_editor = host_has_editor(&plugin);
                let clap_host = match &plugin {
                    PluginHandle::Clap(clap) => Some(
                        clap.lock()
                            .unwrap_or_else(|poison| poison.into_inner())
                            .host(),
                    ),
                    PluginHandle::Vst3(_) => None,
                };
                Self {
                    pref,
                    plugin: Some(plugin),
                    error: None,
                    parameters,
                    has_editor,
                    clap_host,
                    state_stale: !state_current,
                }
            }
            Err(error) => Self {
                error: Some(error),
                ..Self::empty(pref)
            },
        }
    }

    fn active(&self) -> bool {
        !self.pref.bypass && self.error.is_none() && self.plugin.is_some() && !self.ports_changed()
    }

    /// Whether this slot's CLAP plugin invalidated its port buffers.  The
    /// insert is left out of the chain and the panel shows the error until it
    /// is removed and added again.
    fn ports_changed(&self) -> bool {
        self.clap_host
            .as_ref()
            .is_some_and(|host| host.ports_changed())
    }

    /// The first error the slot should show: a load/processing failure, or a
    /// port change that has not yet reached `process`.
    fn resolved_error(&self) -> Option<String> {
        self.error
            .clone()
            .or_else(|| self.ports_changed().then(|| AUDIO_PORTS_CHANGED.to_owned()))
    }

    /// Opens a saved slot on demand (the configure window may be opened before
    /// the bridge has started) and refreshes its control list so the panel
    /// shows the values the plugin has right now.  A slot that failed stays
    /// failed.
    fn ensure_loaded(&mut self, sample_rate: u32) {
        if !self.load_if_needed(sample_rate) {
            if let Some(parameters) = self.plugin.as_ref().map(host_parameters) {
                self.parameters = parameters;
            }
        }
    }

    /// Opens a never-loaded slot.  Returns whether it loaded one, which also
    /// read the fresh control list.
    fn load_if_needed(&mut self, sample_rate: u32) -> bool {
        if self.plugin.is_none() && self.error.is_none() {
            *self = Slot::loaded(self.pref.clone(), sample_rate);
            true
        } else {
            false
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
            error: self.resolved_error(),
            parameters: self.parameters.clone(),
            has_editor: self.plugin.is_some() && self.has_editor,
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

/// Opens the plugin with its saved state and controls applied.  The flag says
/// whether the saved state blob still describes the new instance exactly (it
/// restored and no newer control value had to be applied on top).
fn load_plugin(pref: &PluginPref, sample_rate: u32) -> Result<(PluginHandle, bool), String> {
    let restore = Restore::from_pref(pref);
    match pref.format {
        PluginFormat::Vst3 => load_vst3(pref, &restore, sample_rate)
            .map(|(plugin, current)| (PluginHandle::Vst3(Arc::new(Mutex::new(plugin))), current)),
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
            super::clap::ClapPlugin::load(&path, plugin_id, sample_rate, MAX_BLOCK, &restore).map(
                |(plugin, current)| (PluginHandle::Clap(Arc::new(Mutex::new(plugin))), current),
            )
        }
    }
}

fn load_vst3(
    pref: &PluginPref,
    restore: &Restore<'_>,
    sample_rate: u32,
) -> Result<(Plugin, bool), String> {
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
    // Restore the saved state first, then the controls it does not already
    // cover, all before processing starts.  A stale parameter id from an older
    // plugin version is skipped rather than failing the whole insert.
    let state_loaded = restore
        .state
        .as_deref()
        .is_some_and(|bytes| plugin.load_state(bytes).is_ok());
    let values = restore.params(state_loaded);
    if !values.is_empty() {
        let defaults = plugin.get_parameters().unwrap_or_default();
        for (id, value) in &values {
            let value = value.or_else(|| {
                defaults
                    .iter()
                    .find(|parameter| parameter.id == *id)
                    .map(|parameter| parameter.default)
            });
            if let Some(value) = value {
                let _ = plugin.set_parameter(*id, value.clamp(0.0, 1.0));
            }
        }
    }
    plugin
        .start_processing()
        .map_err(|error| format!("Start failed: {error}"))?;
    Ok((plugin, state_loaded && values.is_empty()))
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

    /// Replaces the slots and hands the old ones back, so the caller can
    /// drop (and destroy) them after releasing the chain lock.
    pub(crate) fn restore(&mut self, prefs: Vec<PluginPref>) -> Vec<Slot> {
        std::mem::replace(
            &mut self.slots,
            prefs.into_iter().map(Slot::empty).collect(),
        )
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

    /// Splices an already loaded slot into the chain, but only when it was
    /// built for the chain's current rate.  The caller holds the chain lock.
    /// A mismatched slot is handed back so the caller can release the lock
    /// before dropping it and rebuild at the new rate.
    pub(crate) fn push_slot(&mut self, slot: Slot, loaded_rate: u32) -> Result<(), Slot> {
        if loaded_rate != self.sample_rate {
            return Err(slot);
        }
        self.slots.push(slot);
        Ok(())
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

    /// Takes a slot out of the chain.  The caller drops it after releasing
    /// the chain lock: destroying a plugin can take a while, and the audio
    /// callback passes the chain through dry whenever it cannot get the lock.
    pub(crate) fn remove(&mut self, index: usize) -> Option<Slot> {
        (index < self.slots.len()).then(|| self.slots.remove(index))
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

    /// The CLAP plugins that need main-thread servicing.  The caller runs
    /// it with [`service_clap`] after releasing the chain lock.
    pub(crate) fn clap_services(&self) -> Vec<(Arc<HostState>, Arc<Mutex<ClapPlugin>>)> {
        self.slots
            .iter()
            .filter_map(|slot| match (&slot.clap_host, &slot.plugin) {
                (Some(host), Some(PluginHandle::Clap(plugin))) => {
                    Some((Arc::clone(host), Arc::clone(plugin)))
                }
                _ => None,
            })
            .collect()
    }

    /// The plugin behind one slot, for work that must not hold the chain
    /// lock.  Fails with the slot's own error when it is not loaded.
    pub(crate) fn loaded_handle(&self, index: usize) -> Result<PluginHandle, String> {
        let Some(slot) = self.slots.get(index) else {
            return Err("That insert is no longer in the chain".to_owned());
        };
        slot.plugin.clone().ok_or_else(|| {
            slot.resolved_error()
                .unwrap_or_else(|| "The plugin is not loaded".to_owned())
        })
    }

    /// Stores controls read with [`read_plugin_parameters`] back into the
    /// slot, provided it still holds the same plugin instance.
    pub(crate) fn apply_synced_parameters(
        &mut self,
        index: usize,
        handle: &PluginHandle,
        parameters: Vec<PluginParamView>,
    ) {
        let Some(slot) = self.slots.get_mut(index) else {
            return;
        };
        if !slot
            .plugin
            .as_ref()
            .is_some_and(|current| current.same(handle))
        {
            return;
        }
        slot.pref.params = merge_synced_params(&slot.pref.params, &parameters);
        slot.parameters = parameters;
        // The editor is open and can change more than parameters.
        slot.state_stale = true;
    }

    /// The loaded plugins whose state should be captured now: those whose
    /// saved blob is missing, did not restore, or is older than a control
    /// change, and CLAP plugins that marked themselves dirty.  The caller
    /// captures them with [`capture_state`] after releasing the chain lock
    /// and hands the results to [`Self::store_state`].
    pub(crate) fn state_requests(&self) -> Vec<StateRequest> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| {
                let handle = slot.plugin.clone()?;
                let dirty = slot
                    .clap_host
                    .as_ref()
                    .is_some_and(|host| host.state_dirty());
                (slot.state_stale || dirty).then(|| StateRequest {
                    index,
                    handle,
                    host: slot.clap_host.clone(),
                    params: slot.pref.params.clone(),
                })
            })
            .collect()
    }

    /// Stores a captured state blob, provided the slot still holds the same
    /// plugin instance.  A failed capture keeps the previous blob, whose
    /// parameter snapshot still lets newer control values win on restore,
    /// and is not retried until a control changes again.  A deferred one
    /// keeps the slot stale so the next save retries.
    pub(crate) fn store_state(&mut self, index: usize, handle: &PluginHandle, capture: Capture) {
        let Some(slot) = self.slots.get_mut(index) else {
            return;
        };
        if !slot
            .plugin
            .as_ref()
            .is_some_and(|current| current.same(handle))
        {
            return;
        }
        slot.state_stale = capture.leaves_stale();
        if let Capture::Saved(state) = capture {
            slot.pref.state = Some(state);
        }
    }

    /// Marks one slot's saved state as out of date, e.g. when its editor
    /// closes, so the next preference save captures it.
    pub(crate) fn mark_state_stale(&mut self, index: usize) {
        if let Some(slot) = self.slots.get_mut(index) {
            if slot.plugin.is_some() {
                slot.state_stale = true;
            }
        }
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
        slot.load_if_needed(rate);
        let Some(plugin) = slot.plugin.as_ref() else {
            return Err(slot
                .error
                .clone()
                .unwrap_or_else(|| "The plugin is not loaded".to_owned()));
        };
        let value = value.clamp(0.0, 1.0);
        host_set_parameter(plugin, id, value)?;
        // Only the moved control is refreshed; re-reading every parameter on
        // each drag event held the plugin (and the chain) far longer.
        let display = match plugin {
            PluginHandle::Vst3(vst3) => {
                let vst3 = vst3.lock().unwrap_or_else(|poison| poison.into_inner());
                vst3.format_parameter(id, value).ok()
            }
            PluginHandle::Clap(clap) => Some(
                clap.lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .display_for(id, value),
            ),
        };
        if let Some(parameter) = slot
            .parameters
            .iter_mut()
            .find(|parameter| parameter.id == id)
        {
            parameter.value = value;
            if let Some(display) = display {
                parameter.display = display;
            }
        }
        if let Some(saved) = slot.pref.params.iter_mut().find(|saved| saved.id == id) {
            saved.value = value;
        } else {
            slot.pref.params.push(PluginParamPref { id, value });
        }
        slot.state_stale = true;
        Ok(())
    }

    /// Returns every control to the plugin's own defaults and forgets the
    /// saved overrides.
    pub(crate) fn reset_parameters(&mut self, index: usize) -> Result<(), String> {
        let rate = self.sample_rate;
        let Some(slot) = self.slots.get_mut(index) else {
            return Err("That insert is no longer in the chain".to_owned());
        };
        slot.load_if_needed(rate);
        let Some(plugin) = slot.plugin.as_ref() else {
            return Err(slot
                .error
                .clone()
                .unwrap_or_else(|| "The plugin is not loaded".to_owned()));
        };
        slot.parameters = host_reset_parameters(plugin)?;
        slot.pref.params.clear();
        slot.state_stale = true;
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
            // The slot keeps its own reference while the audio thread holds
            // the chain lock, so dropping this clone is never the last
            // release: a plugin (and its CLAP module, whose release takes
            // the module registry lock and may `deinit`) is never torn down
            // on the audio thread.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn param(id: u32, value: f64) -> PluginParamPref {
        PluginParamPref { id, value }
    }

    fn view(id: u32, value: f64, default: f64) -> PluginParamView {
        PluginParamView {
            id,
            value,
            default,
            ..PluginParamView::default()
        }
    }

    #[test]
    fn base64_round_trips_every_length() {
        let data: Vec<u8> = (0..=255u8).collect();
        for len in 0..data.len() {
            let encoded = base64_encode(&data[..len]);
            assert_eq!(encoded.len() % 4, 0);
            assert_eq!(base64_decode(&encoded).as_deref(), Some(&data[..len]));
        }
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_decode("Zm8="), Some(b"fo".to_vec()));
    }

    #[test]
    fn base64_rejects_damage() {
        assert_eq!(base64_decode("Zm8"), None);
        assert_eq!(base64_decode("Zm*="), None);
        assert_eq!(base64_decode("Z==="), None);
        assert_eq!(base64_decode("Zg==Zm8="), None);
    }

    #[test]
    fn state_blob_needs_a_known_version_and_data() {
        let state = PluginState::capture(&[1, 2, 3], &[param(1, 0.5)]);
        assert_eq!(state.version, PLUGIN_STATE_VERSION);
        assert_eq!(state.bytes(), Some(vec![1, 2, 3]));
        assert_eq!(state.params, vec![param(1, 0.5)]);
        let newer = PluginState {
            version: PLUGIN_STATE_VERSION + 1,
            ..state.clone()
        };
        assert_eq!(newer.bytes(), None);
        let damaged = PluginState {
            data: "!!".to_owned(),
            ..state.clone()
        };
        assert_eq!(damaged.bytes(), None);
        assert_eq!(PluginState::capture(&[], &[]).bytes(), None);
    }

    #[test]
    fn pref_round_trips_with_and_without_state() {
        let mut pref = PluginPref {
            path: "C:/Plugins/Verb.clap".to_owned(),
            format: PluginFormat::Clap,
            class_id: Some("com.example.verb".to_owned()),
            params: vec![param(4, 0.75)],
            ..PluginPref::default()
        };
        let plain = ron::to_string(&pref).expect("serialize");
        assert!(!plain.contains("state"), "{plain}");
        let back: PluginPref = ron::from_str(&plain).expect("deserialize");
        assert!(back.state.is_none());
        assert_eq!(back.params, pref.params);

        pref.state = Some(PluginState::capture(b"\x00\xffstate", &pref.params));
        let text = ron::to_string(&pref).expect("serialize");
        let back: PluginPref = ron::from_str(&text).expect("deserialize");
        assert_eq!(back.state, pref.state);
        assert_eq!(
            back.state.and_then(|state| state.bytes()),
            Some(b"\x00\xffstate".to_vec())
        );
    }

    #[test]
    fn old_pref_without_state_still_loads() {
        let back: PluginPref = ron::from_str(
            r#"(path:"C:/A.clap",format:clap,class_id:Some("a"),params:[(id:1,value:0.3)])"#,
        )
        .expect("old layout");
        assert!(back.state.is_none());
        assert_eq!(back.params, vec![param(1, 0.3)]);
        let damaged: PluginPref =
            ron::from_str(r#"(path:"C:/A.clap",state:Some((version:"x")))"#).expect("damaged blob");
        assert!(damaged.state.is_none());
    }

    #[test]
    fn without_a_restored_state_every_saved_value_is_applied() {
        let saved = [param(1, 0.2), param(2, 0.9)];
        assert_eq!(
            params_to_restore(&saved, None),
            vec![(1, Some(0.2)), (2, Some(0.9))]
        );
    }

    #[test]
    fn a_restored_state_takes_precedence_over_unchanged_values() {
        let saved = [param(1, 0.2), param(2, 0.9)];
        assert!(params_to_restore(&saved, Some(&saved)).is_empty());
    }

    #[test]
    fn values_changed_after_the_capture_win_over_the_state() {
        let snapshot = [param(1, 0.2), param(2, 0.9), param(3, 0.4)];
        // 1 moved, 2 unchanged, 3 back to default (no longer saved), 4 new.
        let saved = [param(1, 0.6), param(2, 0.9), param(4, 0.1)];
        assert_eq!(
            params_to_restore(&saved, Some(&snapshot)),
            vec![(1, Some(0.6)), (4, Some(0.1)), (3, None)]
        );
    }

    #[test]
    fn restore_ignores_the_snapshot_when_the_state_did_not_load() {
        let pref = PluginPref {
            params: vec![param(1, 0.2)],
            state: Some(PluginState::capture(&[7], &[param(1, 0.2)])),
            ..PluginPref::default()
        };
        let restore = Restore::from_pref(&pref);
        assert_eq!(restore.state, Some(vec![7]));
        assert!(restore.params(true).is_empty());
        assert_eq!(restore.params(false), vec![(1, Some(0.2))]);
    }

    #[test]
    fn only_a_deferred_capture_keeps_the_slot_stale() {
        assert!(Capture::Deferred.leaves_stale());
        assert!(!Capture::Failed.leaves_stale());
        assert!(!Capture::Saved(PluginState::capture(&[1], &[])).leaves_stale());
    }

    #[test]
    fn a_kept_older_blob_still_restores_newer_values() {
        // A panel move was still queued at save time, so the capture was
        // deferred and the older blob (with its own snapshot) was kept.
        let pref = PluginPref {
            params: vec![param(1, 0.9)],
            state: Some(PluginState::capture(&[7], &[param(1, 0.2), param(2, 0.4)])),
            ..PluginPref::default()
        };
        assert_eq!(
            Restore::from_pref(&pref).params(true),
            vec![(1, Some(0.9)), (2, None)]
        );
    }

    #[test]
    fn sync_keeps_changed_values_and_ids_the_plugin_does_not_list() {
        let previous = [param(1, 0.8), param(9, 0.3)];
        let live = [view(1, 0.5, 0.5), view(2, 0.7, 0.1)];
        assert_eq!(
            merge_synced_params(&previous, &live),
            vec![param(2, 0.7), param(9, 0.3)]
        );
    }

    #[test]
    fn a_port_change_is_visible_without_processing_a_block() {
        let mut slot = Slot::empty(PluginPref::default());
        slot.clap_host = Some(Arc::new(HostState::for_test(true)));
        assert!(slot.ports_changed());
        assert_eq!(slot.resolved_error().as_deref(), Some(AUDIO_PORTS_CHANGED));

        slot.clap_host = Some(Arc::new(HostState::for_test(false)));
        assert!(!slot.ports_changed());
        assert!(slot.resolved_error().is_none());
    }
}
