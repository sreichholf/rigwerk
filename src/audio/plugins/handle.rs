//! The processor the audio thread runs, and the [`PluginHandle`] the UI
//! holds.  The UI lock is UI-only: the audio thread never takes it.
//! Lock order: chain model, then `UiSide`, then `ParamOut`.  Never the
//! reverse.
//!
//! Neither half waits for the other.  A restart, or a CLAP flush while
//! not processing, pauses from the UI and resumes.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use super::super::clap::{ClapInstance, ClapLoaded, ClapProcessor};
use super::super::lv2::{Lv2Loaded, Lv2Processor};
use super::super::vst3::{Vst3Controller, Vst3Editor, Vst3Loaded, Vst3Processor};
use super::host::{HostIo, PluginHostUi, Undelivered};
use super::link::{ParamIds, ParamOut, PluginShared, ProcessFailure};
use super::load::{merge_synced_params, Capture};
use super::pause::BlockAction;
use super::pref::{PluginFormat, PluginParamPref, PluginParamView, PluginState};
use super::slot::SlotControls;
use super::transfer::ParamReceiver;

/// `Send`; it lives in the chain's slot.  Boxed: each carries its
/// buffers' descriptions inline.
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
    out: Arc<ParamOut>,
    ui: Mutex<UiSide>,
    format: PluginFormat,
    /// Read once at load, so drawing never locks a plugin.
    has_editor: bool,
}

/// Behind [`PluginUi::ui`].
struct UiSide {
    /// The format's main-thread plugin object.
    host: Box<dyn PluginHostUi>,
    from_audio: ParamReceiver,
    ids: Arc<ParamIds>,
    views: Vec<PluginParamView>,
    /// The list [`PluginHandle::parameters`] last handed out.  While
    /// `views` still equals it, a republish clones the `Arc` instead of
    /// the controls.
    published: Option<Arc<[PluginParamView]>>,
    saved: Vec<PluginParamPref>,
    state: Option<PluginState>,
    /// Whether `state` may no longer describe the live plugin: it was
    /// missing or did not restore, or a control changed since.  The next
    /// preference save captures a fresh blob.
    state_stale: bool,
    /// Kept between frames of `drive_pause`.
    pause_wait: Option<(std::time::Instant, u64)>,
    /// Reused every frame, so a service does not allocate the edit list.
    edits: Vec<(u32, f64)>,
    /// A capture while any of these are outstanding builds the snapshot
    /// so a restore applies them again (see `capture_snapshot`).
    undelivered: Undelivered,
}

/// UI thread only.  The slot, the published link and an open editor each
/// hold one, so the plugin outlives the slot while an editor is open.
#[derive(Clone)]
pub(crate) struct PluginHandle(Arc<PluginUi>);

pub(crate) struct LoadedPlugin {
    processor: PluginProcessor,
    handle: PluginHandle,
}

/// What one effect did with one chunk, and what the buffers hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Fully wet plugin output.
    Processed,
    /// The effect's input, untouched.
    Skipped,
    /// The effect's input.  The failure is recorded on the shared state.
    Failed,
}

pub(crate) struct Saved {
    pub(crate) params: Vec<PluginParamPref>,
    pub(crate) state: Option<PluginState>,
    pub(crate) stale: bool,
}

impl LoadedPlugin {
    /// Processing starts with the first block.
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
        let mut ui = UiSide::new(Box::new(controller), from_audio, ids, views, saved);
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

    /// Processing starts with the first block.
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
        let mut ui = UiSide::new(Box::new(instance), from_audio, ids, views, saved);
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

    /// Processing starts with the first block.
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
        let mut ui = UiSide::new(Box::new(instance), from_audio, ids, views, saved);
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

    /// Atomics.  The audio thread may call it.
    pub(crate) fn latency(&self) -> u32 {
        self.handle.0.shared.latency()
    }

    #[cfg(test)]
    pub(crate) fn clap_processor_for_test(&mut self) -> &mut ClapProcessor {
        match &mut self.processor {
            PluginProcessor::Clap(processor) => processor,
            PluginProcessor::Vst3(_) | PluginProcessor::Lv2(_) => panic!("not a CLAP plugin"),
        }
    }

    #[cfg(test)]
    pub(crate) fn lv2_processor_for_test(&mut self) -> &mut Lv2Processor {
        match &mut self.processor {
            PluginProcessor::Lv2(processor) => processor,
            PluginProcessor::Vst3(_) | PluginProcessor::Clap(_) => panic!("not an LV2 plugin"),
        }
    }

