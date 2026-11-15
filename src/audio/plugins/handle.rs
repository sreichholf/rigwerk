//! A loaded plugin in its two halves: the [`PluginProcessor`] the audio
//! thread runs inside the chain's slot ([`LoadedPlugin`]), and the
//! [`PluginHandle`] the UI reaches without the chain lock (the published
//! insert links, the editor window).  Format-neutral.
//!
//! The UI half keeps what used to live on the slot under the chain lock:
//! the control views, the saved values, the state blob and whether it is
//! stale.  That sits behind [`PluginUi`]'s own mutex, a UI-only lock: the
//! audio thread never takes it, so a control set, an editor sync, a state
//! capture or a preference save no longer touches the chain lock.  Lock
//! order on the UI side: chain → `UiSide` → the instance mutex.  Never the
//! reverse.
//!
//! This step still runs vst3-host and the CLAP instance as one object each
//! behind a mutex (the `*Legacy` arms): both halves hold a clone of that
//! `Arc<Mutex<_>>`, the audio half only `try_lock`s it, and the UI half
//! calls the existing helpers in `load`.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};

use vst3_host::{AudioBuffers, Plugin};

use super::super::clap::{ClapPlugin, HostState, StateSave};
use super::link::{ClapFailure, PluginShared, ProcessFailure};
use super::load::{
    captured_state, host_parameters, host_reset_parameters, host_set_parameter,
    merge_synced_params, read_plugin_parameters, service_clap, take_parameter_changes,
    take_parameter_values, Capture, ParamChange, Vst3Service, MAX_BLOCK,
};
use super::pause::BlockAction;
use super::pref::{PluginParamPref, PluginParamView, PluginState};
use super::slot::SlotControls;

/// The UI half's plugin object.
pub(crate) enum PluginInstance {
    Vst3Legacy(Arc<Mutex<Plugin>>),
    ClapLegacy(Arc<Mutex<ClapPlugin>>),
}

/// The audio half's plugin object (`Send`; it lives in the chain's slot).
pub(crate) enum PluginProcessor {
    Vst3Legacy {
        plugin: Arc<Mutex<Plugin>>,
        /// Sized for [`MAX_BLOCK`] at the load rate (a rate change rebuilds
        /// the slot).
        buffers: AudioBuffers,
        /// A failure whose error owns heap data, kept here so the audio
        /// thread never frees it; dropped with the slot on the UI thread.
        parked: Option<vst3_host::Error>,
    },
    ClapLegacy(Arc<Mutex<ClapPlugin>>),
}

impl Drop for PluginProcessor {
    /// Slots are dropped on the UI thread, so the text of a parked failure
    /// is logged here, once per insert.
    fn drop(&mut self) {
        if let PluginProcessor::Vst3Legacy {
            parked: Some(error),
            ..
        } = self
        {
            eprintln!("[plugins] a VST3 insert stopped processing: {error}");
        }
    }
}

/// The legacy hosts' main-thread services, outside the UI lock: their
/// latency and port flags are atomics, so the audio half reads them too.
enum LegacyService {
    Vst3(Arc<Vst3Service>),
    Clap(Arc<HostState>),
}

pub(crate) struct PluginUi {
    shared: Arc<PluginShared>,
    ui: Mutex<UiSide>,
    /// Read once at load, so drawing never locks a plugin.
    has_editor: bool,
    legacy: LegacyService,
}

/// What the UI half owns; behind [`PluginUi::ui`].
struct UiSide {
    instance: PluginInstance,
    views: Vec<PluginParamView>,
    /// The saved control values (was the slot's `pref.params`).
    saved: Vec<PluginParamPref>,
    /// The saved state blob (was the slot's `pref.state`).
    state: Option<PluginState>,
    /// Whether `state` may no longer describe the live plugin: it was
    /// missing or did not restore, or a control changed since.  The next
    /// preference save captures a fresh blob.
    state_stale: bool,
}

/// The UI's reference to one loaded plugin.  Cheap to clone; the slot, the
/// published link and an open editor each hold one.  UI thread only.
#[derive(Clone)]
pub(crate) struct PluginHandle(Arc<PluginUi>);

/// One loaded plugin as the chain's slot holds it.
pub(crate) struct LoadedPlugin {
    processor: PluginProcessor,
    handle: PluginHandle,
}

