//! UI half's host contract.  Callers hold the handle's UI lock.  Nothing
//! here locks, allocates a queue, or reaches the audio thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use super::super::vst3::Vst3Controller;
use super::link::{ParamIds, ParamOut, PluginShared, ProcessFailure};
use super::load::{apply_parameter_changes, captured_state, Capture, ParamChange};
use super::pause::{drive_pause, PauseStep};
use super::pref::{PluginParamPref, PluginParamView};
use super::state::StateSave;
use super::transfer::ParamReceiver;

/// One implementation per host.  `handle.rs` never branches on the format
/// for these.
pub(crate) trait PluginHostUi: Send {
    /// The plugin's own text for a normalized value (None when the format has
    /// none).
    fn parameter_text(&self, id: u32, value: f64) -> Option<String>;

    fn is_read_only(&self, id: u32) -> bool;

    /// One normalized value, this format's way: set and queue (VST3), a
    /// paused flush or a queued value (CLAP), a noted and queued value
    /// (LV2).  `index` is the position in [`ParamIds`].
    fn write_parameter(&mut self, io: &mut HostIo<'_>, index: usize, value: f64);

    /// Live control views, or `None` when a read now would show a stale value
    /// (a queued delivery) or the format has no read call.
    fn read_parameters(&mut self, io: &mut HostIo<'_>) -> Option<Vec<PluginParamView>>;

    /// Full read after editor edits.  Drain reported values and editor
    /// edits first, or the read shows the old ones.  `None` as above.
    fn sync_parameters(&mut self, io: &mut HostIo<'_>) -> Option<Vec<PluginParamView>>;

    fn reset_parameters(&mut self, io: &mut HostIo<'_>) -> Vec<PluginParamView>;

    /// Apply pending editor edits first where the format has them.
    /// `state_holds_params` decides the snapshot.  LV2 has no state
    /// interface: its parameters alone describe it.
    fn capture_state(&mut self, io: &mut HostIo<'_>) -> Capture;

    /// One UI frame of main-thread work.  Returns whether the views need
    /// republishing; the caller adds a latency change.
    ///
    /// VST3 `kLatencyChanged` pauses, then deactivates and reactivates:
    /// latency may change only while inactive.  `kIoChanged` with other
    /// buses, `kReloadComponent`, or a changed parameter id set asks for a
    /// reload, because the processor's buffers no longer fit.  CLAP uses
    /// the same pause for a restart, or a flush asked for while not
    /// processing.  LV2 has no main-thread callbacks, flush, or restart.
    fn service(&mut self, io: &mut HostIo<'_>) -> bool;

    /// Whether the saved state blob embeds the control values (LV2's does
    /// not).
    fn state_holds_params(&self) -> bool;

    /// The host's own type, for the typed editor and CLAP accessors.
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;
}

/// Fields are disjoint from the host that gets this, so a host may touch
/// any of them.
pub(crate) struct HostIo<'a> {
    pub(crate) shared: &'a PluginShared,
    pub(crate) out: &'a ParamOut,
    pub(crate) from_audio: &'a mut ParamReceiver,
    pub(crate) ids: &'a Arc<ParamIds>,
    pub(crate) views: &'a mut Vec<PluginParamView>,
    pub(crate) saved: &'a mut Vec<PluginParamPref>,
    pub(crate) edits: &'a mut Vec<(u32, f64)>,
    pub(crate) undelivered: &'a mut Undelivered,
    pub(crate) pause_wait: &'a mut Option<(Instant, u64)>,
    pub(crate) state_stale: &'a mut bool,
}

/// The snapshot is the saved values where the format embeds them, and
/// empty where it does not (LV2).  An empty or oversized blob fails.
pub(crate) fn finish_capture(io: &mut HostIo<'_>, save: StateSave, holds_params: bool) -> Capture {
    let bytes = match save {
        StateSave::Saved(bytes) => bytes,
        StateSave::Unavailable => return Capture::Failed,
        StateSave::Deferred => return Capture::Deferred,
    };
    // LV2's blob holds no control values.  They are applied from the saved
    // parameters on restore, so the snapshot stays empty.
    let snapshot: &[PluginParamPref] = if holds_params {
        &capture_snapshot(io.saved, &io.undelivered.ids(), io.views)
    } else {
        &[]
    };
    captured_state(&bytes, snapshot)
}

pub(crate) fn apply_reported(
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

pub(crate) fn drain_values(from_audio: &mut ParamReceiver) -> Vec<(usize, f64)> {
    let mut reported = Vec::new();
    from_audio.drain(|index, value, _| reported.push((index, value)));
    reported
}

pub(crate) fn drain_reported(
    from_audio: &mut ParamReceiver,
    ids: &ParamIds,
    views: &mut [PluginParamView],
    display: impl Fn(u32, f64) -> String,
) -> bool {
    let reported = drain_values(from_audio);
    apply_reported(ids, views, &reported, display)
}

/// [`PauseStep::Ready`](super::pause::PauseStep::Ready) never refuses.  A
/// refusal is logged once, so a plugin is not left silently paused.
pub(crate) fn resume_or_log(pause: &super::pause::PauseHandshake) {
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if !pause.resume() && !LOGGED.swap(true, Ordering::Relaxed) {
        eprintln!("[plugins] a plugin's pause would not resume; it stays paused");
    }
}

/// A plugin that never returned from `process` is failed (an atomic
/// write; no processor call follows) so it does not stay paused with no
/// error.
pub(crate) fn drive_pause_or_fail(io: &mut HostIo<'_>, stop: impl FnOnce()) -> PauseStep {
    let step = drive_pause(&io.shared.pause, io.pause_wait, stop);
    if step == PauseStep::Stranded {
        io.shared.fail(ProcessFailure::PauseStranded);
    }
    step
}

/// View-only: saved values keep what the user set.  Returns the pairs it
/// set, so an echo through `performEdit` (a controller whose
/// `setParamNormalized` edits) is not taken for the user's.
pub(crate) fn drain_vst3_reported(
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

/// An edit is the user's setting.  Notes the ids as not yet delivered
/// to the processor.
pub(crate) fn apply_vst3_edits(
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

/// Ids in `undelivered` were still queued, so the blob may not hold them.
/// No marker says so.  An undelivered id with a saved value is left out,
/// so it counts as changed and is applied on top; one with no saved value
/// is listed at its default from `views`, so restore resets it.
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

/// Ids pushed whose values the processor may not have applied, each with
/// the push count once it was queued.  An id is done only once the
/// processor's applied count (a block, or an empty flush, the plugin
/// accepted) reaches that.  An empty queue is not enough: a drained value
/// can still be inside `process`, or in a block the plugin refused.
#[derive(Default)]
pub(crate) struct Undelivered {
    ids: Vec<(u32, u64)>,
}

impl Undelivered {
    pub(crate) fn note(&mut self, ids: impl IntoIterator<Item = u32>, pushed: u64) {
        for id in ids {
            match self.ids.iter_mut().find(|(queued, _)| *queued == id) {
                Some(entry) => entry.1 = entry.1.max(pushed),
                None => self.ids.push((id, pushed)),
            }
        }
    }

    pub(crate) fn settle(&mut self, applied: u64) {
        self.ids.retain(|&(_, pushed)| pushed > applied);
    }

    pub(crate) fn ids(&self) -> Vec<u32> {
        self.ids.iter().map(|&(id, _)| id).collect()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }
}