    /// Fully wet.  The counters are not the ones a test reads.
    #[cfg(test)]
    pub(crate) fn run_for_test(&mut self, left: &mut [f32], right: &mut [f32]) -> Outcome {
        let controls = SlotControls::new(&super::pref::PluginPref::default());
        self.run(&controls, left, right, true)
    }

    /// Audio thread, one chunk of at most [`MAX_BLOCK`](super::MAX_BLOCK)
    /// frames.  Every loaded effect comes here every chunk, so its pause
    /// handshake is served even while it is skipped; a skip still flushes
    /// queued values.  A failure is recorded in [`PluginShared`] and never
    /// allocates.  No lock here that the UI could hold: a restart pauses
    /// the effect instead.
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
    /// The slot is out of every chain, so no block is inside: stop it if
    /// it is processing (as its audio thread), then drop the processor.
    /// Deactivation comes with the last handle, which an open editor may
    /// hold longer: the view is detached first, then the plugin.
    fn drop(&mut self) {
        let pause = &self.handle.0.shared.pause;
        // A stranded plugin is still inside `process`: a stop would be a
        // second call under it.  Production never reaches this (the slot is
        // leaked), but a direct drop in a test must not either.
        if !pause.stranded() && pause.take_processing() {
            self.processor.stop();
        }
    }
}

impl UiSide {
    fn new(
        host: Box<dyn PluginHostUi>,
        from_audio: ParamReceiver,
        ids: Arc<ParamIds>,
        views: Vec<PluginParamView>,
        saved: Saved,
    ) -> Self {
        Self {
            host,
            from_audio,
            ids,
            views,
            published: None,
            saved: saved.params,
            state: saved.state,
            state_stale: saved.stale,
            pause_wait: None,
            edits: Vec::new(),
            undelivered: Undelivered::default(),
        }
    }
}

