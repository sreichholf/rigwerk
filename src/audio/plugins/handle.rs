//! A loaded plugin in its two halves: the [`PluginProcessor`] the audio
//! thread runs inside the chain's slot ([`LoadedPlugin`]), and the
//! [`PluginHandle`] the UI reaches through the published effect links and
//! the editor window.  Format-neutral.
//!
//! The UI half keeps the control views, the saved values, the state blob
//! and whether it is stale, next to the main-thread plugin object.  That
//! sits behind [`PluginUi`]'s own mutex, a UI-only lock: the audio thread
//! never takes it, so a control set, an editor sync, a state capture or a
//! preference save never touches a lock the audio thread takes.  Lock
//! order on the UI side: chain model → `UiSide` → `ParamOut`.  Never the
//! reverse.
//!
//! Every format is split along its thread rules: the processor holds
//! `ClapProcessor`, `Lv2Processor` or `Vst3Processor`, the UI half
//! `ClapInstance`, `Lv2Instance` or `Vst3Controller`, and they talk through
//! the parameter transfers, [`PluginShared`] and the pause handshake (a
//! restart, or a CLAP flush while not processing, pauses the plugin from the
//! UI and resumes it).  Neither half ever waits for the other.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use super::super::clap::{ClapInstance, ClapLoaded, ClapProcessor, StateSave};
use super::super::lv2::{Lv2Instance, Lv2Loaded, Lv2Processor};
use super::super::vst3::{
    Vst3Controller, Vst3Editor, Vst3Loaded, Vst3Processor, IO_CHANGED, LATENCY_CHANGED,
    PARAM_TITLES_CHANGED, PARAM_VALUES_CHANGED, RELOAD_COMPONENT,
};
use super::link::{
    ClapFailure, ParamIds, ParamOut, PluginShared, ProcessFailure, RESTART_ACTIVATION,
};
use super::load::{
    apply_parameter_changes, captured_state, merge_synced_params, Capture, ParamChange,
};
use super::pause::{drive_pause, BlockAction, PauseStep};
use super::pref::{PluginFormat, PluginParamPref, PluginParamView, PluginState};
use super::slot::SlotControls;
use super::transfer::ParamReceiver;

/// The VST3 flags [`PluginHandle::service`] acts on; others are ignored.
const VST3_HANDLED: u32 =
    LATENCY_CHANGED | PARAM_VALUES_CHANGED | PARAM_TITLES_CHANGED | IO_CHANGED | RELOAD_COMPONENT;

/// The UI half's plugin object.
pub(crate) enum PluginInstance {
    Vst3(Vst3Controller),
    Clap(ClapInstance),
    Lv2(Lv2Instance),
}

/// The audio half's plugin object (`Send`; it lives in the chain's slot).
/// Boxed: each carries its buffers' descriptions inline.
pub(crate) enum PluginProcessor {
    Vst3(Box<Vst3Processor>),
    Clap(Box<ClapProcessor>),
    Lv2(Box<Lv2Processor>),
}

impl PluginProcessor {
    fn start(&mut self) -> Result<(), ProcessFailure> {
        match self {
            PluginProcessor::Vst3(processor) => processor.start(),
            PluginProcessor::Clap(processor) => processor.start().map_err(ProcessFailure::Clap),
            PluginProcessor::Lv2(processor) => processor.start().map_err(ProcessFailure::Lv2),
        }
    }

    fn stop(&mut self) {
        match self {
            PluginProcessor::Vst3(processor) => processor.stop(),
            PluginProcessor::Clap(processor) => processor.stop(),
            PluginProcessor::Lv2(processor) => {
                // LV2 deactivates through the instance, on the UI thread.
                let _ = processor.stop();
            }
        }
    }

    fn flush_if_queued(&mut self) {
        match self {
            PluginProcessor::Vst3(processor) => processor.flush_if_queued(),
            PluginProcessor::Clap(processor) => processor.flush_if_queued(),
            PluginProcessor::Lv2(processor) => processor.flush_if_queued(),
        }
    }

    fn process(&mut self, left: &mut [f32], right: &mut [f32]) -> Result<(), ProcessFailure> {
        match self {
            PluginProcessor::Vst3(processor) => processor.process(left, right),
            PluginProcessor::Clap(processor) => {
                processor.process(left, right).map_err(ProcessFailure::Clap)
            }
            PluginProcessor::Lv2(processor) => {
                processor.process(left, right).map_err(ProcessFailure::Lv2)
            }
        }
    }
}

