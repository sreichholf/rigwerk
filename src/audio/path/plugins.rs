//! The plugin chain of one path, as [`PathControls`] exposes it: chain
//! edits and their publish, plugin controls, servicing, state capture
//! and restore, and the audio thread's `process_plugins`.
//!
//! # Chain handover
//!
//! The audio thread only try-locks a chain; a missed lock costs that chunk
//! its inserts.  So the edits made all the time never take the chain lock:
//! - mix and bypass are atomics per insert (`SlotControls`) that the audio
//!   thread reads every chunk;
//! - the path's chain bypass is one atomic the audio thread loads in
//!   `PluginChain::process`: it skips every insert without writing the
//!   slots' own bypass, and the latency stays counted;
//! - a plugin control, a reset, an editor sync, a state capture and a
//!   preference save go to the plugin's handle through the published insert
//!   links (`slot_links`).  The handle keeps the views, the saved values and
//!   the state blob behind its own UI-only lock and holds the plugin's own
//!   lock only for the call (the audio thread then skips just that insert
//!   for a chunk, delayed by its latency, never the chain); the published
//!   view is patched at once;
//! - drawing reads the published views, a service frame services the
//!   handles, and a latency change or a failure rebuilds the views from the
//!   links, all without the chain lock.
//!
//! What still takes the chain lock is structural and short: adding,
//! removing, moving or restoring inserts, reopening them for a new rate
//! (slow loads and teardowns run outside the lock), opening a never-opened
//! slot (for its editor, or a control set on it), and the publish of the
//! views and links those edits do.  The running poll of a VST3 insert only
//! patches the published view.  A CLAP editor (no change feed) and an
//! insert that has not processed a block since the last sync are read in
//! full instead.
//!
//! For those rare misses the mixer keeps the timing: rather than a second,
//! lock-free copy of the chain (plugin instances are stateful and cannot be
//! duplicated) it plays the chunk's chain input delayed by
//! [`PathControls::latency_samples`] (`mixer::PathProcessor::run_inserts`).
//! The path stays in time in the sum, so nothing combs and the phase does
//! not jump, but its level and tone are not kept: for that chunk the path
//! plays dry.  The step into the fallback is smoothed (no crossfade is
//! possible), and the chain's own output fades back in once it runs again,
//! after one latency's worth of frames (the inserts missed that input).
//! [`PathControls::process_plugins`] reports whether the chain ran.

use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError, TryLockError};

use super::super::plugins::{
    PluginChain, PluginHandle, PluginParamView, PluginPref, PluginView, SlotLink,
};
use super::{LockStats, PathControls, PluginLockReport};

/// Armed by `edit_chain` around the edit itself.  If the edit panics, the
/// chain mutex is poisoned and no later edit publishes again, so the
/// published views and links are cleared here instead of being left to
/// describe (and hold references to) slots the edit may have changed.
struct PublishOnUnwind<'a> {
    path: &'a PathControls,
}

impl Drop for PublishOnUnwind<'_> {
    fn drop(&mut self) {
        self.path.publish_poisoned();
    }
}

/// Whether an editor sync has to read every control: it was asked to
/// (`requested`), or the insert has not processed a block since the last sync
/// (`!processed`, so a reported change may not be visible yet), or it is not a
/// VST3 insert at all (CLAP has no change feed).  Only a VST3 insert that
/// processed a block can be synced from its reported changes.
pub(super) fn needs_full_sync(requested: bool, processed: bool, has_vst3: bool) -> bool {
    requested || !processed || !has_vst3
}

/// What the running poll does with one id from `PluginHandle::take_changes`,
/// decided from the published view after that drain has released the
/// instance.
///
/// A known id only patches the published view.  The saved parameters and the
/// slot's stale flag stay as they are, whether the control is read-only or a
/// writable one the processor echoed.  An id the view does not list is
/// ignored.  A control the plugin actually added is picked up by the full
/// read on save, close, bypass, stop, or bridge start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RunningSync {
    /// Patch the published view's value and display.  Do not write the saved
    /// parameters and do not mark the slot stale.
    ViewOnly,
    /// The published view does not list this id.
    Ignore,
}

/// Classifies one reported id against the controls the published view
/// already lists.  A known id is [`RunningSync::ViewOnly`] whether or not
/// its cached `read_only` flag is set.  An id the view does not list is
/// [`RunningSync::Ignore`], not a request for a full read.
pub(super) fn classify_running_change(parameters: &[PluginParamView], id: u32) -> RunningSync {
    if parameters.iter().any(|parameter| parameter.id == id) {
        RunningSync::ViewOnly
    } else {
        RunningSync::Ignore
    }
}

