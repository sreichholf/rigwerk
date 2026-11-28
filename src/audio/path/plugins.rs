//! The chain model and the runtime share no lock.  The audio thread
//! applies queued edits at the next block and never destroys a slot.
//!
//! A chunk with no runtime plays the chain input delayed by
//! [`PathControls::latency_samples`], so the sum keeps its phase.  The
//! step into that fallback is declicked; the chain fades back after one
//! latency of frames.

use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError};

use super::super::plugins::{
    is_main_thread, release_slot, ChainLease, ChainModel, PluginHandle, PluginPref, PluginView,
    ProcessFailure, ReloadPlan, Slot, SlotLink, MAX_EFFECTS,
};
use super::{LockStats, PathControls, PluginLockReport};

/// If the edit panics, the published views are cleared and the audio
/// side stops running the chain.
struct PublishOnUnwind<'a> {
    path: &'a PathControls,
}

impl Drop for PublishOnUnwind<'_> {
    fn drop(&mut self) {
        self.path.publish_poisoned();
    }
}

/// A failed load is still a slot, so the error stays in the list.
fn load_slot(pref: PluginPref, rate: u32) -> (Slot, Result<(), String>) {
    let slot = Slot::loaded(pref, rate);
    let result = match &slot.error {
        Some(error) => Err(error.clone()),
        None => Ok(()),
    };
    (slot, result)
}

impl PathControls {
    /// `None` when an earlier edit panicked.  Replaced links are released
    /// after the model lock, on this thread.
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
        self.release_slots(returned);
        // Models and cabinet kernels the audio side retired, from the same
        // UI-thread flush as the plugin slots just released.
        self.nam_park.flush();
        self.cab_park.flush();
        Some(result)
    }

    /// No republish: the links did not change.
    pub(crate) fn flush_chain(&self) {
        let returned = match self.chain.lock() {
            Ok(mut chain) => chain.flush(),
            Err(_) => return,
        };
        self.release_slots(returned);
        self.nam_park.flush();
        self.cab_park.flush();
    }

    /// Releases every slot a flush handed back through the one teardown
    /// path, after the model lock is gone.
    #[allow(clippy::vec_box)] // As `ChainModel::flush`: the boxes are the queued slots.
    fn release_slots(&self, returned: Vec<Box<Slot>>) {
        for slot in returned {
            release_slot(slot, &self.slot_park);
        }
    }

    pub(in crate::audio) fn slot_link(&self, index: usize) -> Option<SlotLink> {
        self.slot_links
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(index)
            .cloned()
    }

    /// Only the UI thread publishes, so the unlocked read and the store
    /// cannot race another publisher.
    fn patch_view(&self, index: usize, patch: impl FnOnce(&mut PluginView)) {
        debug_assert!(
            is_main_thread(),
            "effect views are only written from the UI thread"
        );
        let mut list = self.plugin_views().to_vec();
        let Some(view) = list.get_mut(index) else {
            return;
        };
        patch(view);
        *self
            .plugin_views
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = list.into();
        self.plugin_views_version.fetch_add(1, Ordering::Release);
    }

    /// The latency in samples this path's effect chain should add (see
    /// `ChainRuntime::latency_samples`).  Lock-free, for the audio thread.
    pub(crate) fn latency_samples(&self) -> u32 {
        self.latency.load(Ordering::Acquire)
    }

    /// Audio side, after NAM and pitch have taken their settings.
    pub(crate) fn store_path_latency(&self, samples: u32) {
        self.path_latency.store(samples, Ordering::Relaxed);
    }

    /// Before a block has run, the chain latency stands in.  Display
    /// only: the mixer uses the value it computed.
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

    /// Hands back the previous links.  The caller drops them after the
    /// model lock.
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

    /// Only the UI thread calls this, which is what makes
    /// [`Self::patch_view`] safe with no lock held across it.  No debug
    /// assert: chain tests publish off the process-wide main thread.
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

    /// Safe only because every edit and `service_plugins` run on the UI
    /// thread, so nothing publishes between the snapshot and this.
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

    /// From the published flags, without the model lock.
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

    /// A slot rebuilt because its plugin asked is noted, for the loop
    /// bound.
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

    /// The last full read, on save, close or bridge start.  No chain lock.
    pub(crate) fn sync_plugin_parameters(&self, index: usize) {
        let Some(handle) = self.plugin_handle(index) else {
            return;
        };
        // The full read leaves the list in the handle; taking it from there
        // keeps the next refresh sharing it (see `PluginHandle::parameters`).
        if handle.sync_full().is_some() {
            let parameters = handle.parameters();
            self.patch_view(index, |view| view.parameters = parameters);
        }
    }

    /// An effect not open yet goes through the chain, which opens it.
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
        handle.set_parameter(id, value)?;
        self.wake_effects();
        // Taking the handle's list shares it with the next refresh.
        let parameters = handle.parameters();
        self.patch_view(index, |view| view.parameters = parameters);
        Ok(())
    }

    /// One that is not open yet goes through the chain, which opens it.
    pub(crate) fn reset_plugin_parameters(&self, index: usize) -> Result<(), String> {
        let Some(handle) = self.plugin_handle(index) else {
            return self
                .edit_chain(|chain| chain.reset_parameters(index))
                .unwrap_or_else(|| Err("Plugin chain unavailable".to_owned()));
        };
        handle.reset_parameters()?;
        self.wake_effects();
        let parameters = handle.parameters();
        self.patch_view(index, |view| view.parameters = parameters);
        Ok(())
    }

    /// So a skipped effect flushes values just queued for it.
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

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn plugin_views_version(&self) -> u64 {
        self.plugin_views_version.load(Ordering::Acquire)
    }

    /// A plugin timer can do arbitrary work, so this is the UI thread.
    /// Slots the runtime handed back are destroyed here.
    #[cfg(test)]
    pub(crate) fn service_plugins(&self) {
        self.service_plugins_at(None);
    }

    /// A reload that also follows a new rate rebuilds the chain once.
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

    /// A slot rebuilt on request too many times in the window is failed
    /// instead, or a plugin that asks on every load would reload forever.
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

    /// UI thread, through the published links.  No model lock.
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

    /// From the published links.  Call from the UI thread.
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

    /// Lock and allocation free.  Returns whether a restored effect is
    /// still unopened, so the mixer keeps the path silent.
    pub(crate) fn begin_chain(&self, seat: &mut Option<ChainLease>) -> bool {
        // A lease from another home goes back.  That run's `Shared` is
        // alive while its mixer runs, so this does not free the runtime.
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

    /// `false` leaves the block untouched: the caller plays the chain
    /// input delayed by [`Self::latency_samples`].  A bypassed effect
    /// with latency still runs.  A poisoned chain is not counted as a
    /// skip.
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

    /// Does not clear a poisoned chain: a reset cannot make it usable.
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
