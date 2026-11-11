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
//! - a plugin control goes straight to the plugin through the published
//!   insert links (`slot_links`), holding only that plugin's own lock (the
//!   audio thread then skips just that insert for a chunk, delayed by its
//!   latency, never the chain); the view is patched at once and the saved
//!   value waits in `param_backlog` until the UI takes the chain lock for
//!   something else (an edit, a state capture, a save);
//! - drawing reads the published views, and a quiet service frame takes no
//!   lock.
//!
//! What still takes the chain lock is structural and short: adding,
//! removing, moving or restoring inserts, reopening them for a new rate
//! (slow loads and teardowns run outside the lock), resetting a plugin's
//! controls, capturing state for a save, a full editor sync, and the publish
//! of the views those edits do.  The running poll of a VST3 insert does not
//! take it: the changes it reported only patch the published view.  A CLAP
//! editor (no change feed) and an insert that has not processed a block
//! since the last sync are read in full instead.
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
    host_format_parameter, host_set_parameter, take_parameter_changes, take_parameter_values,
    PluginChain, PluginHandle, PluginParamView, PluginPref, PluginView, SlotLink, Vst3Service,
};
use super::{ClapService, LockStats, PathControls, PluginLockReport};

/// The service lists a publish replaces; dropped after the chain lock is
/// released, since they may hold the last reference to a removed plugin.
type StaleServices = (Arc<[ClapService]>, Arc<[Arc<Vst3Service>]>, Arc<[SlotLink]>);

/// A control value the UI set on a plugin without the chain lock, waiting
/// to be stored in the chain's insert (its view and saved value) the next
/// time the UI takes the lock anyway.
pub(super) struct ParamWrite {
    index: usize,
    handle: PluginHandle,
    id: u32,
    value: f64,
    display: Option<String>,
}

/// Armed by `edit_chain` around the edit itself.  If the edit panics, the
/// chain mutex is poisoned and no later edit publishes again, so the
/// published views and CLAP services are cleared here instead of being left
/// to describe (and hold references to) slots the edit may have changed.
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

