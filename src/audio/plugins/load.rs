//! Plugin handles, loading, state capture, and host parameter helpers.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, TryLockError};
use std::time::Instant;

use vst3_host::{Plugin, Vst3Host};

use super::super::clap::{ClapPlugin, HostState, StateSave};
use super::clamp_latency;
use super::pref::{PluginFormat, PluginParamPref, PluginParamView, PluginPref, PluginState};

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
    /// Identifies this plugin instance in [`super::PluginView::instance`].  It is
    /// the address of the shared instance, so it stays unique for as long as
    /// anyone (an open editor, for one) holds a clone of this handle.
    pub(crate) fn instance_id(&self) -> usize {
        match self {
            PluginHandle::Vst3(plugin) => instance_id(plugin),
            PluginHandle::Clap(plugin) => instance_id(plugin),
        }
    }

    pub(crate) fn same(&self, other: &PluginHandle) -> bool {
        match (self, other) {
            (PluginHandle::Vst3(a), PluginHandle::Vst3(b)) => Arc::ptr_eq(a, b),
            (PluginHandle::Clap(a), PluginHandle::Clap(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}

/// The identity [`super::PluginView::instance`] reports for a shared plugin
/// instance.  An editor that holds the `Arc` compares its own against the
/// published view instead of fetching the handle under the chain lock; the
/// address cannot be reused while that `Arc` keeps the instance alive.
pub(crate) fn instance_id<T>(plugin: &Arc<T>) -> usize {
    Arc::as_ptr(plugin) as *const () as usize
}

/// Reads a plugin's current controls in full, for
/// [`PluginChain::apply_synced_parameters`].  This reads and formats every
/// control, so callers use it when a change poll cannot see the edits: a CLAP
/// editor (no change feed), a stopped bridge, or an insert that has not
/// processed a block, where VST3 reports parameter changes only after it has
/// processed one.  An open, processing VST3 editor polls
/// [`take_parameter_changes`] instead.
/// Returns `None` while a CLAP value (one moved in RigWerk's panel, or a
/// saved one restored into a plugin without `params.flush`) is still waiting
/// for the next audio block, because reading the plugin then would show the
/// old value and the sync would save it.
pub(crate) fn read_plugin_parameters(handle: &PluginHandle) -> Option<Vec<PluginParamView>> {
    if let PluginHandle::Clap(clap) = handle {
        let clap = clap.lock().unwrap_or_else(|poison| poison.into_inner());
        if clap.has_pending_parameters() {
            return None;
        }
    }
    Some(host_parameters(handle))
}

/// One control value a plugin reported as changed, with the text its own
/// formatter gives it.
pub(crate) struct ParamChange {
    pub(crate) id: u32,
    pub(crate) value: f64,
    pub(crate) display: Option<String>,
}

/// Drains the control changes a VST3 plugin reported since the last call (its
/// editor's edits and the processor's own output parameters), formatting each
/// while the instance is still locked.  A drag and the processor's feedback
/// can report the same id many times, so the last value per id is kept and
/// only that one is formatted.  `Some(vec![])` means nothing changed; `None`
/// means the format has no equivalent.  VST3 only becomes visible after a
/// processed block, so an insert that has not processed one uses the full
/// [`read_plugin_parameters`] read instead.
pub(crate) fn take_parameter_changes(handle: &PluginHandle) -> Option<Vec<ParamChange>> {
    let PluginHandle::Vst3(plugin) = handle else {
        return None;
    };
    let plugin = plugin.lock().unwrap_or_else(|poison| poison.into_inner());
    let changes = plugin.get_parameter_changes();
    if changes.is_empty() {
        return Some(Vec::new());
    }
    Some(
        latest_parameter_values(changes)
            .into_iter()
            .map(|(id, value)| ParamChange {
                id,
                value,
                display: plugin.format_parameter(id, value).ok(),
            })
            .collect(),
    )
}

/// Collapses reported `(id, value)` pairs to the last value per id, in id
/// order.  A drag reports many values for one control before the next poll,
/// and a processor's output parameter can echo them; formatting each would
/// hold the instance lock far longer than needed.
pub(crate) fn latest_parameter_values(changes: Vec<(u32, f64)>) -> Vec<(u32, f64)> {
    let mut latest = std::collections::BTreeMap::new();
    for (id, value) in changes {
        latest.insert(id, value);
    }
    latest.into_iter().collect()
}

/// Drains the control changes a VST3 plugin reported since the last call,
/// without formatting any of them.  A sync that reads every control anyway
/// only needs the queue emptied: a value queued before the fresher full read
/// would otherwise be applied by the next change poll, putting an older value
/// back.  Formatting is the expensive part of [`take_parameter_changes`] and
/// would be thrown away here.  `None` means the format has no change feed.
pub(crate) fn take_parameter_values(handle: &PluginHandle) -> Option<Vec<(u32, f64)>> {
    let PluginHandle::Vst3(plugin) = handle else {
        return None;
    };
    let plugin = plugin.lock().unwrap_or_else(|poison| poison.into_inner());
    Some(latest_parameter_values(plugin.get_parameter_changes()))
}

/// Applies editor-reported changes to the cached views and the saved values,
/// touching only the ids they name.  Every known change is applied; the
/// return value is `false` when any id was not in the cached views (a control
/// the plugin added or renamed), which asks the caller for a full read.  The
/// saved list keeps [`merge_synced_params`]'s semantics: every live value
/// away from its default, plus saved entries the plugin does not list.
/// Changes that are already at their default therefore leave it.
pub(crate) fn apply_parameter_changes(
    parameters: &mut [PluginParamView],
    saved: &mut Vec<PluginParamPref>,
    changes: &[ParamChange],
) -> bool {
    if changes.is_empty() {
        return true;
    }
    let mut all_known = true;
    for change in changes {
        match parameters
            .iter_mut()
            .find(|parameter| parameter.id == change.id)
        {
            Some(parameter) => {
                parameter.value = change.value;
                if let Some(display) = &change.display {
                    parameter.display = display.clone();
                }
            }
            None => all_known = false,
        }
    }
    *saved = merge_synced_params(saved, parameters);
    all_known
}

/// Runs one CLAP plugin's main-thread work.  Callbacks and timers run without
/// any lock; only a pending flush, rescan, restart or latency re-read takes
/// the instance lock, and only when the audio callback is not holding it.
///
/// Returns whether the plugin's latency changed, so the caller can
/// republish the insert views (see [`super::PluginView::latency`]).
pub(crate) fn service_clap(host: &HostState, plugin: &Mutex<ClapPlugin>) -> bool {
    let before = host.latency();
    if !host.service_main_thread() {
        return false;
    }
    match plugin.try_lock() {
        Ok(mut plugin) => plugin.service_pending(),
        Err(TryLockError::Poisoned(poison)) => poison.into_inner().service_pending(),
        Err(TryLockError::WouldBlock) => {}
    }
    host.latency() != before
}

/// How often [`Vst3Service::service`] looks for `restartComponent` requests
/// while it has a reason to.  VST3 offers no lock-free way to peek at them,
/// and every look try-locks the instance the audio callback also try-locks
/// (a collision makes the callback skip the insert for a block).
const VST3_POLL_MS: u64 = 250;

/// How long after a parameter edit, editor sync or load the service keeps
/// looking.  A plugin raises `kLatencyChanged` in response to such an edit
/// (a mode switch, an oversampling toggle, a preset); with no edit there is
/// nothing to look for.  The 250 ms editor sync nudges this on every poll,
/// even one that reports no changes, so an open editor keeps the window open
/// for as long as it is shown.
const VST3_POLL_WINDOW_MS: u64 = 2_000;

/// How long a full read of the control list is held off after one was forced
/// by a reported id the cached views do not know.  A plugin can report an
/// output parameter it never lists, which would otherwise ask for a full read
/// (and format every control) on every poll.
const UNKNOWN_FULL_MS: u64 = 1_000;

/// Milliseconds since the first call, for the poll window atomics.
fn now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// A VST3 insert's main-thread work: `restartComponent` requests, and the
/// latency read after each activation.  Kept on the slot (like a CLAP
/// plugin's [`HostState`]) so the poll window survives a republish.
pub(crate) struct Vst3Service {
    pub(crate) plugin: Arc<Mutex<Plugin>>,
    /// `IAudioProcessor::getLatencySamples`, as last read.  Lock-free, so
    /// the audio thread may read it.
    latency: AtomicU32,
    /// Until when (see [`now_ms`]) the service looks for requests.
    poll_until: AtomicU64,
    /// When it may look next.
    next_poll: AtomicU64,
    /// Until when (see [`now_ms`]) the unknown-id full-read fallback waits
    /// (see [`UNKNOWN_FULL_MS`]).
    next_unknown_full: AtomicU64,
    /// Set when the plugin reported changed parameter values or titles, so
    /// the next editor sync reads every control instead of only the reported
    /// changes.  This covers an added, renamed or re-defaulted control
    /// without waiting for the editor to close.
    parameters_changed: AtomicBool,
    /// Whether a failed `service_host_requests` was logged already.
    error_logged: AtomicBool,
}

impl Vst3Service {
    /// Reads the latency of a freshly loaded (set up and activated) plugin.
    pub(crate) fn new(plugin: Arc<Mutex<Plugin>>) -> Self {
        let latency = plugin
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .latency_samples();
        let service = Self {
            plugin,
            latency: AtomicU32::new(clamp_latency(latency)),
            poll_until: AtomicU64::new(0),
            next_poll: AtomicU64::new(0),
            next_unknown_full: AtomicU64::new(0),
            parameters_changed: AtomicBool::new(false),
            error_logged: AtomicBool::new(false),
        };
        // Some plugins settle their latency right after loading.
        service.nudge();
        service
    }

    pub(crate) fn latency(&self) -> u32 {
        self.latency.load(Ordering::Acquire)
    }

    /// Takes the "read every control" request set by
    /// [`Self::service`].  The editor sync uses the full read once and then
    /// goes back to polling reported changes.  No unit test exercises this:
    /// building a [`Plugin`] needs a real VST3 module, and a stand-in would
    /// only test the `AtomicBool`.
    pub(crate) fn take_parameters_changed(&self) -> bool {
        self.parameters_changed.swap(false, Ordering::AcqRel)
    }

    /// Opens the poll window: something just touched the plugin (a control,
    /// a reset, a load), so it may raise a restart request.  The 250 ms
    /// editor sync calls this on every poll, changes or not, so an open
    /// editor keeps the window open for as long as it is shown.
    pub(crate) fn nudge(&self) {
        self.poll_until
            .store(now_ms() + VST3_POLL_WINDOW_MS, Ordering::Relaxed);
    }

    /// Whether the full-read fallback for a reported id the cached views do
    /// not know may run now, arming [`UNKNOWN_FULL_MS`].  Bounds a plugin
    /// that reports an output id it never lists.
    pub(crate) fn unknown_full_due(&self) -> bool {
        let now = now_ms();
        if now < self.next_unknown_full.load(Ordering::Relaxed) {
            return false;
        }
        self.next_unknown_full
            .store(now + UNKNOWN_FULL_MS, Ordering::Relaxed);
        true
    }

    /// Applies the plugin's `restartComponent` requests and re-reads its
    /// latency, but only inside the poll window (see [`Self::nudge`]) and at
    /// most every [`VST3_POLL_MS`]; otherwise it does nothing.  Call it on
    /// the UI thread, which loaded the plugin and is its VST3 control
    /// thread.  For `kLatencyChanged` (and `kIoChanged`)
    /// `service_host_requests` stops processing, deactivates, re-reads
    /// `getLatencySamples` and reactivates, as VST3 requires.  The instance
    /// is only try-locked, so a busy audio callback or editor just moves the
    /// work to a later poll.
    ///
    /// Returns whether the latency changed, so the caller can republish the
    /// insert views.
    pub(crate) fn service(&self) -> bool {
        let now = now_ms();
        if now > self.poll_until.load(Ordering::Relaxed)
            || now < self.next_poll.load(Ordering::Relaxed)
        {
            return false;
        }
        self.next_poll.store(now + VST3_POLL_MS, Ordering::Relaxed);
        let mut plugin = match self.plugin.try_lock() {
            Ok(plugin) => plugin,
            Err(TryLockError::Poisoned(poison)) => poison.into_inner(),
            Err(TryLockError::WouldBlock) => return false,
        };
        match plugin.service_host_requests() {
            Ok(flags) => {
                if flags.param_values_changed() || flags.param_titles_changed() {
                    self.parameters_changed.store(true, Ordering::Release);
                }
                if flags.reload_component() {
                    eprintln!(
                        "[plugins] a VST3 plugin asked to be reloaded; remove and add it again"
                    );
                }
            }
            // The latency may still have changed (the request can fail
            // halfway through the restart), so it is read either way.
            Err(error) => {
                if !self.error_logged.swap(true, Ordering::Relaxed) {
                    eprintln!("[plugins] a VST3 restart request failed: {error}");
                }
            }
        }
        let latency = clamp_latency(plugin.latency_samples());
        drop(plugin);
        self.latency.swap(latency, Ordering::AcqRel) != latency
    }
}

/// One plugin whose state [`PluginChain::state_requests`] wants captured.
pub(crate) struct StateRequest {
    pub(crate) index: usize,
    pub(crate) handle: PluginHandle,
    pub(crate) host: Option<Arc<HostState>>,
    /// The saved parameters at request time, kept with the blob as its
    /// snapshot.
    pub(crate) params: Vec<PluginParamPref>,
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
    pub(crate) fn leaves_stale(&self) -> bool {
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
pub(crate) const MAX_BLOCK: usize = 512;

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
/// writable control away from its default, plus any saved entry the plugin
/// does not list (an id hidden or renamed by this plugin version), which a
/// sync must not silently delete.  A read-only control is output the plugin
/// reports, not a setting this app owns, so it is never saved.
pub(crate) fn merge_synced_params(
    previous: &[PluginParamPref],
    live: &[PluginParamView],
) -> Vec<PluginParamPref> {
    let unseen = previous
        .iter()
        .filter(|saved| !live.iter().any(|parameter| parameter.id == saved.id))
        .cloned();
    live.iter()
        .filter(|parameter| {
            !parameter.read_only && (parameter.value - parameter.default).abs() > PARAM_EPSILON
        })
        .map(|parameter| PluginParamPref {
            id: parameter.id,
            value: parameter.value,
        })
        .chain(unseen)
        .collect()
}

pub(crate) fn host_parameters(handle: &PluginHandle) -> Vec<PluginParamView> {
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

pub(crate) fn host_has_editor(handle: &PluginHandle) -> bool {
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

pub(crate) fn host_set_parameter(handle: &PluginHandle, id: u32, value: f64) -> Result<(), String> {
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

/// The plugin's own text for one control value, `None` when it has none.
pub(crate) fn host_format_parameter(handle: &PluginHandle, id: u32, value: f64) -> Option<String> {
    match handle {
        PluginHandle::Vst3(vst3) => {
            let vst3 = vst3.lock().unwrap_or_else(|poison| poison.into_inner());
            vst3.format_parameter(id, value).ok()
        }
        PluginHandle::Clap(clap) => Some(
            clap.lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .display_for(id, value),
        ),
    }
}

pub(crate) fn host_reset_parameters(handle: &PluginHandle) -> Result<Vec<PluginParamView>, String> {
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
pub(crate) fn load_plugin(
    pref: &PluginPref,
    sample_rate: u32,
) -> Result<(PluginHandle, bool), String> {
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
            super::super::clap::ClapPlugin::load(&path, plugin_id, sample_rate, MAX_BLOCK, &restore)
                .map(|(plugin, current)| {
                    (PluginHandle::Clap(Arc::new(Mutex::new(plugin))), current)
                })
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
    // A value that was skipped as read-only leaves the blob as the only
    // record of that control, so the slot is not stale for it.
    let mut applied = false;
    if !values.is_empty() {
        let defaults = plugin.get_parameters().unwrap_or_default();
        // Read-only controls are the plugin's own output, not settings this
        // app owns: an old blob plus a failed capture must not push a saved
        // value, or a default reset, into one.  CLAP's restore skips these
        // too.
        let read_only: Vec<u32> = defaults
            .iter()
            .filter(|parameter| parameter.is_read_only)
            .map(|parameter| parameter.id)
            .collect();
        for (id, value) in &values {
            if read_only.contains(id) {
                continue;
            }
            let value = value.or_else(|| {
                defaults
                    .iter()
                    .find(|parameter| parameter.id == *id)
                    .map(|parameter| parameter.default)
            });
            if let Some(value) = value {
                applied |= plugin.set_parameter(*id, value.clamp(0.0, 1.0)).is_ok();
            }
        }
    }
    plugin
        .start_processing()
        .map_err(|error| format!("Start failed: {error}"))?;
    Ok((plugin, state_loaded && !applied))
}