pub(crate) struct PluginUi {
    shared: Arc<PluginShared>,
    /// The UI→audio parameter producer.
    out: Arc<ParamOut>,
    ui: Mutex<UiSide>,
    format: PluginFormat,
    /// Read once at load, so drawing never locks a plugin.
    has_editor: bool,
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
    /// A VST3 `kLatencyChanged` waiting for that pause.
    reactivate: bool,
    /// Editor edits taken from [`ParamOut`], reused every frame.
    edits: Vec<(u32, f64)>,
    /// Ids pushed to the processor whose values it has not applied yet
    /// (see [`Undelivered`]).  A capture while there are any builds the
    /// blob's snapshot so a restore applies them again (see
    /// `capture_snapshot`).
    undelivered: Undelivered,
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

/// What one effect did with one chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The plugin's output (fully wet) is in the buffers.
    Processed,
    /// The buffers still hold the effect's input.
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
    /// A loaded, activated VST3 plugin; `shared` is the one it was loaded
    /// with.  Processing starts with the first block.
    pub(crate) fn vst3(loaded: Vst3Loaded, shared: Arc<PluginShared>, saved: Saved) -> Self {
        let Vst3Loaded {
            controller,
            processor,
            out,
            from_audio,
            ids,
            views,
            has_editor,
            state_current: _,
            undelivered,
        } = loaded;
        let mut ui = UiSide::new(
            PluginInstance::Vst3(controller),
            from_audio,
            ids,
            views,
            saved,
        );
        ui.undelivered.note(undelivered, out.pushed());
        Self {
            processor: PluginProcessor::Vst3(Box::new(processor)),
            handle: PluginHandle(Arc::new(PluginUi {
                shared,
                out,
                ui: Mutex::new(ui),
                format: PluginFormat::Vst3,
                has_editor,
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
            undelivered,
        } = loaded;
        let mut ui = UiSide::new(
            PluginInstance::Clap(instance),
            from_audio,
            ids,
            views,
            saved,
        );
        ui.undelivered.note(undelivered, out.pushed());
        Self {
            processor: PluginProcessor::Clap(Box::new(processor)),
            handle: PluginHandle(Arc::new(PluginUi {
                shared,
                out,
                ui: Mutex::new(ui),
                format: PluginFormat::Clap,
                has_editor,
            })),
        }
    }

    /// A loaded, activated LV2 plugin; `shared` is the one it was loaded
    /// with.  Processing starts with the first block.
    pub(crate) fn lv2(loaded: Lv2Loaded, shared: Arc<PluginShared>, saved: Saved) -> Self {
        let Lv2Loaded {
            instance,
            processor,
            out,
            from_audio,
            ids,
            views,
            has_editor,
            state_current: _,
            undelivered,
        } = loaded;
        let mut ui = UiSide::new(PluginInstance::Lv2(instance), from_audio, ids, views, saved);
        ui.undelivered.note(undelivered, out.pushed());
        Self {
            processor: PluginProcessor::Lv2(Box::new(processor)),
            handle: PluginHandle(Arc::new(PluginUi {
                shared,
                out,
                ui: Mutex::new(ui),
                format: PluginFormat::Lv2,
                has_editor,
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
        self.handle.0.shared.latency()
    }

    /// The CLAP processor, for tests that look at its buffers.
    #[cfg(test)]
    pub(crate) fn clap_processor_for_test(&mut self) -> &mut ClapProcessor {
        match &mut self.processor {
            PluginProcessor::Clap(processor) => processor,
            PluginProcessor::Vst3(_) | PluginProcessor::Lv2(_) => panic!("not a CLAP plugin"),
        }
    }

    /// The LV2 processor, for tests that look at its buffers.
    #[cfg(test)]
    pub(crate) fn lv2_processor_for_test(&mut self) -> &mut Lv2Processor {
        match &mut self.processor {
            PluginProcessor::Lv2(processor) => processor,
            PluginProcessor::Vst3(_) | PluginProcessor::Clap(_) => panic!("not an LV2 plugin"),
        }
    }

    /// One fully wet [`Self::run`], with counters nobody reads.
    #[cfg(test)]
    pub(crate) fn run_for_test(&mut self, left: &mut [f32], right: &mut [f32]) -> Outcome {
        let controls = SlotControls::new(&super::pref::PluginPref::default());
        self.run(&controls, left, right, true)
    }

    /// Audio thread, one chunk of at most [`MAX_BLOCK`](super::MAX_BLOCK)
    /// frames, in place.  Every loaded effect is brought here every chunk,
    /// so its pause handshake (start, stop, skip) is served even while it
    /// is skipped; only `wet` runs the plugin, and a skipped effect flushes
    /// the values queued for it.  A failure is recorded in [`PluginShared`]
    /// and never allocates.  Neither format has a lock here that the UI
    /// could hold: a restart pauses the effect instead.
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
        match block.action() {
            BlockAction::Process => {}
            BlockAction::Start if shared.failure().is_some() => {
                // A failed effect is not started again.
                shared.pause.start_failed();
                return Outcome::Skipped;
            }
            BlockAction::Start => {
                if let Err(failure) = processor.start() {
                    // Paused and not processing: later blocks skip instead
                    // of processing a plugin that never started, and the
                    // effect shows why.
                    shared.pause.start_failed();
                    shared.fail(failure);
                    return Outcome::Failed;
                }
                shared.pause.started();
            }
            BlockAction::Stop => {
                processor.stop();
                shared.pause.stopped();
                if wet {
                    controls.note_paused();
                }
                return Outcome::Skipped;
            }
            BlockAction::Skip => {
                if wet {
                    controls.note_paused();
                }
                return Outcome::Skipped;
            }
        }
        // An effect waiting for a reload has buffers or tables that may no
        // longer fit it (its ports or buses changed): it is not run, even
        // if the chain still counts it active for this block.
        let waiting = shared.reload_wanted();
        if !wet || waiting {
            // A failed effect's buffers may no longer fit it either.
            if shared.failure().is_none() && !waiting {
                processor.flush_if_queued();
            }
            return Outcome::Skipped;
        }
        match processor.process(&mut left[..count], &mut right[..count]) {
            Ok(()) => Outcome::Processed,
            Err(failure) => {
                shared.fail(failure);
                Outcome::Failed
            }
        }
    }
}

impl Drop for LoadedPlugin {
    /// UI thread, with the slot out of every chain, so no block can be
    /// inside the plugin: stop it if it is processing (as its audio
    /// thread), then drop the processor and the handle.  Deactivation and
    /// destruction come with the last handle (`Vst3Controller` and
    /// `ClapInstance` drop), which an open editor may hold a little longer:
    /// its view is detached first, then the plugin deactivated.
    fn drop(&mut self) {
        if self.handle.0.shared.pause.take_processing() {
            self.processor.stop();
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
            reactivate: false,
            edits: Vec::new(),
            undelivered: Undelivered::default(),
        }
    }
}

/// Writes plugin-reported `(index, normalized)` values into the views.
/// Returns whether any view moved.  `display` formats a value the way the
/// loaded format does, so CLAP and LV2 share this one view update.
fn apply_reported(
    ids: &ParamIds,
    views: &mut [PluginParamView],
    reported: &[(usize, f64)],
    display: impl Fn(u32, f64) -> String,
) -> bool {
    let mut changed = false;
    for &(index, value) in reported {
        let Some(id) = ids.id(index) else {
            continue;
        };
        if let Some(view) = views.iter_mut().find(|view| view.id == id) {
            view.value = value;
            view.display = display(id, value);
            changed = true;
        }
    }
    changed
}

/// Takes what the audio side reported from the transfer.
fn drain_values(from_audio: &mut ParamReceiver) -> Vec<(usize, f64)> {
    let mut reported = Vec::new();
    from_audio.drain(|index, value, _| reported.push((index, value)));
    reported
}

/// Drains what the audio side reported into the views, formatting with
/// `display`.
fn drain_reported(
    from_audio: &mut ParamReceiver,
    ids: &ParamIds,
    views: &mut [PluginParamView],
    display: impl Fn(u32, f64) -> String,
) -> bool {
    let reported = drain_values(from_audio);
    apply_reported(ids, views, &reported, display)
}

/// Resumes after [`PauseStep::Ready`], which never refuses; a refusal is
/// logged once instead of leaving the plugin silently paused unnoticed.
fn resume_or_log(pause: &super::pause::PauseHandshake) {
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if !pause.resume() && !LOGGED.swap(true, Ordering::Relaxed) {
        eprintln!("[plugins] a plugin's pause would not resume; it stays paused");
    }
}

/// Writes what a VST3 processor reported (its output parameters) into the
/// controller and the views.  View-only: the saved values keep what the
/// user set.  A value the controller already has is skipped.  Returns the
/// `(id, value)` pairs it set, so an echo of them through `performEdit`
/// (a controller whose `setParamNormalized` edits) is not taken for the
/// user's.
fn drain_vst3_reported(
    controller: &Vst3Controller,
    from_audio: &mut ParamReceiver,
    ids: &ParamIds,
    views: &mut [PluginParamView],
) -> Vec<(u32, f64)> {
    let mut reported = Vec::new();
    from_audio.drain(|index, value, _| reported.push((index, value)));
    let mut set = Vec::new();
    for (index, value) in reported {
        let Some(id) = ids.id(index) else {
            continue;
        };
        if controller.value(id) == Some(value) {
            continue;
        }
        controller.set(id, value);
        set.push((id, value));
        if let Some(view) = views.iter_mut().find(|view| view.id == id) {
            view.value = value;
            if let Some(display) = controller.format(id, value) {
                view.display = display;
            }
        }
    }
    set
}

/// Applies editor edits to the views and the saved values (an edit is the
/// user's setting), and notes their ids as not yet delivered to the
/// processor.  Returns whether there were any.
fn apply_vst3_edits(
    controller: &Vst3Controller,
    edits: &[(u32, f64)],
    views: &mut [PluginParamView],
    saved: &mut Vec<PluginParamPref>,
    undelivered: &mut Undelivered,
    pushed: u64,
) -> bool {
    if edits.is_empty() {
        return false;
    }
    let changes: Vec<ParamChange> = edits
        .iter()
        .map(|&(id, value)| ParamChange {
            id,
            value,
            display: controller.format(id, value),
        })
        .collect();
    apply_parameter_changes(views, saved, &changes);
    undelivered.note(edits.iter().map(|&(id, _)| id), pushed);
    true
}

/// A state blob's snapshot of the saved values.  Ids in `undelivered` were
/// still queued for the plugin when it saved, so the blob may not hold
/// them.  No marker says so: the snapshot is built so the ordinary restore
/// rule (`params_to_restore`, the same in older builds) applies them.  An
/// undelivered id with a saved value is left out, so the saved value
/// counts as changed and goes on top; one with no saved value (set back to
/// its default) is listed, with its default from `views`, so it is reset.
pub(super) fn capture_snapshot(
    saved: &[PluginParamPref],
    undelivered: &[u32],
    views: &[PluginParamView],
) -> Vec<PluginParamPref> {
    let mut snapshot: Vec<PluginParamPref> = saved
        .iter()
        .filter(|param| !undelivered.contains(&param.id))
        .cloned()
        .collect();
    snapshot.extend(
        undelivered
            .iter()
            .filter(|id| !saved.iter().any(|param| param.id == **id))
            .map(|&id| PluginParamPref {
                id,
                value: views
                    .iter()
                    .find(|view| view.id == id)
                    .map_or(0.0, |view| view.default),
            }),
    );
    snapshot
}

/// The ids pushed to the processor whose values it may not have applied
/// yet, each with the transfer's push count once it was queued.  The
/// processor publishes the push count up to which a block (or an empty
/// flush block) the plugin accepted delivered everything; an id is done
/// only once that count reaches its own.  An empty queue is not enough: a
/// drained value can still be inside `process`, or in a block the plugin
/// refused.
#[derive(Default)]
struct Undelivered {
    ids: Vec<(u32, u64)>,
}

impl Undelivered {
    /// Notes `ids` as queued up to push count `pushed`.
    fn note(&mut self, ids: impl IntoIterator<Item = u32>, pushed: u64) {
        for id in ids {
            match self.ids.iter_mut().find(|(queued, _)| *queued == id) {
                Some(entry) => entry.1 = entry.1.max(pushed),
                None => self.ids.push((id, pushed)),
            }
        }
    }

    /// Forgets the ids the processor has applied up to push count
    /// `applied`.
    fn settle(&mut self, applied: u64) {
        self.ids.retain(|&(_, pushed)| pushed > applied);
    }

    fn ids(&self) -> Vec<u32> {
        self.ids.iter().map(|&(id, _)| id).collect()
    }

    fn is_empty(&self) -> bool {
        self.ids.is_empty()
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
        self.0.shared.latency()
    }

    /// The first processing failure the audio thread recorded (atomics).
    pub(crate) fn failure(&self) -> Option<ProcessFailure> {
        self.0.shared.failure()
    }

    /// What both halves share (atomics only).
    pub(in crate::audio) fn shared(&self) -> &PluginShared {
        &self.0.shared
    }

    /// The buses a VST3 plugin's processor was built for, for the tests.
    #[cfg(test)]
    pub(crate) fn vst3_layout_for_test(&self) -> Option<super::super::vst3::BusLayout> {
        match &self.ui().instance {
            PluginInstance::Vst3(controller) => Some(controller.layout().clone()),
            PluginInstance::Clap(_) | PluginInstance::Lv2(_) => None,
        }
    }

    /// The LV2 instance's own control views, for the tests.
    #[cfg(test)]
    pub(crate) fn lv2_parameters_for_test(&self) -> Vec<PluginParamView> {
        match &self.ui().instance {
            PluginInstance::Lv2(lv2) => lv2.parameters(),
            PluginInstance::Vst3(_) | PluginInstance::Clap(_) => Vec::new(),
        }
    }

    /// A restart request as the plugin's own callback makes it, for the
    /// app's tests.
    #[cfg(test)]
    pub(crate) fn request_restart_for_test(&self, bits: u32) {
        self.0.shared.request_restart(bits);
    }

    /// An edit as a VST3 editor's `performEdit` makes it, for the tests.
    #[cfg(test)]
    pub(crate) fn edit_for_test(&self, id: u32, value: f64) -> bool {
        self.0.out.push_edit(id, value)
    }

    /// The control views as last read or set.
    pub(crate) fn parameters(&self) -> Vec<PluginParamView> {
        self.ui().views.clone()
    }

    /// Reads every control again, so the panel shows the plugin's values
    /// right now.  A CLAP plugin with values still queued for it keeps the
    /// views, which already show them.  An LV2 plugin has no call that reads
    /// a control port (the host's buffer is the value), so its views are
    /// always kept as they are.
    pub(crate) fn refresh_parameters(&self) {
        let mut ui = self.ui();
        let views = match &ui.instance {
            PluginInstance::Vst3(controller) => controller.views(),
            PluginInstance::Clap(_) if self.0.out.has_queued() => return,
            PluginInstance::Clap(clap) => clap.parameters(),
            PluginInstance::Lv2(_) => return,
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
                apply_reported(ids, views, &reported, |id, value| {
                    clap.display_for(id, value)
                });
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
            drain_reported(from_audio, ids, views, |id, value| {
                clap.display_for(id, value)
            });
            let reported = clap.flush_values(&native(clap));
            apply_reported(ids, views, &reported, |id, value| {
                clap.display_for(id, value)
            });
            // A failed effect stays paused: no new start per knob move.
            if shared.failure().is_none() {
                resume_or_log(&shared.pause);
            }
            return;
        }
        for &(index, value) in values {
            self.0.out.push(index, value);
        }
        let ids = &ui.ids;
        ui.undelivered.note(
            values.iter().filter_map(|&(index, _)| ids.id(index)),
            self.0.out.pushed(),
        );
    }

    /// Queues normalized `(index, value)` LV2 values for the processor.  LV2
    /// reads its control buffers during `run`, so nothing is called on the
    /// plugin from the UI, even while it is not processing: the values wait
    /// for the next block or a skipped effect's flush.  That is also why the
    /// UI cannot flush them itself when the stream is stopped, unlike CLAP.
    fn send_lv2(&self, ui: &mut UiSide, values: &[(usize, f64)]) {
        for &(index, value) in values {
            self.0.out.push(index, value);
        }
        let ids = &ui.ids;
        ui.undelivered.note(
            values.iter().filter_map(|&(index, _)| ids.id(index)),
            self.0.out.pushed(),
        );
    }

    /// Sets one normalized control and keeps the views and the saved value
    /// in step.  Returns the plugin's own text for the new value.
    pub(crate) fn set_parameter(&self, id: u32, value: f64) -> Result<Option<String>, String> {
        let value = value.clamp(0.0, 1.0);
        let mut ui = self.ui();
        let ui = &mut *ui;
        let display = match &mut ui.instance {
            PluginInstance::Vst3(controller) => {
                let index = ui
                    .ids
                    .index_of(id)
                    .ok_or_else(|| "That control is no longer available.".to_owned())?;
                // The controller has the value at once; the processor gets
                // it with its next block (or a skipped effect's flush).
                if !controller.is_read_only(id) {
                    controller.set(id, value);
                    self.0.out.push(index, value);
                    ui.undelivered.note([id], self.0.out.pushed());
                }
                controller.format(id, value)
            }
            PluginInstance::Clap(clap) => {
                let index = ui
                    .ids
                    .index_of(id)
                    .ok_or_else(|| "That control is no longer available.".to_owned())?;
                let display = Some(clap.display_for(id, value));
                if !clap.is_read_only(id) {
                    self.send_clap(ui, &[(index, value)]);
                }
                display
            }
            PluginInstance::Lv2(lv2) => {
                let index = ui
                    .ids
                    .index_of(id)
                    .ok_or_else(|| "That control is no longer available.".to_owned())?;
                let display = Some(lv2.display_for(id, value));
                if !lv2.is_read_only(id) {
                    lv2.note_value(index, value);
                    self.send_lv2(ui, &[(index, value)]);
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
        Ok(display)
    }

    /// Returns every control to the plugin's own defaults and forgets the
    /// saved overrides.
    pub(crate) fn reset_parameters(&self) -> Result<Vec<PluginParamView>, String> {
        let mut ui = self.ui();
        let ui = &mut *ui;
        let views = match &mut ui.instance {
            PluginInstance::Vst3(controller) => {
                for (index, value) in controller.default_values() {
                    if let Some(id) = ui.ids.id(index) {
                        controller.set(id, value);
                        self.0.out.push(index, value);
                        ui.undelivered.note([id], self.0.out.pushed());
                    }
                }
                controller.default_views()
            }
            PluginInstance::Clap(clap) => {
                let defaults = clap.default_values();
                let views = clap.default_views();
                self.send_clap(ui, &defaults);
                views
            }
            PluginInstance::Lv2(lv2) => {
                let defaults = lv2.default_values();
                let views = lv2.default_views();
                for &(index, value) in &defaults {
                    lv2.note_value(index, value);
                }
                self.send_lv2(ui, &defaults);
                views
            }
        };
        ui.views = views;
        ui.saved.clear();
        ui.state_stale = true;
        Ok(ui.views.clone())
    }

    /// Reads every control and stores the result as the views and the
    /// saved values.  A VST3 plugin's reported values go to its controller
    /// first, and its editor edits are dropped: the controller already has
    /// them, and this read is fresher.  `None` while a CLAP value still
    /// waits for the audio side (reading the plugin then would show and
    /// save the old value); nothing is stored then.
    pub(crate) fn sync_full(&self) -> Option<Vec<PluginParamView>> {
        let mut ui = self.ui();
        let UiSide {
            instance,
            from_audio,
            ids,
            views,
            edits,
            undelivered,
            ..
        } = &mut *ui;
        let parameters = match instance {
            PluginInstance::Vst3(controller) => {
                drain_vst3_reported(controller, from_audio, ids, views);
                self.0.out.take_edits(edits);
                undelivered.note(edits.iter().map(|&(id, _)| id), self.0.out.pushed());
                controller.views()
            }
            PluginInstance::Clap(_) if self.0.out.has_queued() => return None,
            PluginInstance::Clap(clap) => clap.parameters(),
            // LV2 offers no call that reads a control port: the views are
            // the values, and a queued one is not applied until the next
            // block (or flush), so reading now would show the old value.
            PluginInstance::Lv2(_) if self.0.out.has_queued() => return None,
            PluginInstance::Lv2(_) => views.clone(),
        };
        ui.saved = merge_synced_params(&ui.saved, &parameters);
        ui.views = parameters;
        // The editor is open and can change more than parameters.
        ui.state_stale = true;
        Some(ui.views.clone())
    }

    pub(crate) fn is_vst3(&self) -> bool {
        self.0.format == PluginFormat::Vst3
    }

    pub(crate) fn is_clap(&self) -> bool {
        self.0.format == PluginFormat::Clap
    }

    /// The plugin's main-thread work, once per UI frame, under the UI lock
    /// only (never the chain model's lock, never a lock the audio thread
    /// takes).
    ///
    /// CLAP: callbacks, timers and fds; the output values the audio side
    /// reported, into the views; a parameter rescan; and a restart (or a
    /// flush the plugin asked for while not processing) through the pause
    /// handshake: the first frame asks the audio side to stop, a later one
    /// (or this one, when it is not processing or the stream stalled)
    /// deactivates and reactivates it, reads its latency and resumes it.
    /// VST3: see [`Self::service_vst3`].  LV2: the control outputs the audio
    /// side reported and a changed latency report; it has no main-thread
    /// callbacks, flush request or restart request.
    ///
    /// Returns whether the views need republishing: the latency or a
    /// control the plugin reported changed, or a restart failed.
    pub(crate) fn service(&self) -> bool {
        let shared = &*self.0.shared;
        let mut ui = self.ui();
        if matches!(ui.instance, PluginInstance::Vst3(_)) {
            return self.service_vst3(&mut ui);
        }
        if matches!(ui.instance, PluginInstance::Lv2(_)) {
            return self.service_lv2(&mut ui);
        }
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
        let mut changed = drain_reported(from_audio, ids, views, |id, value| {
            clap.display_for(id, value)
        });
        if let Some(fresh) = clap.rescan_if_requested() {
            *views = fresh;
            changed = true;
        }
        if !clap.is_active() && clap.take_flush_request() {
            // Inactive (a failed reactivation): a main-thread flush.
            let reported = clap.flush_values(&[]);
            changed |= apply_reported(ids, views, &reported, |id, value| {
                clap.display_for(id, value)
            });
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
                changed |= drain_reported(from_audio, ids, views, |id, value| {
                    clap.display_for(id, value)
                });
                let reported = clap.flush_values(&[]);
                changed |= apply_reported(ids, views, &reported, |id, value| {
                    clap.display_for(id, value)
                });
            }
            // Never resume an inactive plugin, or restart a failed one.
            if clap.is_active() && shared.failure().is_none() {
                resume_or_log(&shared.pause);
            }
        }
        clap.reread_latency_if_changed();
        changed | shared.take_latency_change()
    }

    /// An LV2 plugin's frame: the control outputs the audio side reported go
    /// into the views and the instance's values, a latency change is
    /// republished, and queued values settle once the processor applied them.
    /// LV2 has no main-thread callbacks and no flush request.
    fn service_lv2(&self, ui: &mut UiSide) -> bool {
        let shared = &*self.0.shared;
        let UiSide {
            instance,
            from_audio,
            ids,
            views,
            undelivered,
            ..
        } = ui;
        let PluginInstance::Lv2(lv2) = instance else {
            return false;
        };
        let reported = drain_values(from_audio);
        for &(index, value) in &reported {
            lv2.note_value(index, value);
        }
        let mut changed = apply_reported(ids, views, &reported, |id, value| {
            lv2.display_for(id, value)
        });
        lv2.reread_latency_if_changed();
        undelivered.settle(self.0.out.applied());
        changed |= shared.take_latency_change();
        changed
    }

    /// A VST3 plugin's frame: the processor's output values into the
    /// controller and the views; the editor's edits into the views and the
    /// saved values (an edit is the user's setting, so it is saved at
    /// once); then the restart flags.  Values and titles are read again at
    /// once.  `kLatencyChanged` pauses the plugin through the handshake,
    /// then deactivates and reactivates it and reads its latency, which
    /// VST3 only lets change while inactive.  `kIoChanged` with other
    /// buses, `kReloadComponent` and a changed parameter id set ask for a
    /// reload, as the processor's buffers and tables no longer fit: the
    /// effect stops processing and the path rebuilds it.
    fn service_vst3(&self, ui: &mut UiSide) -> bool {
        let shared = &*self.0.shared;
        let UiSide {
            instance,
            from_audio,
            ids,
            views,
            saved,
            state_stale,
            pause_wait,
            reactivate,
            edits,
            undelivered,
            ..
        } = ui;
        let PluginInstance::Vst3(controller) = instance else {
            return false;
        };
        let reported = drain_vst3_reported(controller, from_audio, ids, views);
        let mut changed = !reported.is_empty();
        self.0.out.take_edits(edits);
        edits.retain(|edit| !reported.contains(edit));
        changed |= apply_vst3_edits(
            controller,
            edits,
            views,
            saved,
            undelivered,
            self.0.out.pushed(),
        );
        if !edits.is_empty() {
            *state_stale = true;
        }
        undelivered.settle(self.0.out.applied());
        let flags = shared.take_restart();
        if flags & PARAM_TITLES_CHANGED != 0 {
            if controller.rescan() {
                *views = controller.views();
            } else {
                eprintln!("[plugins] a VST3 plugin changed its parameter ids; reloading it");
                shared.want_reload();
            }
            changed = true;
        }
        if flags & PARAM_VALUES_CHANGED != 0 {
            *views = controller.views();
            *saved = merge_synced_params(saved, views);
            *state_stale = true;
            changed = true;
        }
        if flags & IO_CHANGED != 0 && controller.buses_changed() {
            eprintln!("[plugins] a VST3 plugin changed its audio buses; reloading it");
            shared.want_reload();
            changed = true;
        }
        if flags & RELOAD_COMPONENT != 0 {
            eprintln!("[plugins] a VST3 plugin asked to be reloaded");
            shared.want_reload();
            changed = true;
        }
        if flags & !VST3_HANDLED != 0 {
            static LOGGED: AtomicBool = AtomicBool::new(false);
            if !LOGGED.swap(true, Ordering::Relaxed) {
                eprintln!(
                    "[plugins] ignoring VST3 restart flags {:#x}",
                    flags & !VST3_HANDLED
                );
            }
        }
        *reactivate |= flags & LATENCY_CHANGED != 0;
        if (*reactivate || pause_wait.is_some())
            && drive_pause(&shared.pause, pause_wait, || controller.stop_processing())
                == PauseStep::Ready
        {
            // A flag `setActive` raises itself waits for the next frame.
            if std::mem::take(reactivate) && !controller.reactivate() {
                // Left paused: an inactive plugin must not process.
                shared.fail(ProcessFailure::Vst3Inactive);
                changed = true;
            }
            // Never resume an inactive plugin, or restart a failed one.
            if controller.is_active() && shared.failure().is_none() {
                resume_or_log(&shared.pause);
            }
        }
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
    /// not retried until a control changes again; a deferred one (values
    /// still queued for the audio side, or a CLAP save off the main thread)
    /// stays stale so the next save retries.  Both save under the UI lock
    /// only, so the audio callback keeps the effect.
    ///
    /// Every format captures anyway while values are queued: the processor
    /// is owned by the audio side (in the chain's slot), so the UI cannot
    /// deliver them itself, and with the stream stopped they would wait
    /// forever, losing state that is not a parameter (a file the editor
    /// loaded, say).  The blob then has the old values of those controls,
    /// so its snapshot is built for a restore to apply their saved values
    /// (or defaults) on top (see [`capture_snapshot`]), and the slot stays stale.
    ///
    /// The snapshot must say what the blob holds, or a restore would skip
    /// the newer saved values.  An LV2 blob holds no control values at all,
    /// so its snapshot is empty; the load applies every saved parameter on
    /// top of it.
    pub(crate) fn capture_state(&self) {
        let mut ui = self.ui();
        let UiSide {
            instance,
            views,
            saved,
            state,
            state_stale,
            edits,
            undelivered,
            ..
        } = &mut *ui;
        if let PluginInstance::Vst3(controller) = instance {
            // Edits since the last frame are saved values too.
            self.0.out.take_edits(edits);
            let pushed = self.0.out.pushed();
            apply_vst3_edits(controller, edits, views, saved, undelivered, pushed);
        }
        undelivered.settle(self.0.out.applied());
        let bytes = match instance {
            PluginInstance::Vst3(controller) => controller.save_state().map_err(|error| {
                eprintln!("[plugins] {error}");
                Capture::Failed
            }),
            PluginInstance::Clap(clap) => match clap.save_state() {
                StateSave::Saved(bytes) => Ok(bytes),
                StateSave::Unavailable => Err(Capture::Failed),
                StateSave::Deferred => Err(Capture::Deferred),
            },
            // A plugin without a state interface (or whose save fails) has no
            // blob; its parameters alone describe it.
            PluginInstance::Lv2(lv2) => match lv2.save_state() {
                StateSave::Saved(bytes) => Ok(bytes),
                StateSave::Unavailable => Err(Capture::Failed),
                StateSave::Deferred => Err(Capture::Deferred),
            },
        };
        let capture = match bytes {
            Ok(bytes) => {
                // LV2's blob holds no control values: they are applied
                // separately from the saved parameters on restore, so the
                // snapshot is empty and never claims otherwise.
                let snapshot: &[PluginParamPref] = if matches!(instance, PluginInstance::Lv2(_)) {
                    &[]
                } else {
                    &capture_snapshot(saved, &undelivered.ids(), views)
                };
                captured_state(&bytes, snapshot)
            }
            Err(capture) => capture,
        };
        // A blob that misses queued values is captured again once they
        // reached the processor.
        *state_stale = capture.leaves_stale() || !undelivered.is_empty();
        if let Capture::Saved(captured) = capture {
            *state = Some(captured);
        }
    }

    /// The saved values and state blob, for a preference save.
    pub(crate) fn saved(&self) -> (Vec<PluginParamPref>, Option<PluginState>) {
        let ui = self.ui();
        (ui.saved.clone(), ui.state.clone())
    }

    /// Marks the saved state as out of date (its editor closed), so the
    /// next preference save captures it.
    pub(crate) fn mark_state_stale(&self) {
        self.ui().state_stale = true;
    }

    /// Runs `body` on the CLAP instance (its editor, its main-thread
    /// callbacks), under the UI lock; `None` for another format.  UI
    /// thread only.  The audio thread never waits for this.
    pub(crate) fn with_clap<R>(&self, body: impl FnOnce(&mut ClapInstance) -> R) -> Option<R> {
        match &mut self.ui().instance {
            PluginInstance::Clap(clap) => Some(body(clap)),
            PluginInstance::Vst3(_) | PluginInstance::Lv2(_) => None,
        }
    }

    /// A new editor view of a VST3 plugin, for its window
    /// (`app::plugin_gui`).  The window keeps this handle for longer than
    /// the view.  UI thread; it takes the UI lock only to create the view.
    pub(crate) fn open_vst3_editor(&self) -> Result<Vst3Editor, String> {
        let view = match &self.ui().instance {
            PluginInstance::Vst3(controller) => controller.create_view(),
            PluginInstance::Clap(_) | PluginInstance::Lv2(_) => {
                return Err("This is not a VST3 plugin.".to_owned());
            }
        };
        Vst3Editor::new(view.ok_or_else(|| "The plugin has no editor.".to_owned())?)
    }
}