/// What the running poll does with one id from [`take_parameter_changes`],
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
    /// Runs one edit under the chain lock and republishes what the UI and
    /// the audio flag read from it before releasing the lock.  `None` when
    /// the chain is poisoned.  The replaced service lists are dropped
    /// only after the lock is released: it may hold the last reference to a
    /// plugin the edit removed.
    pub(in crate::audio) fn edit_chain<R>(
        &self,
        edit: impl FnOnce(&mut PluginChain) -> R,
    ) -> Option<R> {
        let (result, stale, stored) = {
            let mut chain = self.plugins.lock().ok()?;
            let stored = self.store_backlog(&mut chain);
            // Declared after the chain guard, so it runs first while
            // unwinding, still under the lock.
            let unwind = PublishOnUnwind { path: self };
            let result = edit(&mut chain);
            std::mem::forget(unwind);
            let stale = self.publish_plugins(&chain);
            (result, stale, stored)
        };
        drop((stale, stored));
        Some(result)
    }

    /// Takes the chain lock for UI-side work that does not change the
    /// inserts, with the control values set meanwhile stored first.  The
    /// stored writes (they hold plugin references) are dropped after the
    /// lock is released.
    fn with_chain<R>(&self, work: impl FnOnce(&mut PluginChain) -> R) -> Option<R> {
        let (result, stored) = {
            let mut chain = self.plugins.lock().ok()?;
            let stored = self.store_backlog(&mut chain);
            (work(&mut chain), stored)
        };
        drop(stored);
        Some(result)
    }

    /// Stores the control values set without the chain lock (see
    /// [`Self::set_plugin_parameter`]).  The caller holds the chain lock
    /// and drops the returned writes after releasing it.
    #[must_use]
    fn store_backlog(&self, chain: &mut PluginChain) -> Vec<ParamWrite> {
        let writes = std::mem::take(
            &mut *self
                .param_backlog
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        for write in &writes {
            chain.store_parameter(
                write.index,
                &write.handle,
                write.id,
                write.value,
                write.display.clone(),
            );
        }
        writes
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
        let services = std::mem::replace(
            &mut *self
                .plugin_services
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
            Arc::from(Vec::new()),
        );
        let vst3 = std::mem::replace(
            &mut *self
                .vst3_services
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
            Arc::from(Vec::new()),
        );
        let links = std::mem::replace(
            &mut *self
                .slot_links
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
            Arc::from(Vec::new()),
        );
        let backlog = std::mem::take(
            &mut *self
                .param_backlog
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        drop((services, vst3, links, backlog));
    }

    /// Publishes the views, the CLAP and VST3 service lists and the cheap
    /// flags for `chain`.  The caller holds the chain lock.  Hands back the
    /// previous service lists, which the caller drops after unlocking.
    #[must_use]
    fn publish_plugins(&self, chain: &PluginChain) -> StaleServices {
        self.plugin_active.store(chain.active(), Ordering::Relaxed);
        self.chain_rate.store(chain.rate(), Ordering::Relaxed);
        self.chain_unopened
            .store(chain.has_unopened(), Ordering::Relaxed);
        self.published_ports_changed
            .store(chain.ports_changed_count(), Ordering::Relaxed);
        let views: Arc<[PluginView]> = chain.views().into();
        let services: Arc<[ClapService]> = chain.clap_services().into();
        let vst3: Arc<[Arc<Vst3Service>]> = chain.vst3_services().into();
        let links: Arc<[SlotLink]> = chain.slot_links().into();
        *self
            .plugin_views
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = views;
        self.plugin_views_version.fetch_add(1, Ordering::Release);
        let stale_clap = std::mem::replace(
            &mut *self
                .plugin_services
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
            services,
        );
        let stale_vst3 = std::mem::replace(
            &mut *self
                .vst3_services
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
            vst3,
        );
        let stale_links = std::mem::replace(
            &mut *self
                .slot_links
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
            links,
        );
        (stale_clap, stale_vst3, stale_links)
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
    pub(crate) fn plugin_handle(&self, index: usize) -> Option<PluginHandle> {
        self.plugins
            .lock()
            .ok()
            .and_then(|chain| chain.handle(index))
    }

    /// Copies control values back after the plugin's own editor changed them.
    ///
    /// With `full`, the whole control list is read and formatted, and the
    /// reporter is only drained (`take_parameter_values`).  Without it, a
    /// running VST3 insert is drained once with [`take_parameter_changes`]
    /// and each id is classified from the published view (see
    /// [`classify_running_change`]).  A known id, read-only or writable,
    /// patches that view's value and display and nothing else.  An id the
    /// view does not list is ignored.  That poll does not take the chain
    /// lock, does not write the saved parameters, and does not mark the slot
    /// stale.  A CLAP plugin has no change feed, and an insert that has not
    /// processed a block since the last sync has no visible changes yet, so
    /// both cases read in full instead (see [`needs_full_sync`]).  A plugin
    /// that reported a changed parameter list (`param_titles_changed` /
    /// `param_values_changed`) is forced to the full read too.  A control the
    /// plugin actually added, or a changed name or default on an existing id,
    /// is picked up by that full read on save, close, bypass, stop, or bridge
    /// start.
    ///
    /// A missing or unloaded insert is a silent no-op.
    pub(crate) fn sync_plugin_parameters(&self, index: usize, full: bool) -> Result<(), String> {
        // From the published links, so the periodic sync never takes the
        // chain lock just to find the plugin.
        let link = self.slot_link(index);
        // Keep the VST3 restart/parameter poll window open for as long as the
        // editor is being synced, even when it reports no parameter changes:
        // a request raised by an editor action that does not call
        // `performEdit` would otherwise go unseen once the window lapses.
        // This is a plain atomic store, on the UI thread.
        if let Some(service) = link.as_ref().and_then(|link| link.vst3.as_ref()) {
            service.nudge();
        }
        let parameters_changed = link
            .as_ref()
            .and_then(|link| link.vst3.as_ref())
            .is_some_and(|vst3| vst3.take_parameters_changed());
        let Some(link) = link else {
            return Ok(());
        };
        let processed = link.controls.take_processed();
        let has_vst3 = link.vst3.is_some();
        let Some(handle) = link.plugin else {
            return Ok(());
        };
        // Decided before draining, from the processed flag and the service's
        // requests (atomics, no plugin call).
        let full = needs_full_sync(full || parameters_changed, processed, has_vst3);
        if !full {
            let Some(changes) = take_parameter_changes(&handle) else {
                return Ok(());
            };
            if changes.is_empty() {
                return Ok(());
            }
            // The instance lock is released.  Classify from the published
            // view, which already caches `read_only`.  No second plugin call
            // and no chain lock.
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
                    if classify_running_change(&view.parameters, change.id) != RunningSync::ViewOnly
                    {
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
            return Ok(());
        }
        // Drain the reporter before the full read, without formatting:
        // a value queued before the fresher read would otherwise be
        // applied by the next change poll, putting an older value back.
        drop(take_parameter_values(&handle));
        let Some(parameters) = super::super::plugins::read_plugin_parameters(&handle) else {
            return Ok(());
        };
        self.edit_chain(|chain| chain.apply_synced_parameters(index, &handle, parameters))
            .ok_or_else(|| "Plugin chain unavailable".to_owned())
    }

    /// Sets one plugin control.  A loaded plugin is reached through the
    /// published links, without the chain lock (only the plugin's own lock,
    /// which the audio thread just skips the insert for): the view is
    /// patched at once and the saved value is stored the next time the UI
    /// takes the chain lock anyway.  An insert that is not open yet goes
    /// through the chain, which opens it.
    pub(crate) fn set_plugin_parameter(
        &self,
        index: usize,
        id: u32,
        value: f64,
    ) -> Result<(), String> {
        let Some(handle) = self.slot_link(index).and_then(|link| {
            if let Some(vst3) = &link.vst3 {
                vst3.nudge();
            }
            link.plugin
        }) else {
            return self
                .edit_chain(|chain| chain.set_parameter(index, id, value))
                .unwrap_or_else(|| Err("Plugin chain unavailable".to_owned()));
        };
        let value = value.clamp(0.0, 1.0);
        host_set_parameter(&handle, id, value)?;
        let display = host_format_parameter(&handle, id, value);
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
        let mut backlog = self
            .param_backlog
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // A drag sends many values for one control; only the last counts.
        backlog.retain(|write| !(write.index == index && write.id == id));
        backlog.push(ParamWrite {
            index,
            handle,
            id,
            value,
            display,
        });
        Ok(())
    }

    pub(crate) fn reset_plugin_parameters(&self, index: usize) -> Result<(), String> {
        self.edit_chain(|chain| chain.reset_parameters(index))
            .unwrap_or_else(|| Err("Plugin chain unavailable".to_owned()))
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
    /// published lists, so a quiet frame takes no chain lock at all.
    pub(crate) fn service_plugins(&self) {
        let services = Arc::clone(
            &self
                .plugin_services
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        let vst3 = Arc::clone(
            &self
                .vst3_services
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        let mut latency_changed = false;
        for (host, plugin) in services.iter() {
            super::super::plugins::service_clap(host, plugin);
            // Taken from the host rather than the call's result, so a change
            // an open editor's own service picked up counts here too.
            latency_changed |= host.take_latency_change();
        }
        for service in vst3.iter() {
            latency_changed |= service.service();
        }
        // An insert failed on the audio thread, or a CLAP plugin changed its
        // ports since the last publish: rebuild the views here, on the UI
        // thread, under a normal chain lock.
        let ports_changed = services
            .iter()
            .filter(|(host, _)| host.ports_changed())
            .count();
        let dirty = self.plugin_views_dirty.swap(false, Ordering::AcqRel);
        if dirty
            || latency_changed
            || ports_changed != self.published_ports_changed.load(Ordering::Relaxed)
        {
            // Also refreshes the chain's latency total.
            self.edit_chain(PluginChain::refresh_latency);
        }
        // Dropped outside the chain lock, on this thread, in case a removal
        // left it as the last reference to a plugin.
        drop((services, vst3));
    }

    /// Captures the opaque state of every loaded plugin whose saved blob is
    /// missing or stale.  Runs on the UI thread (the CLAP main thread); the
    /// plugins are saved without the chain lock, which is only taken briefly
    /// to list them and to store the results.
    pub(crate) fn capture_plugin_states(&self) {
        let Some(requests) = self.with_chain(|chain| chain.state_requests()) else {
            return;
        };
        if requests.is_empty() {
            return;
        }
        let captured: Vec<_> = requests
            .iter()
            .map(super::super::plugins::capture_state)
            .collect();
        self.with_chain(|chain| {
            for (request, state) in requests.iter().zip(captured) {
                chain.store_state(request.index, &request.handle, state);
            }
        });
    }

    /// Marks one insert's saved state as out of date (its editor closed).
    pub(crate) fn mark_plugin_state_stale(&self, index: usize) {
        self.with_chain(|chain| chain.mark_state_stale(index));
    }

    /// The insert chain as it should be saved, with fresh plugin state.
    /// Call it from the UI thread.
    pub(crate) fn plugin_prefs(&self) -> Vec<PluginPref> {
        self.capture_plugin_states();
        self.with_chain(|chain| chain.prefs()).unwrap_or_default()
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
    /// its skipped inserts delay the block by their latency.  `try_lock`
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