impl PathControls {
    /// Runs one structural edit under the chain lock and republishes what
    /// the UI and the audio flag read from it before releasing the lock.
    /// `None` when the chain is poisoned.  The replaced links are dropped
    /// only after the lock is released: they may hold the last reference to
    /// a plugin the edit removed.
    pub(in crate::audio) fn edit_chain<R>(
        &self,
        edit: impl FnOnce(&mut PluginChain) -> R,
    ) -> Option<R> {
        let (result, stale) = {
            let mut chain = self.plugins.lock().ok()?;
            // Declared after the chain guard, so it runs first while
            // unwinding, still under the lock.
            let unwind = PublishOnUnwind { path: self };
            let result = edit(&mut chain);
            std::mem::forget(unwind);
            let stale = self.publish_plugins(&chain);
            (result, stale)
        };
        drop(stale);
        Some(result)
    }

    /// The insert at `index` as last published.
    fn slot_link(&self, index: usize) -> Option<SlotLink> {
        self.slot_links
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(index)
            .cloned()
    }

    /// Changes one insert in the published views, without the chain lock.
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

    /// The latency in samples this path's insert chain should add (see
    /// [`PluginChain::latency_samples`]).  Lock-free, for the audio thread.
    pub(crate) fn latency_samples(&self) -> u32 {
        self.latency.load(Ordering::Acquire)
    }

    /// Stores a chain latency without a plugin (screenshots only; the next
    /// chain edit or latency change replaces it).
    pub(crate) fn screenshot_latency(&self, samples: u32) {
        self.latency.store(samples, Ordering::Release);
    }