impl PluginHandle {
    fn ui(&self) -> MutexGuard<'_, UiSide> {
        self.0.ui.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The address of the shared UI half, unique while any clone (an open
    /// editor, for one) still holds it.
    pub(crate) fn instance_id(&self) -> usize {
        Arc::as_ptr(&self.0) as *const () as usize
    }

    pub(crate) fn has_editor(&self) -> bool {
        self.0.has_editor
    }

    pub(crate) fn latency(&self) -> u32 {
        self.0.shared.latency()
    }

    pub(crate) fn failure(&self) -> Option<ProcessFailure> {
        self.0.shared.failure()
    }

    pub(in crate::audio) fn shared(&self) -> &PluginShared {
        &self.0.shared
    }

    #[cfg(test)]
    pub(crate) fn vst3_layout_for_test(&self) -> Option<super::super::vst3::BusLayout> {
        let mut ui = self.ui();
        Some(
            ui.host
                .as_any_mut()
                .downcast_mut::<Vst3Controller>()?
                .layout()
                .clone(),
        )
    }

    #[cfg(test)]
    pub(crate) fn lv2_parameters_for_test(&self) -> Vec<PluginParamView> {
        let mut ui = self.ui();
        ui.host
            .as_any_mut()
            .downcast_mut::<super::super::lv2::Lv2Instance>()
            .map(|lv2| lv2.parameters())
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub(crate) fn request_restart_for_test(&self, bits: u32) {
        self.0.shared.request_restart(bits);
    }

    /// As a VST3 editor's `performEdit`.
    #[cfg(test)]
    pub(crate) fn edit_for_test(&self, id: u32, value: f64) -> bool {
        self.0.out.push_edit(id, value)
    }

    /// The list from the last call, while the views still equal it, so a
    /// republish copies no controls.  A changed list is copied once and
    /// kept (see [`PluginView::parameters`]).
    pub(crate) fn parameters(&self) -> Arc<[PluginParamView]> {
        let mut ui = self.ui();
        if let Some(published) = &ui.published {
            if published.as_ref() == ui.views.as_slice() {
                return Arc::clone(published);
            }
        }
        let published: Arc<[PluginParamView]> = ui.views.clone().into();
        ui.published = Some(Arc::clone(&published));
        published
    }

    /// `None` from the host (a read would be stale, or the format has no
    /// read call) leaves the views as they are.
    pub(crate) fn refresh_parameters(&self) {
        let mut ui = self.ui();
        let views = {
            let UiSide {
                host,
                from_audio,
                ids,
                views,
                saved,
                edits,
                undelivered,
                pause_wait,
                state_stale,
                ..
            } = &mut *ui;
            let mut io = HostIo {
                shared: &self.0.shared,
                out: &self.0.out,
                from_audio,
                ids,
                views,
                saved,
                edits,
                undelivered,
                pause_wait,
                state_stale,
            };
            host.read_parameters(&mut io)
        };
        if let Some(views) = views {
            ui.views = views;
        }
    }

    pub(crate) fn set_parameter(&self, id: u32, value: f64) -> Result<Option<String>, String> {
        let value = value.clamp(0.0, 1.0);
        let mut ui = self.ui();
        let index = ui
            .ids
            .index_of(id)
            .ok_or_else(|| "That control is no longer available.".to_owned())?;
        let display = ui.host.parameter_text(id, value);
        // A read-only control still shows the value the plugin reports and
        // is saved like any other, but is never delivered to the plugin.
        if !ui.host.is_read_only(id) {
            let UiSide {
                host,
                from_audio,
                ids,
                views,
                saved,
                edits,
                undelivered,
                pause_wait,
                state_stale,
                ..
            } = &mut *ui;
            let mut io = HostIo {
                shared: &self.0.shared,
                out: &self.0.out,
                from_audio,
                ids,
                views,
                saved,
                edits,
                undelivered,
                pause_wait,
                state_stale,
            };
            host.write_parameter(&mut io, index, value);
        }
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

    /// Forgets the saved overrides.  The list stays here;
    /// [`Self::parameters`] shares it with the next refresh.
    pub(crate) fn reset_parameters(&self) -> Result<(), String> {
        let mut ui = self.ui();
        let views = {
            let UiSide {
                host,
                from_audio,
                ids,
                views,
                saved,
                edits,
                undelivered,
                pause_wait,
                state_stale,
                ..
            } = &mut *ui;
            let mut io = HostIo {
                shared: &self.0.shared,
                out: &self.0.out,
                from_audio,
                ids,
                views,
                saved,
                edits,
                undelivered,
                pause_wait,
                state_stale,
            };
            host.reset_parameters(&mut io)
        };
        ui.views = views;
        ui.saved.clear();
        ui.state_stale = true;
        Ok(())
    }

    /// VST3 reported values go to the controller first, and its editor
    /// edits are dropped: the controller already has them, and this read
    /// is fresher.  `None` (a read would be stale) stores nothing.
    pub(crate) fn sync_full(&self) -> Option<()> {
        let mut ui = self.ui();
        let parameters = {
            let UiSide {
                host,
                from_audio,
                ids,
                views,
                saved,
                edits,
                undelivered,
                pause_wait,
                state_stale,
                ..
            } = &mut *ui;
            let mut io = HostIo {
                shared: &self.0.shared,
                out: &self.0.out,
                from_audio,
                ids,
                views,
                saved,
                edits,
                undelivered,
                pause_wait,
                state_stale,
            };
            host.sync_parameters(&mut io)
        };
        let parameters = parameters?;
        ui.saved = merge_synced_params(&ui.saved, &parameters);
        ui.views = parameters;
        // The editor is open and can change more than parameters.
        ui.state_stale = true;
        Some(())
    }

    pub(crate) fn is_vst3(&self) -> bool {
        self.0.format == PluginFormat::Vst3
    }

    pub(crate) fn is_clap(&self) -> bool {
        self.0.format == PluginFormat::Clap
    }

    /// Once per UI frame, under the UI lock only: never the chain model's
    /// lock, never a lock the audio thread takes.  The host's `service`
    /// does the format work; this adds the latency change.
    pub(crate) fn service(&self) -> bool {
        let shared = &*self.0.shared;
        let mut ui = self.ui();
        let changed = {
            let UiSide {
                host,
                from_audio,
                ids,
                views,
                saved,
                edits,
                undelivered,
                pause_wait,
                state_stale,
                ..
            } = &mut *ui;
            let mut io = HostIo {
                shared: &self.0.shared,
                out: &self.0.out,
                from_audio,
                ids,
                views,
                saved,
                edits,
                undelivered,
                pause_wait,
                state_stale,
            };
            host.service(&mut io)
        };
        changed | shared.take_latency_change()
    }

    pub(crate) fn wants_capture(&self) -> bool {
        self.ui().state_stale || self.0.shared.state_dirty()
    }

    /// A failed capture keeps the previous blob and is not retried until a
    /// control changes.  A deferred one (values still queued, or a CLAP
    /// save off the main thread) stays stale so the next save retries.
    /// Under the UI lock only, so the audio callback keeps the effect.
    ///
    /// Captures even while values are queued: the processor is on the
    /// audio side, so the UI cannot deliver them, and a stopped stream
    /// would wait forever and lose state that is not a parameter.  The
    /// snapshot is built so restore applies those saved values (or
    /// defaults) on top, and the slot stays stale.
    ///
    /// The snapshot must say what the blob holds, or restore skips the
    /// newer saved values.  LV2's blob holds no controls
    /// (`state_holds_params`), so its snapshot is empty and every saved
    /// parameter is applied on top.
    pub(crate) fn capture_state(&self) {
        let mut ui = self.ui();
        let capture = {
            let UiSide {
                host,
                from_audio,
                ids,
                views,
                saved,
                edits,
                undelivered,
                pause_wait,
                state_stale,
                ..
            } = &mut *ui;
            let mut io = HostIo {
                shared: &self.0.shared,
                out: &self.0.out,
                from_audio,
                ids,
                views,
                saved,
                edits,
                undelivered,
                pause_wait,
                state_stale,
            };
            host.capture_state(&mut io)
        };
        // A blob that misses queued values is captured again once they
        // reached the processor.
        ui.state_stale = capture.leaves_stale() || !ui.undelivered.is_empty();
        if let Capture::Saved(captured) = capture {
            ui.state = Some(captured);
        }
    }

    pub(crate) fn saved(&self) -> (Vec<PluginParamPref>, Option<PluginState>) {
        let ui = self.ui();
        (ui.saved.clone(), ui.state.clone())
    }

    /// The editor closed, so the next preference save captures state.
    pub(crate) fn mark_state_stale(&self) {
        self.ui().state_stale = true;
    }

    /// Under the UI lock; `None` for another format.  UI thread only.
    /// The audio thread never waits for this.
    pub(crate) fn with_clap<R>(&self, body: impl FnOnce(&mut ClapInstance) -> R) -> Option<R> {
        let mut ui = self.ui();
        let clap = ui.host.as_any_mut().downcast_mut::<ClapInstance>()?;
        Some(body(clap))
    }

    /// The window keeps this handle longer than the view.  UI thread; the
    /// UI lock is taken only to create the view.
    pub(crate) fn open_vst3_editor(&self) -> Result<Vst3Editor, String> {
        let view = {
            let mut ui = self.ui();
            let controller = ui
                .host
                .as_any_mut()
                .downcast_mut::<Vst3Controller>()
                .ok_or_else(|| "This is not a VST3 plugin.".to_owned())?;
            controller
                .create_view()
                .ok_or_else(|| "The plugin has no editor.".to_owned())?
        };
        Vst3Editor::new(view)
    }
}

#[cfg(test)]
impl PluginHandle {
    /// Past the stranded deadline, so the test does not sleep 2 s.
    pub(crate) fn seed_pause_wait_for_test(&self) {
        use super::pause::PAUSE_STRANDED;
        let past = std::time::Instant::now()
            .checked_sub(PAUSE_STRANDED)
            .expect("uptime");
        self.ui().pause_wait = Some((past, self.0.shared.pause.served()));
    }

    /// No processor behind it, so nothing can run it.
    pub(crate) fn with_host_for_test(
        host: Box<dyn PluginHostUi>,
        shared: Arc<PluginShared>,
        views: Vec<PluginParamView>,
        saved: Saved,
        format: PluginFormat,
    ) -> Self {
        let ids = Arc::new(ParamIds::new(views.iter().map(|view| view.id).collect()));
        let (to_audio, from_ui) = super::transfer::param_transfer(ids.len());
        let (to_ui, from_audio) = super::transfer::param_transfer(ids.len());
        drop((from_ui, to_ui));
        let out = Arc::new(ParamOut::new(Arc::clone(&ids), to_audio));
        Self(Arc::new(PluginUi {
            shared,
            out,
            ui: Mutex::new(UiSide::new(host, from_audio, ids, views, saved)),
            format,
            has_editor: false,
        }))
    }
}
