//! The plugin chain of one path, as [`PathControls`] exposes it: chain
//! edits and their publish, plugin controls, servicing, state capture
//! and restore, and the audio side's `begin_chain` / `run_plugins`.
//!
//! # Chain handover
//!
//! The chain is split in two (see `plugins::chain` and `plugins::model`):
//! the runtime the audio thread owns and runs, and the model the UI edits.
//! They share no lock, so nothing the UI does makes the audio thread miss
//! the chain:
//! - mix and bypass are atomics per effect (`SlotControls`) that the audio
//!   thread reads every chunk;
//! - the path's chain bypass is one atomic the runtime loads every block:
//!   it skips every effect without writing the slots' own bypass, and the
//!   latency stays counted;
//! - a plugin control, a reset, an editor sync, a state capture and a
//!   preference save go to the plugin's handle through the published effect
//!   links (`slot_links`).  The handle keeps the views, the saved values and
//!   the state blob behind its own UI-only lock; the published view is
//!   patched at once;
//! - a structural edit (adding, removing, moving or restoring effects,
//!   reopening them for a new rate or because a plugin changed its layout,
//!   opening a never-opened slot) changes the model under its UI-only lock,
//!   builds and loads any new slot on the UI thread, and queues commands.
//!   The audio thread applies them at the start of its next block of the
//!   path (`begin_chain`) and hands the slots it let go of back; the next
//!   flush (every edit and every service frame) destroys them here.  With
//!   no bridge, or while the path is not live, the runtime is parked and
//!   the edit applies the commands itself.
//!
//! The mixer still keeps the timing when a block finds no runtime to run:
//! the moment the UI holds the parked runtime to apply an edit, a second
//! mixer of the same run while the first still holds it, or a chain
//! left unusable by a panicking edit.  It plays the chunk's chain input
//! delayed by [`PathControls::latency_samples`]
//! (`mixer::PathProcessor::run_effects`).  The path stays in time in the
//! sum, so nothing combs and the phase does not jump, but for that chunk it
//! plays dry.  The step into the fallback is smoothed, and the chain's own
//! output fades back in once it runs again, after one latency's worth of
//! frames.  [`PathControls::run_plugins`] reports whether the chain ran.

use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError};

use super::super::plugins::{
    ChainLease, ChainModel, PluginHandle, PluginPref, PluginView, ProcessFailure, ReloadPlan, Slot,
    SlotLink, MAX_EFFECTS,
};
use super::{LockStats, PathControls, PluginLockReport};

/// Armed by `edit_chain` around the edit itself.  If the edit panics, the
/// model's mutex is poisoned and no later edit publishes again, so the
/// published views and links are cleared here instead of being left to
/// describe (and hold references to) slots the edit may have changed, and
/// the audio side stops running the chain.
struct PublishOnUnwind<'a> {
    path: &'a PathControls,
}

impl Drop for PublishOnUnwind<'_> {
    fn drop(&mut self) {
        self.path.publish_poisoned();
    }
}

/// Builds a slot for `pref` at `rate` on this (the UI) thread.  A failed
/// load is returned as an error but is still a valid slot, so the failure
/// stays visible in the effect list.
fn load_slot(pref: PluginPref, rate: u32) -> (Slot, Result<(), String>) {
    let slot = Slot::loaded(pref, rate);
    let result = match &slot.error {
        Some(error) => Err(error.clone()),
        None => Ok(()),
    };
    (slot, result)
}

impl PathControls {
    /// Runs one structural edit on the model, sends its commands (applying
    /// them at once while the runtime is parked), and republishes what the
    /// UI and the audio flags read from it.  `None` when an earlier edit
    /// panicked.  The replaced links and the slots the runtime handed back
    /// are dropped after the model lock is released, on this thread.
    pub(in crate::audio) fn edit_chain<R>(
        &self,
        edit: impl FnOnce(&mut ChainModel) -> R,
    ) -> Option<R> {
        let (result, stale, returned) = {
            let mut chain = self.chain.lock().ok()?;
            // Declared after the model guard, so it runs first while
            // unwinding, still under the lock.
            let unwind = PublishOnUnwind { path: self };
            let result = edit(&mut chain);
            let returned = chain.flush();
            std::mem::forget(unwind);
            let stale = self.publish_plugins(&chain);
            (result, stale, returned)
        };
        drop(stale);
        drop(returned);
        // Models and cabinet kernels the audio side retired, from the same
        // UI-thread flush as the plugin slots just dropped.
        self.nam_park.flush();
        self.cab_park.flush();
        Some(result)
    }