/// What one insert did with one chunk.
pub(crate) enum Outcome {
    /// The plugin's output (fully wet) is in the buffers.
    Processed,
    /// The buffers still hold the insert's input.
    Skipped,
    /// The plugin failed; the buffers hold its input.
    Failed,
}

/// What a fresh instance starts with on the UI side.
pub(crate) struct Saved {
    pub(crate) params: Vec<PluginParamPref>,
    pub(crate) state: Option<PluginState>,
    pub(crate) stale: bool,
}

impl LoadedPlugin {
    /// A loaded, started vst3-host plugin.
    pub(crate) fn vst3_legacy(plugin: Plugin, saved: Saved, sample_rate: u32) -> Self {
        let views = super::load::parameter_views(&plugin);
        let has_editor = plugin.has_editor();
        let plugin = Arc::new(Mutex::new(plugin));
        let service = Arc::new(Vst3Service::new(Arc::clone(&plugin)));
        let rate = if sample_rate == 0 {
            48_000
        } else {
            sample_rate
        };
        Self::assemble(
            PluginProcessor::Vst3Legacy {
                plugin: Arc::clone(&plugin),
                buffers: AudioBuffers::new(2, 2, MAX_BLOCK, rate as f64),
                parked: None,
            },
            PluginInstance::Vst3Legacy(plugin),
            LegacyService::Vst3(service),
            views,
            has_editor,
            saved,
        )
    }

    /// A loaded, activated CLAP plugin.
    pub(crate) fn clap_legacy(plugin: ClapPlugin, saved: Saved) -> Self {
        let views = plugin.parameters();
        let has_editor = plugin.has_editor();
        Self::clap_with(plugin, views, has_editor, saved)
    }

    /// A CLAP plugin the way the chain tests insert one: no controls read,
    /// no editor, nothing saved.
    #[cfg(test)]
    pub(crate) fn clap_for_test(plugin: ClapPlugin) -> Self {
        Self::clap_with(
            plugin,
            Vec::new(),
            false,
            Saved {
                params: Vec::new(),
                state: None,
                stale: false,
            },
        )
    }

    /// As [`Self::clap_for_test`], restoring the saved values and state of
    /// `pref` the way [`super::load::load_plugin`] hands them over.
    #[cfg(test)]
    pub(crate) fn clap_restoring_for_test(
        plugin: ClapPlugin,
        pref: &super::pref::PluginPref,
    ) -> Self {
        Self::clap_with(
            plugin,
            Vec::new(),
            false,
            Saved {
                params: pref.params.clone(),
                state: pref.state.clone(),
                stale: false,
            },
        )
    }

    fn clap_with(
        plugin: ClapPlugin,
        views: Vec<PluginParamView>,
        has_editor: bool,
        saved: Saved,
    ) -> Self {
        let host = plugin.host();
        let plugin = Arc::new(Mutex::new(plugin));
        Self::assemble(
            PluginProcessor::ClapLegacy(Arc::clone(&plugin)),
            PluginInstance::ClapLegacy(plugin),
            LegacyService::Clap(host),
            views,
            has_editor,
            saved,
        )
    }

    fn assemble(
        processor: PluginProcessor,
        instance: PluginInstance,
        legacy: LegacyService,
        views: Vec<PluginParamView>,
        has_editor: bool,
        saved: Saved,
    ) -> Self {
        Self {
            processor,
            handle: PluginHandle(Arc::new(PluginUi {
                // The legacy arms read their latency from `legacy`.
                shared: Arc::new(PluginShared::new(0)),
                ui: Mutex::new(UiSide {
                    instance,
                    views,
                    saved: saved.params,
                    state: saved.state,
                    state_stale: saved.stale,
                }),
                has_editor,
                legacy,
            })),
        }
    }

    pub(crate) fn handle(&self) -> &PluginHandle {
        &self.handle
    }

    /// Lock and allocation free, so the audio thread may call it.
    pub(crate) fn shared(&self) -> &PluginShared {
        &self.handle.0.shared
    }

    /// The latency the plugin last reported (atomics; audio-safe).
    pub(crate) fn latency(&self) -> u32 {
        self.handle.0.latency()
    }

    /// Whether the plugin's ports no longer match its buffers (atomics).
    pub(crate) fn ports_changed(&self) -> bool {
        self.handle.0.ports_changed()
    }