    /// What a poisoned chain looks like to every reader: no inserts, nothing
    /// to service, nothing to reopen.  This is what `plugin_views` returned
    /// for a poisoned chain before the views were published.
    fn publish_poisoned(&self) {
        // A panic in edit_chain clears the active flag before the callback
        // can observe the poisoned mutex.  A zero-latency chain then never
        // try-locks.
        self.chain_poisoned.store(true, Ordering::Relaxed);
        self.plugin_active.store(0, Ordering::Relaxed);
        self.chain_unopened.store(false, Ordering::Relaxed);
        self.published_ports_changed.store(0, Ordering::Relaxed);
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

    /// Publishes the links, the views and the cheap flags for `chain`.  The
    /// caller holds the chain lock.  Hands back the previous links, which
    /// the caller drops after unlocking.
    #[must_use]
    fn publish_plugins(&self, chain: &PluginChain) -> Arc<[SlotLink]> {
        self.chain_rate.store(chain.rate(), Ordering::Relaxed);
        self.chain_unopened
            .store(chain.has_unopened(), Ordering::Relaxed);
        let links: Arc<[SlotLink]> = chain.slot_links().into();
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

    /// Rebuilds the views, the active count and the port-change count from
    /// `links`, without the chain lock.  The chain's own atomics (latency,
    /// failures, ports) are what the audio thread reads too, so the result
    /// matches what it publishes per block.
    fn publish_from(&self, links: &[SlotLink]) {
        self.plugin_active.store(
            links.iter().filter(|link| link.active()).count(),
            Ordering::Relaxed,
        );
        self.published_ports_changed.store(
            links.iter().filter(|link| link.ports_changed()).count(),
            Ordering::Relaxed,
        );
        let views: Arc<[PluginView]> = links.iter().map(SlotLink::view).collect();
        *self
            .plugin_views
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = views;
        self.plugin_views_version.fetch_add(1, Ordering::Release);
    }

    /// Republishes after a change the plugins made outside the chain lock
    /// (a latency re-read, a processing failure, a port change): the
    /// latency total, the active count and the views, from the links.  The
    /// audio thread also republishes the total with its next block.
    ///
    /// It snapshots the links and publishes from that snapshot without the
    /// chain lock.  That is only correct because every chain edit (which
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
        let total = links
            .iter()
            .map(SlotLink::latency)
            .fold(0u32, u32::saturating_add);
        self.latency.store(total, Ordering::Release);
        self.publish_from(&links);
    }

    /// Replaces the saved insert list without loading anything.  Plugins are
    /// opened when the bridge starts (or when one is added in the UI).
    pub(crate) fn restore_plugins(&self, prefs: Vec<PluginPref>) {
        let old = self
            .edit_chain(|chain| chain.restore(prefs))
            .unwrap_or_default();
        // The insert counters come from the fresh SlotControls; the chain
        // lock's own counters belong to the chain this replaces.
        self.chain_attempts.store(0, Ordering::Relaxed);
        self.chain_skips.store(0, Ordering::Relaxed);
        // Destroyed after the chain lock is released.
        drop(old);
    }

    /// Adds the pure-delay test insert (see `Slot::delay_for_test`).  Lets a
    /// test outside `audio` give a path one active insert.
    #[cfg(test)]
    pub(crate) fn insert_delay_for_test(&self, latency: u32) {
        self.edit_chain(|chain| chain.insert_delay_for_test(latency));
    }

    /// Poisons the chain lock the way a panicking edit does, so a test can
    /// check what the audio thread and Diagnostics report afterwards.
    #[cfg(test)]
    pub(crate) fn poison_chain_for_test(&self) {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _chain = self.plugins.lock().expect("the chain is not poisoned yet");
            panic!("poison the chain");
        }));
    }

    /// Panics inside `edit_chain`, so the unwind guard publishes the dead
    /// chain.  Unlike `poison_chain_for_test`, this is the edit-path poison:
    /// the active flag is cleared before anything try-locks the mutex.
    #[cfg(test)]
    pub(crate) fn poison_chain_in_edit_for_test(&self) {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.edit_chain(|_chain| panic!("poison the chain in an edit"));
        }));
    }

    /// Whether the chain needs a reload for `rate`: it was built at another
    /// rate, or a restored slot was never opened.  Answered from the
    /// published flags, without the chain lock.
    /// The rate the chain was last built for (lock-free).
    pub(crate) fn chain_rate(&self) -> u32 {
        self.chain_rate.load(Ordering::Relaxed)
    }

    pub(crate) fn needs_reload(&self, rate: u32) -> bool {
        rate != self.chain_rate.load(Ordering::Relaxed)
            || self.chain_unopened.load(Ordering::Relaxed)
    }

    /// Loads outside the chain lock and drops the replaced slots only after
    /// releasing it.  `0` means 48 kHz, matching the chain's own fallback.
    pub(crate) fn reload_plugins_for_rate(&self, sample_rate: u32) {
        let rate = if sample_rate == 0 {
            48_000
        } else {
            sample_rate
        };
        const MAX_ATTEMPTS: usize = 3;
        for _ in 0..MAX_ATTEMPTS {
            let plan = match self.edit_chain(|chain| chain.reload_plan(rate)) {
                Some(Some(plan)) => plan,
                Some(None) | None => return,
            };
            let built: Vec<_> = plan
                .jobs
                .into_iter()
                .map(|(index, pref)| {
                    let (slot, _) = PluginChain::load_slot(pref.clone(), rate);
                    (index, pref, slot)
                })
                .collect();
            let installed = match self
                .edit_chain(|chain| chain.install_reload(plan.previous_rate, rate, built))
            {
                Some(Ok(old)) => Some(old),
                // Built for a chain that changed meanwhile; dropped here,
                // after the chain lock was released.
                Some(Err(stale)) => {
                    drop(stale);
                    None
                }
                None => return,
            };
            if let Some(old) = installed {
                drop(old);
                return;
            }
        }
    }

    // `push_slot` hands a mismatched slot back so it can be dropped unlocked.
    #[allow(clippy::result_large_err)]
    pub(crate) fn add_plugin(&self, pref: PluginPref) -> Result<(), String> {
        const MAX_ATTEMPTS: usize = 3;
        let mut attempts = 0;
        loop {
            let rate = self
                .plugins
                .lock()
                .map(|chain| chain.rate())
                .map_err(|_| "Plugin chain unavailable".to_owned())?;
            // Build the slot before taking the chain lock again: loading a
            // dynamic library and calling `start_processing` can take
            // milliseconds, and the audio callback only try-locks the chain.
            let (slot, result) = PluginChain::load_slot(pref.clone(), rate);
            match self
                .edit_chain(|chain| chain.push_slot(slot, rate))
                .ok_or_else(|| "Plugin chain unavailable".to_owned())?
            {
                Ok(()) => return result,
                Err(stale) => {
                    // The chain rate changed while the slot was loading, so
                    // it is set up for the wrong rate.  Drop it outside the
                    // chain lock and build a fresh one.
                    drop(stale);
                }
            }
            attempts += 1;
            if attempts >= MAX_ATTEMPTS {
                return Err(
                    "The plugin chain kept changing sample rate while the plugin was loading."
                        .to_owned(),
                );
            }
        }
    }

    pub(crate) fn remove_plugin(&self, index: usize) {
        let removed = self.edit_chain(|chain| chain.remove(index)).flatten();
        // Destroyed after the chain lock is released.
        drop(removed);
    }

    pub(crate) fn move_plugin(&self, index: usize, delta: isize) {
        self.edit_chain(|chain| chain.move_slot(index, delta));
    }

    /// Bypasses one insert, without the chain lock: the audio thread reads
    /// the flag with its next chunk.
    pub(crate) fn set_plugin_bypass(&self, index: usize, bypass: bool) {
        let Some(link) = self.slot_link(index) else {
            return;
        };
        link.controls.set_bypass(bypass);
        if !bypass {
            // The cheap "anything to run?" flag the audio thread checks
            // before locking; it recounts the active inserts itself.
            self.plugin_active.fetch_max(1, Ordering::Relaxed);
        }
        self.patch_view(index, |view| view.bypass = bypass);
    }

    /// Sets one insert's dry/wet mix, without the chain lock.
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

    /// The loaded plugin behind one insert, for its own editor window.
    /// From the published links, without the chain lock.
    pub(crate) fn plugin_handle(&self, index: usize) -> Option<PluginHandle> {
        self.slot_link(index).and_then(|link| link.plugin)
    }

    /// Copies control values back after the plugin's own editor changed them.
    ///
    /// With `full`, the whole control list is read and formatted and stored
    /// on the plugin's handle as its views and saved values (see
    /// `PluginHandle::sync_full`).  Without it, a running VST3 insert is
    /// drained once with `PluginHandle::take_changes` and each id is
    /// classified from the published view (see [`classify_running_change`]).
    /// A known id, read-only or writable, patches that view's value and
    /// display and nothing else.  An id the view does not list is ignored.
    /// That poll does not write the saved parameters and does not mark the
    /// slot stale.  A CLAP plugin has no change feed, and an insert that has
    /// not processed a block since the last sync has no visible changes yet,
    /// so both cases read in full instead (see [`needs_full_sync`]).  A
    /// plugin that reported a changed parameter list (`param_titles_changed`
    /// / `param_values_changed`) is forced to the full read too.  A control
    /// the plugin actually added, or a changed name or default on an
    /// existing id, is picked up by that full read on save, close, bypass,
    /// stop, or bridge start.  Neither takes the chain lock.
    ///
    /// A missing or unloaded insert is a silent no-op.
    pub(crate) fn sync_plugin_parameters(&self, index: usize, full: bool) -> Result<(), String> {
        let Some(link) = self.slot_link(index) else {
            return Ok(());
        };
        let processed = link.controls.take_processed();
        let Some(handle) = link.plugin else {
            return Ok(());
        };
        // Keep the VST3 restart/parameter poll window open for as long as the
        // editor is being synced, even when it reports no parameter changes:
        // a request raised by an editor action that does not call
        // `performEdit` would otherwise go unseen once the window lapses.
        // This is a plain atomic store, on the UI thread.
        handle.nudge();
        // Decided before draining, from the processed flag and the service's
        // requests (atomics, no plugin call).
        let full = needs_full_sync(
            full || handle.take_parameters_changed(),
            processed,
            handle.is_vst3(),
        );
        if full {
            if let Some(parameters) = handle.sync_full() {
                self.patch_view(index, |view| view.parameters = parameters);
            }
            return Ok(());
        }
        let Some(changes) = handle.take_changes() else {
            return Ok(());
        };
        if changes.is_empty() {
            return Ok(());
        }
        // The instance lock is released.  Classify from the published view,
        // which already caches `read_only`.  No second plugin call.
        let views = self.plugin_views();
        let Some(cached) = views.get(index).map(|view| &view.parameters) else {
            return Ok(());
        };
        if !changes
            .iter()
            .any(|change| classify_running_change(cached, change.id) == RunningSync::ViewOnly)
        {
            return Ok(());
        }
        self.patch_view(index, |view| {
            for change in &changes {
                if classify_running_change(&view.parameters, change.id) != RunningSync::ViewOnly {
                    continue;
                }
                if let Some(parameter) = view
                    .parameters
                    .iter_mut()
                    .find(|parameter| parameter.id == change.id)
                {
                    parameter.value = change.value;
                    if let Some(display) = &change.display {
                        parameter.display = display.clone();
                    }
                }
            }
        });
        Ok(())
    }

    /// Sets one plugin control.  A loaded plugin is reached through the
    /// published links, without the chain lock (only the plugin's own lock,
    /// which the audio thread just skips the insert for): its handle stores
    /// the value and the published view is patched at once.  An insert that
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
    /// reset through its link, without the chain lock; one that is not open
    /// yet goes through the chain, which opens it.
    pub(crate) fn reset_plugin_parameters(&self, index: usize) -> Result<(), String> {
        let Some(handle) = self.plugin_handle(index) else {
            return self
                .edit_chain(|chain| chain.reset_parameters(index))
                .unwrap_or_else(|| Err("Plugin chain unavailable".to_owned()));
        };
        let parameters = handle.reset_parameters()?;
        self.patch_view(index, |view| view.parameters = parameters);
        Ok(())
    }

    /// The insert views as last published.  Never takes the chain lock, so
    /// drawing the UI cannot make the audio callback miss a chain block.
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

    /// Runs this path's CLAP main-thread callbacks and timers, and the VST3
    /// restart requests, once per UI frame, outside the chain lock: a
    /// plugin's timer can do arbitrary work.  The plugins come from the
    /// published links.  A latency change, a processing failure or a port
    /// change republishes from the links too, so no frame takes the chain
    /// lock.
    pub(crate) fn service_plugins(&self) {
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
        let ports_changed = links.iter().filter(|link| link.ports_changed()).count();
        let dirty = self.plugin_views_dirty.swap(false, Ordering::AcqRel);
        if dirty
            || latency_changed
            || ports_changed != self.published_ports_changed.load(Ordering::Relaxed)
        {
            self.refresh_from_links();
        }
        // Dropped on this thread, in case a removal left it as the last
        // reference to a plugin.
        drop(links);
    }

    /// Captures the opaque state of every loaded plugin whose saved blob is
    /// missing, stale or marked dirty.  Runs on the UI thread (the CLAP main
    /// thread), through the published links: no chain lock.
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

    /// Marks one insert's saved state as out of date (its editor closed).
    pub(crate) fn mark_plugin_state_stale(&self, index: usize) {
        if let Some(handle) = self.plugin_handle(index) {
            handle.mark_state_stale();
        }
    }

    /// The insert chain as it should be saved, with fresh plugin state.
    /// Built from the published links, without the chain lock.  Call it
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
        self.plugin_active.load(Ordering::Relaxed) > 0
    }

    /// Called from the output callback.  Returns whether the chain ran;
    /// `false` leaves the block untouched and the caller plays the chain's
    /// input delayed by [`Self::latency_samples`] instead (see
    /// `mixer::PathProcessor::run_inserts` and the module docs).
    ///
    /// The flags keep the common no-plugin case lock-free.  A chain with no
    /// active insert but some latency (every insert bypassed) still runs, so
    /// its skipped inserts delay the block by their latency.  The path's
    /// chain bypass is an atomic the chain loads while this holds it by
    /// `try_lock`.  When it is set, every insert is skipped, the latency
    /// still counts, and each slot's own bypass is left alone.  `try_lock`
    /// keeps a UI edit from stalling the block.
    ///
    /// Every real attempt on the chain lock is counted; so is a lock found
    /// busy, which leaves the block to the dry fallback below.  A poisoned
    /// lock is not counted as a skip: it is not contention, and the chain
    /// stays unusable for the rest of the session (see
    /// [`PluginLockReport::chain_poisoned`]).  Measurement only: the counters
    /// are written relaxedly and Diagnostics reads them.
    pub(crate) fn process_plugins(
        &self,
        left: &mut [f32],
        right: &mut [f32],
        frames: usize,
    ) -> bool {
        if self.plugin_active.load(Ordering::Relaxed) == 0 && self.latency_samples() == 0 {
            return true;
        }
        self.chain_attempts.fetch_add(1, Ordering::Relaxed);
        let mut chain = match self.plugins.try_lock() {
            Ok(chain) => chain,
            Err(TryLockError::WouldBlock) => {
                self.chain_skips.fetch_add(1, Ordering::Relaxed);
                return false;
            }
            Err(TryLockError::Poisoned(_)) => {
                // An earlier panic held the chain lock: the inserts are gone
                // for this session, and no poll will bring them back.
                self.chain_poisoned.store(true, Ordering::Relaxed);
                return false;
            }
        };
        let failed = chain.process(left, right, frames);
        self.plugin_active.store(chain.active(), Ordering::Relaxed);
        if failed {
            // The views are rebuilt on the UI thread, not here.
            self.plugin_views_dirty.store(true, Ordering::Release);
        }
        true
    }

    /// This path's plugin lock counters: the chain lock and each insert's
    /// own plugin lock.  Measurement only; never takes the chain lock.
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
            inserts: links
                .iter()
                .map(|link| link.controls.lock_stats())
                .collect(),
            chain_poisoned: self.chain_poisoned.load(Ordering::Relaxed),
        }
    }

    /// Clears this path's plugin lock counters (UI thread).  It does not
    /// clear [`PluginLockReport::chain_poisoned`]: resetting a counter cannot
    /// make a poisoned chain usable.
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