    /// Sends what the model still queues and destroys the slots the
    /// runtime handed back since the last flush.  No republish: the links
    /// did not change.
    pub(crate) fn flush_chain(&self) {
        let returned = match self.chain.lock() {
            Ok(mut chain) => chain.flush(),
            Err(_) => return,
        };
        drop(returned);
        self.nam_park.flush();
        self.cab_park.flush();
    }

    /// The effect at `index` as last published.
    pub(in crate::audio) fn slot_link(&self, index: usize) -> Option<SlotLink> {
        self.slot_links
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(index)
            .cloned()
    }

    /// Changes one effect in the published views, without the model lock.
    fn patch_view(&self, index: usize, patch: impl FnOnce(&mut PluginView)) {
        let mut views = self
            .plugin_views
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut list = views.to_vec();
        if let Some(view) = list.get_mut(index) {
            patch(view);
            *views = list.into();
            self.plugin_views_version.fetch_add(1, Ordering::Release);
        }
    }

    /// The latency in samples this path's effect chain should add (see
    /// `ChainRuntime::latency_samples`).  Lock-free, for the audio thread.
    pub(crate) fn latency_samples(&self) -> u32 {
        self.latency.load(Ordering::Acquire)
    }

    /// Stores the whole path's latency for the block just begun: the chain's
    /// plus the delay of each oversampled NAM block.  Audio side, after the
    /// NAM stage has taken its commands.
    pub(crate) fn store_path_latency(&self, samples: u32) {
        self.path_latency.store(samples, Ordering::Relaxed);
    }

    /// The whole path's latency as the last block counted it; before a block
    /// has run, the chain's published latency stands in.  UI display only:
    /// the mixer aligns with the value it computed for its own block.
    pub(crate) fn path_latency_samples(&self) -> u32 {
        self.path_latency
            .load(Ordering::Relaxed)
            .max(self.latency_samples())
    }

    /// Stores a chain latency without a plugin (screenshots only, before a
    /// bridge runs; the next chain edit or latency change replaces it).
    pub(crate) fn screenshot_latency(&self, samples: u32) {
        self.latency.store(samples, Ordering::Release);
    }

    /// What a dead chain looks like to every reader: no effects, nothing to
    /// service, nothing to reopen, and nothing for the audio side to run.
    fn publish_poisoned(&self) {
        // Cleared before the next block, so a zero-latency chain never
        // counts an attempt.
        self.chain_poisoned.store(true, Ordering::Relaxed);
        self.plugin_active.store(0, Ordering::Relaxed);
        self.chain_unopened.store(false, Ordering::Relaxed);
        *self
            .plugin_views
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Arc::from(Vec::new());
        self.plugin_views_version.fetch_add(1, Ordering::Release);
        // The slots still own these plugins, so this is never the last
        // reference; what matters is that none outlives the chain.
        let links = std::mem::replace(
            &mut *self
                .slot_links
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
            Arc::from(Vec::new()),
        );
        drop(links);
    }

    /// Publishes the links, the views and the cheap flags for `chain`.
    /// Hands back the previous links, which the caller drops after
    /// releasing the model lock.
    #[must_use]
    fn publish_plugins(&self, chain: &ChainModel) -> Arc<[SlotLink]> {
        self.chain_rate.store(chain.rate(), Ordering::Relaxed);
        self.chain_unopened
            .store(chain.has_unopened(), Ordering::Relaxed);
        let links: Arc<[SlotLink]> = chain.links().into();
        let stale = std::mem::replace(
            &mut *self
                .slot_links
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
            Arc::clone(&links),
        );
        self.publish_from(&links);
        stale
    }