    /// Audio thread, one chunk of at most [`MAX_BLOCK`] frames, in place.
    /// Every loaded insert is brought here every chunk, so its pause
    /// handshake is served even while it is skipped; only `wet` runs the
    /// plugin.  A plugin whose instance lock is busy (its editor or a UI
    /// call holds it) is skipped for the chunk and counted on `controls`;
    /// a failure is recorded in [`PluginShared`] and never allocates.
    pub(crate) fn run(
        &mut self,
        controls: &SlotControls,
        left: &mut [f32],
        right: &mut [f32],
        wet: bool,
    ) -> Outcome {
        let Self { processor, handle } = self;
        let shared = &*handle.0.shared;
        let block = shared.pause.begin_block();
        // The legacy arms are never asked to pause (their restarts stay in
        // vst3-host and the CLAP instance) and start processing at load or
        // lazily inside `process`, so the start and stop calls are no-ops.
        match block.action() {
            BlockAction::Process => {}
            BlockAction::Start => shared.pause.started(),
            BlockAction::Stop => {
                shared.pause.stopped();
                return Outcome::Skipped;
            }
            BlockAction::Skip => return Outcome::Skipped,
        }
        if !wet {
            return Outcome::Skipped;
        }
        controls.note_attempt();
        let count = left.len().min(right.len());
        match processor {
            PluginProcessor::Vst3Legacy {
                plugin,
                buffers,
                parked,
            } => {
                let mut plugin = match plugin.try_lock() {
                    Ok(plugin) => plugin,
                    Err(TryLockError::Poisoned(poison)) => poison.into_inner(),
                    Err(TryLockError::WouldBlock) => {
                        controls.note_lock_skip();
                        return Outcome::Skipped;
                    }
                };
                buffers.block_size = count;
                // The VST3 host works out how many frames to run from the
                // length of these vectors, not from `block_size`.  They are
                // allocated at MAX_BLOCK, so without trimming them to
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
                buffers.inputs[0].copy_from_slice(&left[..count]);
                buffers.inputs[1].copy_from_slice(&right[..count]);
                buffers.outputs[0].fill(0.0);
                buffers.outputs[1].fill(0.0);
                match plugin.process_audio(buffers) {
                    Ok(()) => {
                        left[..count].copy_from_slice(&buffers.outputs[0]);
                        right[..count].copy_from_slice(&buffers.outputs[1]);
                        Outcome::Processed
                    }
                    Err(error) => {
                        drop(plugin);
                        let failure = match &error {
                            vst3_host::Error::ProcessFailed(code) => ProcessFailure::Vst3(*code),
                            vst3_host::Error::NotProcessing => ProcessFailure::Vst3Stopped,
                            _ => ProcessFailure::Vst3Host,
                        };
                        shared.fail(failure);
                        // A failed slot is no longer active, so it is never
                        // run wet again: `parked` was empty and the
                        // assignment frees nothing here.
                        debug_assert!(parked.is_none());
                        *parked = Some(error);
                        Outcome::Failed
                    }
                }
            }
            PluginProcessor::ClapLegacy(plugin) => {
                let mut plugin = match plugin.try_lock() {
                    Ok(plugin) => plugin,
                    Err(TryLockError::Poisoned(poison)) => poison.into_inner(),
                    Err(TryLockError::WouldBlock) => {
                        controls.note_lock_skip();
                        return Outcome::Skipped;
                    }
                };
                match plugin.process(left, right, count, 1.0) {
                    Ok(true) => Outcome::Processed,
                    Ok(false) => Outcome::Skipped,
                    Err(message) => {
                        drop(plugin);
                        shared.fail(ProcessFailure::Clap(ClapFailure::from_message(message)));
                        Outcome::Failed
                    }
                }
            }
        }
    }
}

impl PluginUi {
    fn latency(&self) -> u32 {
        match &self.legacy {
            LegacyService::Vst3(service) => service.latency(),
            LegacyService::Clap(host) => host.latency(),
        }
    }

    fn ports_changed(&self) -> bool {
        self.shared.ports_changed()
            || matches!(&self.legacy, LegacyService::Clap(host) if host.ports_changed())
    }
}

