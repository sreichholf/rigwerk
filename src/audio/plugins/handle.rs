//! A loaded plugin in its two halves: the [`PluginProcessor`] the audio
//! thread runs inside the chain's slot ([`LoadedPlugin`]), and the
//! [`PluginHandle`] the UI reaches without the chain lock (the published
//! insert links, the editor window).  Format-neutral.
//!
//! The UI half keeps the control views, the saved values, the state blob
//! and whether it is stale, next to the main-thread plugin object.  That
//! sits behind [`PluginUi`]'s own mutex, a UI-only lock: the audio thread
//! never takes it, so a control set, an editor sync, a state capture or a
//! preference save never touches a lock the audio thread takes.  Lock
//! order on the UI side: chain → `UiSide` → `ParamOut`.  Never the reverse.
//!
//! CLAP is split along its thread rules: the processor holds
//! `ClapProcessor`, the UI half `ClapInstance`, and they talk through the
//! parameter transfers, [`PluginShared`] and the pause handshake (a
//! restart, or a flush while not processing, pauses the plugin from the UI
//! and resumes it).  VST3 still runs vst3-host as one object behind a
//! mutex (the `Vst3Legacy` arms): both halves hold a clone of that
//! `Arc<Mutex<_>>`, the audio half only `try_lock`s it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};

use vst3_host::{AudioBuffers, Plugin};

use super::super::clap::{ClapInstance, ClapLoaded, ClapProcessor, StateSave};
use super::link::{
    ClapFailure, ParamIds, ParamOut, PluginShared, ProcessFailure, RESTART_ACTIVATION,
};
use super::load::{
    captured_state, host_format_parameter, host_parameters, host_reset_parameters,
    host_set_parameter, merge_synced_params, take_parameter_changes, take_parameter_values,
    Capture, ParamChange, Vst3Service, MAX_BLOCK,
};
use super::pause::{drive_pause, BlockAction, PauseStep};
use super::pref::{PluginFormat, PluginParamPref, PluginParamView, PluginState};
use super::slot::SlotControls;
use super::transfer::{param_transfer, ParamReceiver};

/// The UI half's plugin object.
pub(crate) enum PluginInstance {
    Vst3Legacy(Arc<Mutex<Plugin>>),
    Clap(ClapInstance),
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
    /// Boxed: it carries its port buffers' descriptions inline.
    Clap(Box<ClapProcessor>),
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

pub(crate) struct PluginUi {
    shared: Arc<PluginShared>,
    /// The UI→audio parameter producer (empty for vst3-host, which keeps
    /// its own queue).
    out: Arc<ParamOut>,
    ui: Mutex<UiSide>,
    format: PluginFormat,
    /// Read once at load, so drawing never locks a plugin.
    has_editor: bool,
    /// vst3-host's main-thread service; its latency is an atomic, so the
    /// audio half reads it too.
    vst3: Option<Arc<Vst3Service>>,
}

/// What the UI half owns; behind [`PluginUi::ui`].
struct UiSide {
    instance: PluginInstance,
    /// The audio→UI parameter queue: CLAP output values, for the views.
    from_audio: ParamReceiver,
    ids: Arc<ParamIds>,
    views: Vec<PluginParamView>,
    /// The saved control values (was the slot's `pref.params`).
    saved: Vec<PluginParamPref>,
    /// The saved state blob (was the slot's `pref.state`).
    state: Option<PluginState>,
    /// Whether `state` may no longer describe the live plugin: it was
    /// missing or did not restore, or a control changed since.  The next
    /// preference save captures a fresh blob.
    state_stale: bool,
    /// A pause the UI is driving (see `drive_pause`), kept between frames.
    pause_wait: Option<(std::time::Instant, u64)>,
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
        // vst3-host queues its own parameter changes: empty transfers.
        let ids = Arc::new(ParamIds::new(Vec::new()));
        let (sender, _) = param_transfer(0);
        let (_, from_audio) = param_transfer(0);
        Self {
            processor: PluginProcessor::Vst3Legacy {
                plugin: Arc::clone(&plugin),
                buffers: AudioBuffers::new(2, 2, MAX_BLOCK, rate as f64),
                parked: None,
            },
            handle: PluginHandle(Arc::new(PluginUi {
                // vst3-host reads its latency from `vst3`.
                shared: Arc::new(PluginShared::new(0)),
                out: Arc::new(ParamOut::new(Arc::clone(&ids), sender)),
                ui: Mutex::new(UiSide::new(
                    PluginInstance::Vst3Legacy(plugin),
                    from_audio,
                    ids,
                    views,
                    saved,
                )),
                format: PluginFormat::Vst3,
                has_editor,
                vst3: Some(service),
            })),
        }
    }

