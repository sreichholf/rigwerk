//! The UI half's one host contract ([`PluginHostUi`]) and the bookkeeping
//! the handle and every host share: the delivered-value ledger, the helpers
//! that write what the audio side reported into the views, and the state
//! snapshot rule.  The VST3 reported-value drain stays here too, the one
//! helper that names a host type.
//!
//! The handle's parameter, state and service operations never branch on the
//! format; each host module implements the trait once.  The callers hold the
//! handle's UI lock; nothing here locks, allocates a queue, or reaches the
//! audio thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use super::super::vst3::Vst3Controller;
use super::link::{ParamIds, ParamOut, PluginShared};
use super::load::{apply_parameter_changes, captured_state, Capture, ParamChange};
use super::pref::{PluginParamPref, PluginParamView};
use super::state::StateSave;
use super::transfer::ParamReceiver;

/// The UI half's per-format contract.  One implementation per host module;
/// the shared handle in `handle.rs` never branches on the format for these.
pub(crate) trait PluginHostUi: Send {
    /// The plugin's own text for a normalized value (None when the format has
    /// none).
    fn parameter_text(&self, id: u32, value: f64) -> Option<String>;

    /// Whether the plugin reports this control and never lets the host set it.
    fn is_read_only(&self, id: u32) -> bool;

    /// Delivers one normalized value the way this format allows: set + queue
    /// (VST3), a paused flush or a queued value (CLAP), a noted + queued value
    /// (LV2).  `index` is the position in [`ParamIds`].
    fn write_parameter(&mut self, io: &mut HostIo<'_>, index: usize, value: f64);

    /// Live control views, or `None` when a read now would show a stale value
    /// (a queued delivery) or the format has no read call.
    fn read_parameters(&mut self, io: &mut HostIo<'_>) -> Option<Vec<PluginParamView>>;

    /// A full read after the editor changed values: drain what the audio side
    /// reported and the editor's edits first, then read.  `None` as above.
    fn sync_parameters(&mut self, io: &mut HostIo<'_>) -> Option<Vec<PluginParamView>>;

    /// Returns every control to its default, delivered this format's way;
    /// returns the default views.
    fn reset_parameters(&mut self, io: &mut HostIo<'_>) -> Vec<PluginParamView>;

    /// Captures the state blob, applying the editor's pending edits first
    /// where the format has them.  `state_holds_params` decides the snapshot
    /// rule.  A format without a state interface (LV2) has no blob: its
    /// parameters alone describe it.
    fn capture_state(&mut self, io: &mut HostIo<'_>) -> Capture;

    /// The plugin's main-thread work for one UI frame.  Returns whether the
    /// views need republishing; the shared caller adds the latency change.
    ///
    /// VST3: the reported values and the editor's edits, then the restart
    /// flags.  `kLatencyChanged` pauses the plugin through the handshake,
    /// then deactivates and reactivates it and reads its latency, which VST3
    /// only lets change while inactive; `kIoChanged` with other buses,
    /// `kReloadComponent` and a changed parameter id set ask for a reload, as
    /// the processor's buffers and tables no longer fit.  CLAP: callbacks,
    /// timers and fds; a parameter rescan; and a restart (or a flush the
    /// plugin asked for while not processing) through the same handshake, the
    /// first frame asking the audio side to stop and a later one deactivating
    /// and reactivating it.  LV2: the control outputs the audio side
    /// reported, a changed latency report, and a wake for the worker thread;
    /// it has no main-thread callbacks, flush request or restart request.
    fn service(&mut self, io: &mut HostIo<'_>) -> bool;

    /// Whether the saved state blob embeds the control values (LV2's does
    /// not).
    fn state_holds_params(&self) -> bool;

    /// The host's own type, for the typed editor and CLAP accessors.
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;
}

/// The shared inputs one operation needs, borrowed from the handle's
/// `UiSide` and its `PluginUi`.  Its fields are disjoint from the host that
/// gets it, so a host may touch any of them.
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

/// The handle's capture outcome from a host's blob.  An empty or oversized
/// blob is a failed capture (see [`captured_state`]); the snapshot says what
/// the blob holds: the saved values where the format embeds them, nothing
/// where it does not (LV2).
pub(crate) fn finish_capture(io: &mut HostIo<'_>, save: StateSave, holds_params: bool) -> Capture {
    let bytes = match save {
        StateSave::Saved(bytes) => bytes,
        StateSave::Unavailable => return Capture::Failed,
        StateSave::Deferred => return Capture::Deferred,
    };
    // LV2's blob holds no control values: they are applied separately from
    // the saved parameters on restore, so the snapshot is empty and never
    // claims otherwise.
    let snapshot: &[PluginParamPref] = if holds_params {
        &capture_snapshot(io.saved, &io.undelivered.ids(), io.views)
    } else {
        &[]
    };
    captured_state(&bytes, snapshot)
}

/// Writes plugin-reported `(index, normalized)` values into the views.
/// Returns whether any view moved.  `display` formats a value the way the
/// loaded format does, so CLAP and LV2 share this one view update.
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

/// Takes what the audio side reported from the transfer.
pub(crate) fn drain_values(from_audio: &mut ParamReceiver) -> Vec<(usize, f64)> {
    let mut reported = Vec::new();
    from_audio.drain(|index, value, _| reported.push((index, value)));
    reported
}

/// Drains what the audio side reported into the views, formatting with
/// `display`.
pub(crate) fn drain_reported(
    from_audio: &mut ParamReceiver,
    ids: &ParamIds,
    views: &mut [PluginParamView],
    display: impl Fn(u32, f64) -> String,
) -> bool {
    let reported = drain_values(from_audio);
    apply_reported(ids, views, &reported, display)
}

/// Resumes after [`PauseStep::Ready`](super::pause::PauseStep::Ready), which
/// never refuses; a refusal is logged once instead of leaving the plugin
/// silently paused unnoticed.
pub(crate) fn resume_or_log(pause: &super::pause::PauseHandshake) {
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

/// Applies editor edits to the views and the saved values (an edit is the
/// user's setting), and notes their ids as not yet delivered to the
/// processor.  Returns whether there were any.
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
pub(crate) struct Undelivered {
    ids: Vec<(u32, u64)>,
}

impl Undelivered {
    /// Notes `ids` as queued up to push count `pushed`.
    pub(crate) fn note(&mut self, ids: impl IntoIterator<Item = u32>, pushed: u64) {
        for id in ids {
            match self.ids.iter_mut().find(|(queued, _)| *queued == id) {
                Some(entry) => entry.1 = entry.1.max(pushed),
                None => self.ids.push((id, pushed)),
            }
        }
    }

    /// Forgets the ids the processor has applied up to push count
    /// `applied`.
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