impl PluginHandle {
    fn ui(&self) -> MutexGuard<'_, UiSide> {
        self.0.ui.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Identifies this plugin instance in [`super::PluginView::instance`].
    /// It is the address of the shared UI half, so it stays unique for as
    /// long as anyone (an open editor, for one) holds a clone of the handle.
    pub(crate) fn instance_id(&self) -> usize {
        Arc::as_ptr(&self.0) as *const () as usize
    }

    pub(crate) fn has_editor(&self) -> bool {
        self.0.has_editor
    }

    /// Atomics only.
    pub(crate) fn latency(&self) -> u32 {
        self.0.latency()
    }

    /// Atomics only.
    pub(crate) fn ports_changed(&self) -> bool {
        self.0.ports_changed()
    }

    /// The first processing failure the audio thread recorded (atomics).
    pub(crate) fn failure(&self) -> Option<ProcessFailure> {
        self.0.shared.failure()
    }

    /// The control views as last read or set.
    pub(crate) fn parameters(&self) -> Vec<PluginParamView> {
        self.ui().views.clone()
    }

    /// Reads every control again, so the panel shows the plugin's values
    /// right now.
    pub(crate) fn refresh_parameters(&self) {
        let mut ui = self.ui();
        ui.views = host_parameters(&ui.instance);
    }

    /// Sets one normalized control and keeps the views and the saved value
    /// in step.  Returns the plugin's own text for the new value.
    pub(crate) fn set_parameter(&self, id: u32, value: f64) -> Result<Option<String>, String> {
        let value = value.clamp(0.0, 1.0);
        let mut ui = self.ui();
        host_set_parameter(&ui.instance, id, value)?;
        // Only the moved control is refreshed: re-reading every parameter
        // on each drag event holds the plugin far longer.
        let display = super::load::host_format_parameter(&ui.instance, id, value);
        if let Some(parameter) = ui.views.iter_mut().find(|parameter| parameter.id == id) {
            parameter.value = value;
            if let Some(display) = &display {
                parameter.display = display.clone();
            }
        }
        if let Some(saved) = ui.saved.iter_mut().find(|saved| saved.id == id) {
            saved.value = value;
        } else {
            ui.saved.push(PluginParamPref { id, value });
        }
        ui.state_stale = true;
        drop(ui);
        self.nudge();
        Ok(display)
    }

    /// Returns every control to the plugin's own defaults and forgets the
    /// saved overrides.
    pub(crate) fn reset_parameters(&self) -> Result<Vec<PluginParamView>, String> {
        let mut ui = self.ui();
        ui.views = host_reset_parameters(&ui.instance)?;
        ui.saved.clear();
        ui.state_stale = true;
        let views = ui.views.clone();
        drop(ui);
        self.nudge();
        Ok(views)
    }

    /// Reads every control and stores the result as the views and the
    /// saved values (see [`read_plugin_parameters`]).  The VST3 change
    /// reporter is drained first, without formatting: a value queued before
    /// this fresher read would otherwise be applied by the next change poll,
    /// putting an older value back.  `None` while a CLAP value still waits
    /// for the next block; nothing is stored then.
    pub(crate) fn sync_full(&self) -> Option<Vec<PluginParamView>> {
        let mut ui = self.ui();
        drop(take_parameter_values(&ui.instance));
        let parameters = read_plugin_parameters(&ui.instance)?;
        ui.saved = merge_synced_params(&ui.saved, &parameters);
        ui.views = parameters;
        // The editor is open and can change more than parameters.
        ui.state_stale = true;
        let views = ui.views.clone();
        drop(ui);
        self.nudge();
        Some(views)
    }

    /// The control changes a VST3 plugin reported since the last call (see
    /// [`take_parameter_changes`]); `None` for a format without a change
    /// feed.  Nothing is stored.
    pub(crate) fn take_changes(&self) -> Option<Vec<ParamChange>> {
        take_parameter_changes(&self.ui().instance)
    }

    pub(crate) fn is_vst3(&self) -> bool {
        matches!(self.0.legacy, LegacyService::Vst3(_))
    }

    /// Tells a VST3 plugin's service that it was just touched, so it looks
    /// for restart requests for a while (see [`Vst3Service::nudge`]).
    pub(crate) fn nudge(&self) {
        if let LegacyService::Vst3(service) = &self.0.legacy {
            service.nudge();
        }
    }