    /// A loaded, activated CLAP plugin; `shared` is the one it was loaded
    /// with.  Processing starts with the first block.
    pub(crate) fn clap(loaded: ClapLoaded, shared: Arc<PluginShared>, saved: Saved) -> Self {
        let ClapLoaded {
            instance,
            processor,
            out,
            from_audio,
            ids,
            views,
            has_editor,
            state_current: _,
        } = loaded;
        Self {
            processor: PluginProcessor::Clap(Box::new(processor)),
            handle: PluginHandle(Arc::new(PluginUi {
                shared,
                out,
                ui: Mutex::new(UiSide::new(
                    PluginInstance::Clap(instance),
                    from_audio,
                    ids,
                    views,
                    saved,
                )),
                format: PluginFormat::Clap,
                has_editor,
                vst3: None,
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
        self.handle.0.shared.ports_changed()
    }

    /// The CLAP processor, for tests that look at its buffers.
    #[cfg(test)]
    pub(crate) fn clap_processor_for_test(&mut self) -> &mut ClapProcessor {
        match &mut self.processor {
            PluginProcessor::Clap(processor) => processor,
            PluginProcessor::Vst3Legacy { .. } => panic!("not a CLAP plugin"),
        }
    }

    /// One fully wet [`Self::run`], with counters nobody reads.
    #[cfg(test)]
    pub(crate) fn run_for_test(&mut self, left: &mut [f32], right: &mut [f32]) -> Outcome {
        let controls = SlotControls::new(&super::pref::PluginPref::default());
        self.run(&controls, left, right, true)
    }

    /// Audio thread, one chunk of at most [`MAX_BLOCK`] frames, in place.
    /// Every loaded insert is brought here every chunk, so its pause
    /// handshake (start, stop, skip) is served even while it is skipped;
    /// only `wet` runs the plugin, and a skipped CLAP insert flushes the
    /// values queued for it.  A failure is recorded in [`PluginShared`] and
    /// never allocates.  A vst3-host plugin whose instance lock is busy is
    /// skipped for the chunk and counted on `controls`.  A CLAP plugin has
    /// no such lock.
    pub(crate) fn run(
        &mut self,
        controls: &SlotControls,
        left: &mut [f32],
        right: &mut [f32],
        wet: bool,
    ) -> Outcome {
        let Self { processor, handle } = self;
        let shared = &*handle.0.shared;
        if wet {
            controls.note_attempt();
        }
        let block = shared.pause.begin_block();
        let count = left.len().min(right.len());
        match processor {
            PluginProcessor::Clap(processor) => {
                match block.action() {
                    BlockAction::Process => {}
                    BlockAction::Start if shared.failure().is_some() => {
                        // A failed insert is not started again.
                        shared.pause.start_failed();
                        return Outcome::Skipped;
                    }
                    BlockAction::Start => {
                        if let Err(failure) = processor.start() {
                            // Paused and not processing: later blocks skip
                            // instead of processing a plugin that never
                            // started, and the insert shows why.
                            shared.pause.start_failed();
                            shared.fail(ProcessFailure::Clap(failure));
                            return Outcome::Failed;
                        }
                        shared.pause.started();
                    }
                    BlockAction::Stop => {
                        processor.stop();
                        shared.pause.stopped();
                        return Outcome::Skipped;
                    }
                    BlockAction::Skip => return Outcome::Skipped,
                }
                if !wet {
                    processor.flush_if_queued();
                    return Outcome::Skipped;
                }
                match processor.process(&mut left[..count], &mut right[..count]) {
                    Ok(()) => Outcome::Processed,
                    Err(failure) => {
                        shared.fail(ProcessFailure::Clap(failure));
                        Outcome::Failed
                    }
                }
            }
            PluginProcessor::Vst3Legacy {
                plugin,
                buffers,
                parked,
            } => {
                // vst3-host is never asked to pause (its restarts stay in
                // vst3-host) and starts processing at load, so the start and
                // stop calls are no-ops.
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
        }
    }
}

impl Drop for LoadedPlugin {
    /// UI thread, with the slot out of every chain, so no block can be
    /// inside the plugin: stop it if it is processing (as its audio
    /// thread), deactivate it (main thread), then drop the processor and
    /// the handle.  The plugin is destroyed with the last handle (an open
    /// editor may hold one a little longer).
    fn drop(&mut self) {
        if let PluginProcessor::Clap(processor) = &mut self.processor {
            if self.handle.0.shared.pause.take_processing() {
                processor.stop();
            }
            self.handle.deactivate();
        }
    }
}

impl UiSide {
    fn new(
        instance: PluginInstance,
        from_audio: ParamReceiver,
        ids: Arc<ParamIds>,
        views: Vec<PluginParamView>,
        saved: Saved,
    ) -> Self {
        Self {
            instance,
            from_audio,
            ids,
            views,
            saved: saved.params,
            state: saved.state,
            state_stale: saved.stale,
            pause_wait: None,
        }
    }
}

impl PluginUi {
    fn latency(&self) -> u32 {
        match &self.vst3 {
            Some(service) => service.latency(),
            None => self.shared.latency(),
        }
    }
}

/// Writes plugin-reported `(index, normalized)` values into the views.
/// Returns whether any view moved.
fn apply_reported(
    clap: &ClapInstance,
    ids: &ParamIds,
    views: &mut [PluginParamView],
    reported: &[(usize, f64)],
) -> bool {
    let mut changed = false;
    for &(index, value) in reported {
        let Some(id) = ids.id(index) else {
            continue;
        };
        if let Some(view) = views.iter_mut().find(|view| view.id == id) {
            view.value = value;
            view.display = clap.display_for(id, value);
            changed = true;
        }
    }
    changed
}

/// Drains what the audio side reported into the views.
fn drain_reported(
    clap: &ClapInstance,
    from_audio: &mut ParamReceiver,
    ids: &ParamIds,
    views: &mut [PluginParamView],
) -> bool {
    let mut reported = Vec::new();
    from_audio.drain(|index, value, _| reported.push((index, value)));
    apply_reported(clap, ids, views, &reported)
}

/// Resumes after [`PauseStep::Ready`], which never refuses; a refusal is
/// logged once instead of leaving the plugin silently paused unnoticed.
fn resume_or_log(pause: &super::pause::PauseHandshake) {
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if !pause.resume() && !LOGGED.swap(true, Ordering::Relaxed) {
        eprintln!("[plugins] a CLAP plugin's pause would not resume; it stays paused");
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
        self.0.shared.ports_changed()
    }

    /// The first processing failure the audio thread recorded (atomics).
    pub(crate) fn failure(&self) -> Option<ProcessFailure> {
        self.0.shared.failure()
    }

    /// What both halves share, for tests that look at the handshake.
    #[cfg(test)]
    pub(crate) fn shared(&self) -> &PluginShared {
        &self.0.shared
    }

    /// The control views as last read or set.
    pub(crate) fn parameters(&self) -> Vec<PluginParamView> {
        self.ui().views.clone()
    }

    /// Reads every control again, so the panel shows the plugin's values
    /// right now.  A CLAP plugin with values still queued for it keeps the
    /// views, which already show them.
    pub(crate) fn refresh_parameters(&self) {
        let mut ui = self.ui();
        let views = match &ui.instance {
            PluginInstance::Vst3Legacy(plugin) => host_parameters(plugin),
            PluginInstance::Clap(_) if self.0.out.has_queued() => return,
            PluginInstance::Clap(clap) => clap.parameters(),
        };
        ui.views = views;
    }

    /// Sends normalized `(index, value)` CLAP values to the plugin.  While
    /// it is processing they go through the transfer into the next block's
    /// input events.  While it is not (no block yet, or paused) and nothing
    /// older is queued, the UI pauses it and hands them to `params.flush`
    /// itself, so a value set with no audio running reaches the plugin and
    /// its state at once.  Never waits for the audio thread: a pause that
    /// is not ready at once leaves the values queued and is finished by
    /// [`Self::service`].
    fn send_clap(&self, ui: &mut UiSide, values: &[(usize, f64)]) {
        let shared = &*self.0.shared;
        let UiSide {
            instance,
            from_audio,
            ids,
            views,
            pause_wait,
            ..
        } = ui;
        let PluginInstance::Clap(clap) = instance else {
            return;
        };
        let native = |clap: &ClapInstance| -> Vec<(u32, f64)> {
            values
                .iter()
                .filter_map(|&(index, value)| clap.native(index, value))
                .collect()
        };
        if !clap.is_active() {
            // A failed reactivation: the plugin stays paused and never
            // starts again, so `flush` is a plain main-thread call.
            if clap.can_flush() {
                let reported = clap.flush_values(&native(clap));
                apply_reported(clap, ids, views, &reported);
            }
            return;
        }
        // Only from a pause, or before a start no block has begun: a plugin
        // between the audio side's start and `started` is left to it.
        let direct = shared.pause.is_idle()
            && pause_wait.is_none()
            && !shared.restart_pending()
            && clap.can_flush()
            && !self.0.out.has_queued();
        if direct
            && drive_pause(&shared.pause, pause_wait, || clap.stop_processing()) == PauseStep::Ready
        {
            // What the last blocks reported is older than this flush.
            drain_reported(clap, from_audio, ids, views);
            let reported = clap.flush_values(&native(clap));
            apply_reported(clap, ids, views, &reported);
            // A failed insert stays paused: no new start per knob move.
            if shared.failure().is_none() {
                resume_or_log(&shared.pause);
            }
            return;
        }
        for &(index, value) in values {
            self.0.out.push(index, value);
        }
    }

    /// Sets one normalized control and keeps the views and the saved value
    /// in step.  Returns the plugin's own text for the new value.
    pub(crate) fn set_parameter(&self, id: u32, value: f64) -> Result<Option<String>, String> {
        let value = value.clamp(0.0, 1.0);
        let mut ui = self.ui();
        let display = match &ui.instance {
            PluginInstance::Vst3Legacy(plugin) => {
                host_set_parameter(plugin, id, value)?;
                // Only the moved control is refreshed: re-reading every
                // parameter on each drag event holds the plugin far longer.
                host_format_parameter(plugin, id, value)
            }
            PluginInstance::Clap(clap) => {
                let index = ui
                    .ids
                    .index_of(id)
                    .ok_or_else(|| "That control is no longer available.".to_owned())?;
                let display = Some(clap.display_for(id, value));
                if !clap.is_read_only(id) {
                    self.send_clap(&mut ui, &[(index, value)]);
                }
                display
            }
        };
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
        let views = match &ui.instance {
            PluginInstance::Vst3Legacy(plugin) => host_reset_parameters(plugin)?,
            PluginInstance::Clap(clap) => {
                let defaults = clap.default_values();
                let views = clap.default_views();
                self.send_clap(&mut ui, &defaults);
                views
            }
        };
        ui.views = views;
        ui.saved.clear();
        ui.state_stale = true;
        let views = ui.views.clone();
        drop(ui);
        self.nudge();
        Ok(views)
    }

    /// Reads every control and stores the result as the views and the
    /// saved values.  The VST3 change reporter is drained first, without
    /// formatting: a value queued before this fresher read would otherwise
    /// be applied by the next change poll, putting an older value back.
    /// `None` while a CLAP value still waits for the audio side (reading
    /// the plugin then would show and save the old value); nothing is
    /// stored then.
    pub(crate) fn sync_full(&self) -> Option<Vec<PluginParamView>> {
        let mut ui = self.ui();
        let parameters = match &ui.instance {
            PluginInstance::Vst3Legacy(plugin) => {
                drop(take_parameter_values(&ui.instance));
                host_parameters(plugin)
            }
            PluginInstance::Clap(_) if self.0.out.has_queued() => return None,
            PluginInstance::Clap(clap) => clap.parameters(),
        };
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
        self.0.format == PluginFormat::Vst3
    }

    pub(crate) fn is_clap(&self) -> bool {
        self.0.format == PluginFormat::Clap
    }

    /// Tells a VST3 plugin's service that it was just touched, so it looks
    /// for restart requests for a while (see [`Vst3Service::nudge`]).
    pub(crate) fn nudge(&self) {
        if let Some(service) = &self.0.vst3 {
            service.nudge();
        }
    }

    /// Takes a VST3 plugin's "read every control" request (see
    /// [`Vst3Service::take_parameters_changed`]).
    pub(crate) fn take_parameters_changed(&self) -> bool {
        self.0
            .vst3
            .as_ref()
            .is_some_and(|service| service.take_parameters_changed())
    }

    /// The plugin's main-thread work, once per UI frame, under the UI lock
    /// only (never the chain lock, never a lock the audio thread takes).
    ///
    /// CLAP: callbacks, timers and fds; the output values the audio side
    /// reported, into the views; a parameter rescan; and a restart (or a
    /// flush the plugin asked for while not processing) through the pause
    /// handshake: the first frame asks the audio side to stop, a later one
    /// (or this one, when it is not processing or the stream stalled)
    /// deactivates and reactivates it, reads its latency and resumes it.
    /// VST3: vst3-host's restart requests.
    ///
    /// Returns whether the views need republishing: the latency or a
    /// control the plugin reported changed, or a restart failed.
    pub(crate) fn service(&self) -> bool {
        if let Some(service) = &self.0.vst3 {
            return service.service();
        }
        let shared = &*self.0.shared;
        let mut ui = self.ui();
        let UiSide {
            instance,
            from_audio,
            ids,
            views,
            pause_wait,
            ..
        } = &mut *ui;
        let PluginInstance::Clap(clap) = instance else {
            return false;
        };
        clap.service_callbacks();
        let mut changed = drain_reported(clap, from_audio, ids, views);
        if let Some(fresh) = clap.rescan_if_requested() {
            *views = fresh;
            changed = true;
        }
        if !clap.is_active() && clap.take_flush_request() {
            // Inactive (a failed reactivation): a main-thread flush.
            let reported = clap.flush_values(&[]);
            changed |= apply_reported(clap, ids, views, &reported);
        }
        let flush_wanted = clap.flush_requested() && !shared.pause.processing();
        if (shared.restart_pending() || pause_wait.is_some() || flush_wanted)
            && drive_pause(&shared.pause, pause_wait, || clap.stop_processing()) == PauseStep::Ready
        {
            if shared.take_restart() & RESTART_ACTIVATION != 0 && !clap.restart() {
                // Left paused: an inactive plugin must not start again.
                shared.fail(ProcessFailure::Clap(ClapFailure::NotActive));
                changed = true;
            }
            if clap.is_active() && clap.take_flush_request() {
                // Quiet: what the last blocks reported is older.
                changed |= drain_reported(clap, from_audio, ids, views);
                let reported = clap.flush_values(&[]);
                changed |= apply_reported(clap, ids, views, &reported);
            }
            // Never resume an inactive plugin, or restart a failed one.
            if clap.is_active() && shared.failure().is_none() {
                resume_or_log(&shared.pause);
            }
        }
        clap.reread_latency_if_changed();
        changed | shared.take_latency_change()
    }

    /// Whether the saved state blob needs a capture: stale, or the plugin
    /// marked its state dirty.
    pub(crate) fn wants_capture(&self) -> bool {
        self.ui().state_stale || self.0.shared.state_dirty()
    }

    /// Captures the plugin's opaque state into the saved blob, with the
    /// saved values as its snapshot.  A failed capture keeps the previous
    /// blob, whose snapshot still lets newer values win on restore, and is
    /// not retried until a control changes again; a deferred one (CLAP
    /// values still queued for the audio side, or off the main thread)
    /// stays stale so the next save retries.  CLAP saves under the UI lock
    /// only, so the audio callback keeps the insert; VST3 locks it.
    ///
    /// CLAP values queued while the plugin processes wait for a block: if
    /// the stream stops first, the capture stays deferred until the next
    /// block (or a skipped one's flush) delivers them.  It cannot capture
    /// anyway: the blob would hold the old values, and its snapshot must
    /// say what the blob holds, or a restore would skip the newer saved
    /// values.  The saved values still restore them.
    pub(crate) fn capture_state(&self) {
        let mut ui = self.ui();
        let bytes = match &mut ui.instance {
            PluginInstance::Clap(_) if self.0.out.has_queued() => Err(Capture::Deferred),
            PluginInstance::Clap(clap) => match clap.save_state() {
                StateSave::Saved(bytes) => Ok(bytes),
                StateSave::Unavailable => Err(Capture::Failed),
                StateSave::Deferred => Err(Capture::Deferred),
            },
            PluginInstance::Vst3Legacy(plugin) => plugin
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .save_state()
                .map_err(|_| Capture::Failed),
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

    /// Runs `body` on the CLAP instance (its editor, its main-thread
    /// callbacks), under the UI lock; `None` for another format.  UI
    /// thread only.  The audio thread never waits for this.
    pub(crate) fn with_clap<R>(&self, body: impl FnOnce(&mut ClapInstance) -> R) -> Option<R> {
        match &mut self.ui().instance {
            PluginInstance::Clap(clap) => Some(body(clap)),
            PluginInstance::Vst3Legacy(_) => None,
        }
    }

    /// Teardown (see [`LoadedPlugin`]'s drop): deactivates a CLAP plugin.
    fn deactivate(&self) {
        self.with_clap(ClapInstance::deactivate);
    }

    /// The vst3-host instance, for its editor window (`app::plugin_gui`).
    pub(crate) fn legacy_vst3(&self) -> Option<Arc<Mutex<Plugin>>> {
        match &self.ui().instance {
            PluginInstance::Vst3Legacy(plugin) => Some(Arc::clone(plugin)),
            PluginInstance::Clap(_) => None,
        }
    }
}