    /// Publishes the latency total, the active count and the views from
    /// `links`, without the model lock.  The links read the plugins' own
    /// atomics (latency, failures, reload requests), which the runtime
    /// reads too, so a running bridge stores the same total and count with
    /// its next block, once it applied the edit's commands.
    fn publish_from(&self, links: &[SlotLink]) {
        let total = links
            .iter()
            .map(SlotLink::latency)
            .fold(0u32, u32::saturating_add);
        self.latency.store(total, Ordering::Release);
        self.plugin_active.store(
            links.iter().filter(|link| link.active()).count(),
            Ordering::Relaxed,
        );
        let views: Arc<[PluginView]> = links.iter().map(SlotLink::view).collect();
        *self
            .plugin_views
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = views;
        self.plugin_views_version.fetch_add(1, Ordering::Release);
    }

    /// Republishes after a change the plugins made on their own (a latency
    /// re-read, a processing failure, a port change), from the links.
    ///
    /// It snapshots the links and publishes from that snapshot without the
    /// model lock.  That is only correct because every chain edit (which
    /// republishes the links) and `service_plugins` run on the UI thread,
    /// so no edit can publish newer links between the snapshot and this
    /// publish.
    fn refresh_from_links(&self) {
        let links = Arc::clone(
            &self
                .slot_links
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        if self.chain_poisoned.load(Ordering::Relaxed) {
            return;
        }
        self.publish_from(&links);
    }

    /// Replaces the saved effect list without loading anything.  Plugins are
    /// opened when the bridge starts (or when one is added in the UI).
    pub(crate) fn restore_plugins(&self, prefs: Vec<PluginPref>) {
        self.edit_chain(|chain| chain.restore(prefs));
        // The effect counters come from the fresh SlotControls; the chain's
        // own counters belong to the chain this replaces.
        self.chain_attempts.store(0, Ordering::Relaxed);
        self.chain_skips.store(0, Ordering::Relaxed);
    }

    /// Adds the pure-delay test effect (see `Slot::delay_for_test`).  Lets a
    /// test outside `audio` give a path one active effect.
    #[cfg(test)]
    pub(crate) fn insert_delay_for_test(&self, latency: u32) {
        self.edit_chain(|chain| chain.insert_delay_for_test(latency));
    }

    /// Panics inside `edit_chain`, so the unwind guard publishes the dead
    /// chain, as a panicking edit would.
    #[cfg(test)]
    pub(crate) fn poison_chain_for_test(&self) {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.edit_chain(|_chain| panic!("poison the chain in an edit"));
        }));
    }

    /// The rate the chain was last built for (lock-free).
    pub(crate) fn chain_rate(&self) -> u32 {
        self.chain_rate.load(Ordering::Relaxed)
    }

    /// Whether the chain needs a reload for `rate`: it was built at another
    /// rate, or a restored slot was never opened.  Answered from the
    /// published flags, without the model lock.
    pub(crate) fn needs_reload(&self, rate: u32) -> bool {
        rate != self.chain_rate.load(Ordering::Relaxed)
            || self.chain_unopened.load(Ordering::Relaxed)
    }

    /// Rebuilds the chain for `sample_rate` (`0` means 48 kHz, matching the
    /// model's own fallback).
    pub(crate) fn reload_plugins_for_rate(&self, sample_rate: u32) {
        let rate = if sample_rate == 0 {
            48_000
        } else {
            sample_rate
        };
        self.reload_with(rate, |chain| chain.reload_plan(rate));
    }

    /// Builds the slots `plan` lists, here on the UI thread, and installs
    /// them in place of the old ones, which the runtime hands back.  A slot
    /// rebuilt because its plugin asked is noted as such on its controls
    /// (which the rebuilt slot keeps), for the loop bound.
    fn reload_with(&self, rate: u32, plan: impl FnOnce(&mut ChainModel) -> Option<ReloadPlan>) {
        self.edit_chain(|chain| {
            let Some(plan) = plan(chain) else {
                return;
            };
            let requested: Vec<bool> = plan
                .jobs
                .iter()
                .map(|(index, _)| plan.requested.contains(index))
                .collect();
            let indices: Vec<usize> = plan.jobs.iter().map(|(index, _)| *index).collect();
            let built: Vec<(usize, Slot)> = plan
                .jobs
                .into_iter()
                .zip(&requested)
                .map(|((index, pref), &requested)| {
                    let (mut slot, _) = load_slot(pref, rate);
                    if requested && slot.plugin.is_none() {
                        // One attempt per request: a slot that failed to
                        // load wants nothing more, and says why.
                        slot.error = slot
                            .error
                            .take()
                            .map(|error| format!("The plugin asked to be reloaded: {error}"));
                    }
                    (index, slot)
                })
                .collect();
            chain.install(rate, built);
            // The rebuilt slots kept their controls.  A rate change is the
            // user's: the counts of rebuilds on request start again.
            for (index, requested) in indices.into_iter().zip(requested) {
                let controls = &chain.links()[index].controls;
                if plan.previous_rate != rate {
                    controls.reset_reloads();
                }
                if requested {
                    controls.note_rebuilt();
                }
            }
        });
    }

    /// Loads `pref` here, on the UI thread, and appends it.  A plugin that
    /// fails to load stays in the list with its error.
    pub(crate) fn add_plugin(&self, pref: PluginPref) -> Result<(), String> {
        self.edit_chain(|chain| {
            if chain.is_full() {
                return Err(format!("A path holds at most {MAX_EFFECTS} effects."));
            }
            let (slot, result) = load_slot(pref, chain.rate());
            match chain.push(slot) {
                Ok(()) => result,
                Err(_) => Err(format!("A path holds at most {MAX_EFFECTS} effects.")),
            }
        })
        .unwrap_or_else(|| Err("Plugin chain unavailable".to_owned()))
    }

    pub(crate) fn remove_plugin(&self, index: usize) {
        self.edit_chain(|chain| chain.remove(index));
    }

    pub(crate) fn move_plugin(&self, index: usize, delta: isize) {
        self.edit_chain(|chain| chain.move_slot(index, delta));
    }

    /// Bypasses one effect through its link: the audio thread reads the
    /// flag with its next chunk.
    pub(crate) fn set_plugin_bypass(&self, index: usize, bypass: bool) {
        let Some(link) = self.slot_link(index) else {
            return;
        };
        link.controls.set_bypass(bypass);
        if !bypass {
            // The audio thread recounts the active effects itself.
            self.wake_effects();
        }
        self.patch_view(index, |view| view.bypass = bypass);
    }

    /// Sets one effect's dry/wet mix through its link.
    pub(crate) fn set_plugin_mix(&self, index: usize, mix: f32) {
        let Some(link) = self.slot_link(index) else {
            return;
        };
        link.controls.set_mix(mix);
        let mix = link.controls.mix();
        self.patch_view(index, |view| view.mix = mix);
    }

    pub(crate) fn ensure_plugin_loaded(&self, index: usize) {
        self.edit_chain(|chain| chain.ensure_loaded(index));
    }

    /// The loaded plugin behind one effect, for its own editor window.
    /// From the published links, without the model lock.
    pub(crate) fn plugin_handle(&self, index: usize) -> Option<PluginHandle> {
        self.slot_link(index).and_then(|link| link.plugin)
    }

    /// Reads every control of a loaded effect again and stores them as its
    /// views and saved values (see `PluginHandle::sync_full`), after its
    /// editor changed them.  The service frames already follow edits and
    /// reported values as they happen; this is the last full read on save,
    /// close or bridge start.  Without the chain lock.  A missing or
    /// unloaded effect is a silent no-op.
    pub(crate) fn sync_plugin_parameters(&self, index: usize) {
        let Some(handle) = self.plugin_handle(index) else {
            return;
        };
        if let Some(parameters) = handle.sync_full() {
            self.patch_view(index, |view| view.parameters = parameters);
        }
    }

    /// Sets one plugin control.  A loaded plugin is reached through the
    /// published links, without the model lock: its handle stores the value
    /// and the published view is patched at once.  An effect that
    /// is not open yet goes through the chain, which opens it.
    pub(crate) fn set_plugin_parameter(
        &self,
        index: usize,
        id: u32,
        value: f64,
    ) -> Result<(), String> {
        let Some(handle) = self.plugin_handle(index) else {
            return self
                .edit_chain(|chain| chain.set_parameter(index, id, value))
                .unwrap_or_else(|| Err("Plugin chain unavailable".to_owned()));
        };
        let value = value.clamp(0.0, 1.0);
        let display = handle.set_parameter(id, value)?;
        self.wake_effects();
        self.patch_view(index, |view| {
            if let Some(parameter) = view
                .parameters
                .iter_mut()
                .find(|parameter| parameter.id == id)
            {
                parameter.value = value;
                if let Some(display) = &display {
                    parameter.display = display.clone();
                }
            }
        });
        Ok(())
    }

    /// Returns every control to the plugin's defaults.  A loaded plugin is
    /// reset through its link, without the model lock; one that is not open
    /// yet goes through the chain, which opens it.
    pub(crate) fn reset_plugin_parameters(&self, index: usize) -> Result<(), String> {
        let Some(handle) = self.plugin_handle(index) else {
            return self
                .edit_chain(|chain| chain.reset_parameters(index))
                .unwrap_or_else(|| Err("Plugin chain unavailable".to_owned()));
        };
        let parameters = handle.reset_parameters()?;
        self.wake_effects();
        self.patch_view(index, |view| view.parameters = parameters);
        Ok(())
    }

    /// Makes the audio thread enter the chain with its next block, even if
    /// every effect is bypassed at latency 0, so a skipped effect flushes
    /// the values just queued for it.  It recounts the active effects
    /// itself.
    fn wake_effects(&self) {
        self.plugin_wake.store(true, Ordering::Release);
    }

    /// The effect views as last published.  Never takes the model lock, so
    /// drawing never waits for an edit in progress.
    pub(crate) fn plugin_views(&self) -> Arc<[PluginView]> {
        Arc::clone(
            &self
                .plugin_views
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }

    /// Bumped on every publish of [`Self::plugin_views`].
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn plugin_views_version(&self) -> u64 {
        self.plugin_views_version.load(Ordering::Acquire)
    }

    /// Runs this path's plugins' main-thread work (CLAP callbacks and
    /// timers, VST3 edits and restart requests) once per UI frame, from the
    /// published links: a plugin's timer can do arbitrary work.  A latency
    /// change, a processing failure or a port change republishes from the
    /// links too.  It also flushes the chain: commands that did not fit the
    /// queue go out, and the slots the runtime handed back are destroyed
    /// here.
    #[cfg(test)]
    pub(crate) fn service_plugins(&self) {
        self.service_plugins_at(None);
    }

    /// [`Self::service_plugins`], knowing the rate the device opened at:
    /// a reload request in a frame that also has to follow a new rate
    /// rebuilds the chain at that rate once, instead of twice.
    pub(crate) fn service_plugins_at(&self, device_rate: Option<u32>) {
        let links = Arc::clone(
            &self
                .slot_links
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        let mut latency_changed = false;
        for handle in links.iter().filter_map(|link| link.plugin.as_ref()) {
            latency_changed |= handle.service();
        }
        if links.iter().any(SlotLink::reload_wanted) && self.reload_requested(&links, device_rate) {
            // The reload republished the links and views, and flushed.
            return;
        }
        self.flush_chain();
        let dirty = self.plugin_views_dirty.swap(false, Ordering::AcqRel);
        if dirty || latency_changed || self.pause_moved(&links) {
            self.refresh_from_links();
        }
        // Dropped on this thread, in case a removal left it as the last
        // reference to a plugin.
        drop(links);
    }

    /// Whether an effect went into or out of a pause (a restart, a
    /// reactivation) since its view was published.
    fn pause_moved(&self, links: &[SlotLink]) -> bool {
        let views = self.plugin_views();
        links
            .iter()
            .zip(views.iter())
            .any(|(link, view)| link.paused() != view.paused)
    }

    /// Rebuilds the effects whose plugin asked for it (a layout change):
    /// their state and values are captured from the old instance, a fresh
    /// one is loaded from them on this thread and swapped in, as a rate
    /// change does for every slot (`reload_plugins_for_rate`).  The old
    /// instance stopped processing when it asked, so its state is final; a
    /// capture that cannot happen yet keeps the last blob, which the saved
    /// values override on restore.  The runtime hands the old slot back and
    /// a later flush drops it on this thread.  Only the requesting slots
    /// are rebuilt, unless
    /// `device_rate` is a new rate, which rebuilds them all at once.
    ///
    /// A slot that was rebuilt on request `RELOAD_LIMIT` times within
    /// `RELOAD_WINDOW_MS` is failed instead when it asks again ("keeps
    /// asking"): a plugin that asks on every load (from its own setup,
    /// activation, first block or a timer) would otherwise reload
    /// forever.  Returns whether anything was rebuilt.
    fn reload_requested(&self, links: &[SlotLink], device_rate: Option<u32>) -> bool {
        let mut rebuild = false;
        for link in links.iter().filter(|link| link.reload_wanted()) {
            let Some(handle) = &link.plugin else {
                continue;
            };
            if !link.controls.may_reload() {
                eprintln!("[plugins] a plugin keeps asking to be reloaded; it stays stopped");
                handle.shared().fail(ProcessFailure::ReloadLoop);
                self.plugin_views_dirty.store(true, Ordering::Release);
            } else {
                handle.capture_state();
                rebuild = true;
            }
        }
        if !rebuild {
            return false;
        }
        let rate = self.chain_rate();
        match device_rate.filter(|&device| device != 0 && device != rate) {
            Some(device) => self.reload_plugins_for_rate(device),
            None => self.reload_with(rate, ChainModel::request_plan),
        }
        true
    }

    /// Captures the opaque state of every loaded plugin whose saved blob is
    /// missing, stale or marked dirty.  Runs on the UI thread (the CLAP main
    /// thread), through the published links: no model lock.
    pub(crate) fn capture_plugin_states(&self) {
        let links = Arc::clone(
            &self
                .slot_links
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        for handle in links.iter().filter_map(|link| link.plugin.as_ref()) {
            if handle.wants_capture() {
                handle.capture_state();
            }
        }
    }

    /// Marks one effect's saved state as out of date (its editor closed).
    pub(crate) fn mark_plugin_state_stale(&self, index: usize) {
        if let Some(handle) = self.plugin_handle(index) {
            handle.mark_state_stale();
        }
    }

    /// The effect chain as it should be saved, with fresh plugin state.
    /// Built from the published links, without the model lock.  Call it
    /// from the UI thread.
    pub(crate) fn plugin_prefs(&self) -> Vec<PluginPref> {
        self.capture_plugin_states();
        let links = Arc::clone(
            &self
                .slot_links
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        links.iter().map(SlotLink::saved_pref).collect()
    }

    pub(crate) fn has_plugins(&self) -> bool {
        self.plugin_active.load(Ordering::Relaxed) > 0 || self.plugin_wake.load(Ordering::Acquire)
    }

    /// Audio side, at the start of the path's block, before the mixer reads
    /// [`Self::latency_samples`].  Takes the runtime into `seat` if it holds
    /// none, applies the commands the UI queued, and stores the latency
    /// total and active count the block runs with.  Lock and allocation
    /// free.  A latency that moved since the last block (a plugin re-read
    /// it) asks the UI thread to rebuild the views.
    ///
    /// Returns whether a restored effect is still unopened, so the mixer
    /// keeps the path silent rather than playing it dry.  Without the
    /// runtime that is the model's published flag.
    pub(crate) fn begin_chain(&self, seat: &mut Option<ChainLease>) -> bool {
        // A mixer renders one run, so its lease is from this home.  Were
        // it not, it goes back to its own home; that run's `Shared` is
        // alive while its mixer runs, so this never frees the runtime here.
        debug_assert!(seat
            .as_ref()
            .is_none_or(|lease| lease.is_from(&self.chain_home)));
        if seat
            .as_ref()
            .is_some_and(|lease| !lease.is_from(&self.chain_home))
        {
            *seat = None;
        }
        if seat.is_none() {
            *seat = self.chain_home.take();
        }
        let Some(runtime) = seat.as_deref_mut() else {
            return self.chain_pending();
        };
        if self.chain_poisoned.load(Ordering::Relaxed) {
            return false;
        }
        runtime.apply_commands();
        if runtime.publish_latency() {
            // The views are rebuilt on the UI thread, not here.
            self.plugin_views_dirty.store(true, Ordering::Release);
        }
        self.latency.store(runtime.latency(), Ordering::Release);
        self.plugin_active
            .store(runtime.active(), Ordering::Relaxed);
        runtime.has_unopened()
    }

    /// Called from the output callback, after [`Self::begin_chain`] in the
    /// same block, with the same `seat`.  Returns whether the chain ran;
    /// `false` leaves the block untouched and the caller plays the chain's
    /// input delayed by [`Self::latency_samples`] instead (see
    /// `mixer::PathProcessor::run_effects` and the module docs).
    ///
    /// The flags keep the common no-plugin case cheap.  A chain with no
    /// active effect but some latency (every effect bypassed) still runs, so
    /// its skipped effects delay the block by their latency.  The path's
    /// chain bypass is an atomic the runtime loads; when it is set, every
    /// effect is skipped, the latency still counts, and each slot's own
    /// bypass is left alone.
    ///
    /// Every real attempt is counted; so is one that found no runtime in
    /// `seat`, which leaves the block to the dry fallback.  A dead chain (a
    /// panicking edit) is not counted as a skip: it is not contention, and
    /// it stays unused for the rest of the run (see
    /// [`PluginLockReport::chain_poisoned`]).  Measurement only: the
    /// counters are written relaxedly and Diagnostics reads them.
    pub(crate) fn run_plugins(
        &self,
        seat: &mut Option<ChainLease>,
        left: &mut [f32],
        right: &mut [f32],
        frames: usize,
    ) -> bool {
        if self.plugin_active.load(Ordering::Relaxed) == 0
            && self.latency_samples() == 0
            && !self.plugin_wake.swap(false, Ordering::AcqRel)
        {
            return true;
        }
        self.chain_attempts.fetch_add(1, Ordering::Relaxed);
        if self.chain_poisoned.load(Ordering::Relaxed) {
            return false;
        }
        let Some(runtime) = seat.as_deref_mut() else {
            self.chain_skips.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        let failed = runtime.process(left, right, frames);
        self.plugin_active
            .store(runtime.active(), Ordering::Relaxed);
        if failed {
            // The views are rebuilt on the UI thread, not here.
            self.plugin_views_dirty.store(true, Ordering::Release);
        }
        true
    }

    /// One block of the chain as a mixer would run it: takes the runtime,
    /// applies the queued commands, processes, and parks it again.
    #[cfg(test)]
    pub(crate) fn process_plugins(
        &self,
        left: &mut [f32],
        right: &mut [f32],
        frames: usize,
    ) -> bool {
        let mut seat = None;
        self.begin_chain(&mut seat);
        self.run_plugins(&mut seat, left, right, frames)
    }

    /// Takes the runtime as an audio thread would and keeps it until the
    /// lease drops: meanwhile no block (and no edit) can apply commands.
    #[cfg(test)]
    pub(crate) fn hold_chain_for_test(&self) -> ChainLease {
        self.chain_home.take().expect("the runtime is parked")
    }

    /// Whether edits are still queued for the runtime.
    #[cfg(test)]
    pub(crate) fn chain_commands_pending(&self) -> bool {
        self.chain
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pending()
    }

    /// This path's plugin counters: the chunks the chain missed and each
    /// effect's pauses.  Measurement only; it reads the published links.
    pub(crate) fn plugin_lock_report(&self) -> PluginLockReport {
        let links = Arc::clone(
            &self
                .slot_links
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        PluginLockReport {
            chain: LockStats {
                attempts: self.chain_attempts.load(Ordering::Relaxed),
                skips: self.chain_skips.load(Ordering::Relaxed),
            },
            effects: links
                .iter()
                .map(|link| link.controls.lock_stats())
                .collect(),
            chain_poisoned: self.chain_poisoned.load(Ordering::Relaxed),
        }
    }

    /// Clears this path's plugin counters (UI thread).  It does not
    /// clear [`PluginLockReport::chain_poisoned`]: resetting a counter cannot
    /// make a dead chain usable.
    pub(crate) fn reset_plugin_lock_counters(&self) {
        self.chain_attempts.store(0, Ordering::Relaxed);
        self.chain_skips.store(0, Ordering::Relaxed);
        let links = Arc::clone(
            &self
                .slot_links
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        for link in links.iter() {
            link.controls.reset_lock_stats();
        }
    }
}