    /// Takes a VST3 plugin's "read every control" request (see
    /// [`Vst3Service::take_parameters_changed`]).
    pub(crate) fn take_parameters_changed(&self) -> bool {
        match &self.0.legacy {
            LegacyService::Vst3(service) => service.take_parameters_changed(),
            LegacyService::Clap(_) => false,
        }
    }

    /// The plugin's main-thread work, once per UI frame: CLAP callbacks,
    /// timers and pending lifecycle work, VST3 restart requests.  Returns
    /// whether the latency changed since the last call, so the caller can
    /// republish the insert views.  Never takes the chain lock.
    pub(crate) fn service(&self) -> bool {
        match &self.0.legacy {
            LegacyService::Clap(host) => {
                let plugin = match &self.ui().instance {
                    PluginInstance::ClapLegacy(plugin) => Arc::clone(plugin),
                    PluginInstance::Vst3Legacy(_) => return false,
                };
                let changed = service_clap(host, &plugin);
                // Taken from the host as well, so a change an open editor's
                // own service picked up counts here too.
                host.take_latency_change() | changed
            }
            LegacyService::Vst3(service) => service.service(),
        }
    }

    /// Whether the saved state blob needs a capture: stale, or the plugin
    /// marked its state dirty.
    pub(crate) fn wants_capture(&self) -> bool {
        self.ui().state_stale
            || self.0.shared.state_dirty()
            || matches!(&self.0.legacy, LegacyService::Clap(host) if host.state_dirty())
    }

    /// Captures the plugin's opaque state into the saved blob, with the
    /// saved values as its snapshot.  A failed capture keeps the previous
    /// blob, whose snapshot still lets newer values win on restore, and is
    /// not retried until a control changes again; a deferred one stays
    /// stale so the next save retries.  CLAP saves without the instance
    /// lock, so the audio callback keeps the insert; VST3 locks it.
    pub(crate) fn capture_state(&self) {
        let mut ui = self.ui();
        let bytes = match (&ui.instance, &self.0.legacy) {
            (PluginInstance::ClapLegacy(_), LegacyService::Clap(host)) => match host.save_state() {
                StateSave::Saved(bytes) => Ok(bytes),
                StateSave::Unavailable => Err(Capture::Failed),
                StateSave::Deferred => Err(Capture::Deferred),
            },
            (PluginInstance::Vst3Legacy(plugin), _) => plugin
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .save_state()
                .map_err(|_| Capture::Failed),
            (PluginInstance::ClapLegacy(_), _) => Err(Capture::Failed),
        };
        let capture = match bytes {
            Ok(bytes) => captured_state(&bytes, &ui.saved),
            Err(capture) => capture,
        };
        ui.state_stale = capture.leaves_stale();
        if let Capture::Saved(state) = capture {
            ui.state = Some(state);
        }
    }

    /// The saved values and state blob, for a preference save.
    pub(crate) fn saved(&self) -> (Vec<PluginParamPref>, Option<PluginState>) {
        let ui = self.ui();
        (ui.saved.clone(), ui.state.clone())
    }

    /// Whether the saved values and state blob are `params` and `state`,
    /// compared in place.
    pub(crate) fn saved_is(&self, params: &[PluginParamPref], state: Option<&PluginState>) -> bool {
        let ui = self.ui();
        ui.saved == params && ui.state.as_ref() == state
    }

    /// Marks the saved state as out of date (its editor closed), so the
    /// next preference save captures it.
    pub(crate) fn mark_state_stale(&self) {
        self.ui().state_stale = true;
        self.nudge();
    }

    /// The vst3-host instance, for its editor window (`app::plugin_gui`).
    pub(crate) fn legacy_vst3(&self) -> Option<Arc<Mutex<Plugin>>> {
        match &self.ui().instance {
            PluginInstance::Vst3Legacy(plugin) => Some(Arc::clone(plugin)),
            PluginInstance::ClapLegacy(_) => None,
        }
    }

    /// The CLAP instance, for its editor window (`app::plugin_gui`).
    pub(crate) fn legacy_clap(&self) -> Option<Arc<Mutex<ClapPlugin>>> {
        match &self.ui().instance {
            PluginInstance::ClapLegacy(plugin) => Some(Arc::clone(plugin)),
            PluginInstance::Vst3Legacy(_) => None,
        }
    }
}
