//! The bookkeeping the plugin handle and each host's UI half share: the
//! delivered-value ledger, the helpers that write what the audio side
//! reported into the views, and the state snapshot rule.  Format-neutral.
//!
//! The callers hold the handle's UI lock; nothing here locks, allocates a
//! queue, or reaches the audio thread.

use std::sync::atomic::{AtomicBool, Ordering};

use super::super::vst3::Vst3Controller;
use super::link::ParamIds;
use super::load::{apply_parameter_changes, ParamChange};
use super::pref::{PluginParamPref, PluginParamView};
use super::transfer::ParamReceiver;

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
