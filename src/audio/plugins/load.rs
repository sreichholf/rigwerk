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
/// nothing to look for.  An open editor syncs every 250 ms, so it keeps the
/// window open for as long as it is shown.
const VST3_POLL_WINDOW_MS: u64 = 2_000;

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
            error_logged: AtomicBool::new(false),
        };
        // Some plugins settle their latency right after loading.
        service.nudge();
        service
    }

    pub(crate) fn latency(&self) -> u32 {
        self.latency.load(Ordering::Acquire)
    }

    /// Opens the poll window: something just changed the plugin (a control,
    /// a reset, an editor sync, a load), so it may raise a restart request.
    pub(crate) fn nudge(&self) {
        self.poll_until
            .store(now_ms() + VST3_POLL_WINDOW_MS, Ordering::Relaxed);
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
            Ok(flags) if flags.reload_component() => {
                eprintln!("[plugins] a VST3 plugin asked to be reloaded; remove and add it again");
            }
            Ok(_) => {}
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
