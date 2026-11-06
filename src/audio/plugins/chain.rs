//! The master insert chain.

use std::sync::{Arc, Mutex, TryLockError};

use vst3_host::AudioBuffers;

use super::super::clap::{ClapPlugin, HostState};
use super::load::{
    host_reset_parameters, host_set_parameter, merge_synced_params, Capture, PluginHandle,
    StateRequest, MAX_BLOCK,
};
#[cfg(test)]
use super::pref::PluginFormat;
use super::pref::{PluginParamPref, PluginParamView, PluginPref, PluginView};
use super::slot::{defer_plugin_slots, Slot};

/// Slots [`PluginChain::reload_plan`] wants opened at a new or first rate.
/// The caller builds them without the chain lock.
pub(crate) struct ReloadPlan {
    pub(crate) previous_rate: u32,
    pub(crate) jobs: Vec<(usize, PluginPref)>,
}

/// The master insert chain.  The audio callback only try-locks the chain and
/// each loaded plugin, so an edit or an open editor skips that processing for
/// a block instead of stalling the callback; UI paths may still block on the
/// chain lock.
pub(crate) struct PluginChain {
    slots: Vec<Slot>,
    buffers: AudioBuffers,
    dry_l: Vec<f32>,
    dry_r: Vec<f32>,
    sample_rate: u32,
}

impl PluginChain {
    pub(crate) fn new(sample_rate: u32) -> Self {
        let rate = if sample_rate == 0 {
            48_000
        } else {
            sample_rate
        };
        Self {
            slots: Vec::new(),
            buffers: AudioBuffers::new(2, 2, MAX_BLOCK, rate as f64),
            dry_l: vec![0.0; MAX_BLOCK],
            dry_r: vec![0.0; MAX_BLOCK],
            sample_rate: rate,
        }
    }

    /// Replaces the slots and hands the old ones back, so the caller can
    /// drop (and destroy) them after releasing the chain lock.
    pub(crate) fn restore(&mut self, prefs: Vec<PluginPref>) -> Vec<Slot> {
        std::mem::replace(
            &mut self.slots,
            prefs.into_iter().map(Slot::empty).collect(),
        )
    }

    /// Builds a fully opened slot without holding the chain lock.  Add it with
    /// [`Self::push_slot`] once it is ready, so a slow library load or
    /// `start_processing` never stalls the audio callback.  A failed load is
    /// returned as an error but remains a valid slot to push, so the failure
    /// stays visible in the insert list instead of disappearing.
    pub(crate) fn load_slot(pref: PluginPref, sample_rate: u32) -> (Slot, Result<(), String>) {
        let slot = Slot::loaded(pref, sample_rate);
        let result = match &slot.error {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        };
        (slot, result)
    }

    /// Splices an already loaded slot into the chain, but only when it was
    /// built for the chain's current rate.  The caller holds the chain lock.
    /// A mismatched slot is handed back so the caller can release the lock
    /// before dropping it and rebuild at the new rate.
    #[allow(clippy::result_large_err)] // Slot is returned so the caller can rebuild it.
    pub(crate) fn push_slot(&mut self, slot: Slot, loaded_rate: u32) -> Result<(), Slot> {
        if loaded_rate != self.sample_rate {
            return Err(slot);
        }
        self.slots.push(slot);
        Ok(())
    }

    /// Lists slots to rebuild at `rate` without loading them.  When the rate
    /// changed and nothing is installed, the rate is updated here.  The
    /// caller loads [`ReloadPlan::jobs`] outside the chain lock and commits
    /// them with [`Self::install_reload`].
    pub(crate) fn reload_plan(&mut self, rate: u32) -> Option<ReloadPlan> {
        let rate_changed = rate != self.sample_rate;
        let jobs: Vec<(usize, PluginPref)> = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| rate_changed || slot.needs_open())
            .map(|(index, slot)| (index, slot.pref.clone()))
            .collect();
        if jobs.is_empty() {
            if rate_changed {
                self.apply_rate(rate);
            }
            return None;
        }
        Some(ReloadPlan {
            previous_rate: self.sample_rate,
            jobs,
        })
    }

    /// Swaps in slots built outside the lock.  On success the replaced slots
    /// are returned so the caller can drop them after releasing the lock.
    /// On conflict the new slots are returned instead and the chain is left
    /// as it was.
    pub(crate) fn install_reload(
        &mut self,
        previous_rate: u32,
        rate: u32,
        built: Vec<(usize, PluginPref, Slot)>,
    ) -> Result<Vec<Slot>, Vec<Slot>> {
        let unchanged = self.sample_rate == previous_rate
            && built.iter().all(|(index, pref, _)| {
                self.slots
                    .get(*index)
                    .is_some_and(|slot| &slot.pref == pref)
            });
        if !unchanged {
            return Err(built.into_iter().map(|(_, _, slot)| slot).collect());
        }
        let mut old = Vec::with_capacity(built.len());
        for (index, _, slot) in built {
            old.push(std::mem::replace(&mut self.slots[index], slot));
        }
        self.apply_rate(rate);
        Ok(old)
    }

    fn apply_rate(&mut self, rate: u32) {
        if rate == self.sample_rate {
            return;
        }
        self.sample_rate = rate;
        self.buffers = AudioBuffers::new(2, 2, MAX_BLOCK, rate as f64);
    }

    #[cfg(test)]
    pub(crate) fn built_rates(&self) -> Vec<Option<u32>> {
        self.slots.iter().map(|slot| slot.built_rate).collect()
    }

    #[cfg(test)]
    pub(crate) fn insert_tracked_slot_for_test(&mut self) {
        self.slots.push(Slot::tracked_for_test());
    }

    /// Adds an already loaded CLAP plugin as a slot, the way a real load
    /// fills one in.
    #[cfg(test)]
    pub(crate) fn insert_clap_for_test(&mut self, plugin: ClapPlugin) {
        let host = plugin.host();
        let mut slot = Slot::empty(PluginPref {
            name: "Fake".to_owned(),
            format: PluginFormat::Clap,
            ..PluginPref::default()
        });
        slot.plugin = Some(PluginHandle::Clap(Arc::new(Mutex::new(plugin))));
        slot.clap_host = Some(host);
        slot.built_rate = Some(self.sample_rate);
        self.slots.push(slot);
    }

    pub(crate) fn rate(&self) -> u32 {
        self.sample_rate
    }

    pub(crate) fn views(&self) -> Vec<PluginView> {
        self.slots.iter().map(Slot::view).collect()
    }

    pub(crate) fn prefs(&self) -> Vec<PluginPref> {
        self.slots.iter().map(|slot| slot.pref.clone()).collect()
    }

    /// Whether a restored slot is still waiting for its first open.
    pub(crate) fn has_unopened(&self) -> bool {
        self.slots.iter().any(Slot::needs_open)
    }

    /// How many CLAP slots report a port change.  Published with the views,
    /// so the UI thread can notice a new one without the chain lock.
    pub(crate) fn ports_changed_count(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| slot.ports_changed())
            .count()
    }

    pub(crate) fn active(&self) -> usize {
        self.slots.iter().filter(|slot| slot.active()).count()
    }

    /// Takes a slot out of the chain.  The caller drops it after releasing
    /// the chain lock: destroying a plugin can take a while, and the audio
    /// callback passes the chain through dry whenever it cannot get the lock.
    pub(crate) fn remove(&mut self, index: usize) -> Option<Slot> {
        (index < self.slots.len()).then(|| self.slots.remove(index))
    }

    pub(crate) fn move_slot(&mut self, index: usize, delta: isize) {
        if index >= self.slots.len() {
            return;
        }
        let target = (index as isize + delta).clamp(0, self.slots.len() as isize - 1) as usize;
        if target != index {
            self.slots.swap(index, target);
        }
    }

    pub(crate) fn set_bypass(&mut self, index: usize, bypass: bool) {
        if let Some(slot) = self.slots.get_mut(index) {
            slot.pref.bypass = bypass;
        }
    }

    pub(crate) fn set_mix(&mut self, index: usize, mix: f32) {
        if let Some(slot) = self.slots.get_mut(index) {
            slot.pref.mix = mix.clamp(0.0, 1.0);
        }
    }

    /// Opens a saved slot so its controls can be shown before playback starts.
    pub(crate) fn ensure_loaded(&mut self, index: usize) {
        let rate = self.sample_rate;
        if let Some(slot) = self.slots.get_mut(index) {
            slot.ensure_loaded(rate);
        }
    }

    /// The plugin instance behind one slot, shared with the window that may
    /// show the plugin's own editor.
    pub(crate) fn handle(&self, index: usize) -> Option<PluginHandle> {
        self.slots.get(index).and_then(|slot| slot.plugin.clone())
    }

    /// The CLAP plugins that need main-thread servicing.  The caller runs
    /// it with [`super::service_clap`] after releasing the chain lock.
    pub(crate) fn clap_services(&self) -> Vec<(Arc<HostState>, Arc<Mutex<ClapPlugin>>)> {
        self.slots
            .iter()
            .filter_map(|slot| match (&slot.clap_host, &slot.plugin) {
                (Some(host), Some(PluginHandle::Clap(plugin))) => {
                    Some((Arc::clone(host), Arc::clone(plugin)))
                }
                _ => None,
            })
            .collect()
    }

    /// The plugin behind one slot, for work that must not hold the chain
    /// lock.  Fails with the slot's own error when it is not loaded.
    pub(crate) fn loaded_handle(&self, index: usize) -> Result<PluginHandle, String> {
        let Some(slot) = self.slots.get(index) else {
            return Err("That insert is no longer in the chain".to_owned());
        };
        slot.plugin.clone().ok_or_else(|| {
            slot.resolved_error()
                .unwrap_or_else(|| "The plugin is not loaded".to_owned())
        })
    }

    /// Stores controls read with [`super::read_plugin_parameters`] back into the
    /// slot, provided it still holds the same plugin instance.
    pub(crate) fn apply_synced_parameters(
        &mut self,
        index: usize,
        handle: &PluginHandle,
        parameters: Vec<PluginParamView>,
    ) {
        let Some(slot) = self.slots.get_mut(index) else {
            return;
        };
        if !slot
            .plugin
            .as_ref()
            .is_some_and(|current| current.same(handle))
        {
            return;
        }
        slot.pref.params = merge_synced_params(&slot.pref.params, &parameters);
        slot.parameters = parameters;
        // The editor is open and can change more than parameters.
        slot.state_stale = true;
    }

    /// The loaded plugins whose state should be captured now: those whose
    /// saved blob is missing, did not restore, or is older than a control
    /// change, and CLAP plugins that marked themselves dirty.  The caller
    /// captures them with [`super::capture_state`] after releasing the chain lock
    /// and hands the results to [`Self::store_state`].
    pub(crate) fn state_requests(&self) -> Vec<StateRequest> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| {
                let handle = slot.plugin.clone()?;
                let dirty = slot
                    .clap_host
                    .as_ref()
                    .is_some_and(|host| host.state_dirty());
                (slot.state_stale || dirty).then(|| StateRequest {
                    index,
                    handle,
                    host: slot.clap_host.clone(),
                    params: slot.pref.params.clone(),
                })
            })
            .collect()
    }

    /// Stores a captured state blob, provided the slot still holds the same
    /// plugin instance.  A failed capture keeps the previous blob, whose
    /// parameter snapshot still lets newer control values win on restore,
    /// and is not retried until a control changes again.  A deferred one
    /// keeps the slot stale so the next save retries.
    pub(crate) fn store_state(&mut self, index: usize, handle: &PluginHandle, capture: Capture) {
        let Some(slot) = self.slots.get_mut(index) else {
            return;
        };
        if !slot
            .plugin
            .as_ref()
            .is_some_and(|current| current.same(handle))
        {
            return;
        }
        slot.state_stale = capture.leaves_stale();
        if let Capture::Saved(state) = capture {
            slot.pref.state = Some(state);
        }
    }

    /// Marks one slot's saved state as out of date, e.g. when its editor
    /// closes, so the next preference save captures it.
    pub(crate) fn mark_state_stale(&mut self, index: usize) {
        if let Some(slot) = self.slots.get_mut(index) {
            if slot.plugin.is_some() {
                slot.state_stale = true;
            }
        }
    }

    /// Sets one normalized plugin control and keeps the saved insert in step.
    pub(crate) fn set_parameter(
        &mut self,
        index: usize,
        id: u32,
        value: f64,
    ) -> Result<(), String> {
        let rate = self.sample_rate;
        let Some(slot) = self.slots.get_mut(index) else {
            return Err("That insert is no longer in the chain".to_owned());
        };
        slot.load_if_needed(rate);
        let Some(plugin) = slot.plugin.as_ref() else {
            return Err(slot
                .error
                .clone()
                .unwrap_or_else(|| "The plugin is not loaded".to_owned()));
        };
        let value = value.clamp(0.0, 1.0);
        host_set_parameter(plugin, id, value)?;
        // Only the moved control is refreshed; re-reading every parameter on
        // each drag event held the plugin (and the chain) far longer.
        let display = match plugin {
            PluginHandle::Vst3(vst3) => {
                let vst3 = vst3.lock().unwrap_or_else(|poison| poison.into_inner());
                vst3.format_parameter(id, value).ok()
            }
            PluginHandle::Clap(clap) => Some(
                clap.lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .display_for(id, value),
            ),
        };
        if let Some(parameter) = slot
            .parameters
            .iter_mut()
            .find(|parameter| parameter.id == id)
        {
            parameter.value = value;
            if let Some(display) = display {
                parameter.display = display;
            }
        }
        if let Some(saved) = slot.pref.params.iter_mut().find(|saved| saved.id == id) {
            saved.value = value;
        } else {
            slot.pref.params.push(PluginParamPref { id, value });
        }
        slot.state_stale = true;
        Ok(())
    }

    /// Returns every control to the plugin's own defaults and forgets the
    /// saved overrides.
    pub(crate) fn reset_parameters(&mut self, index: usize) -> Result<(), String> {
        let rate = self.sample_rate;
        let Some(slot) = self.slots.get_mut(index) else {
            return Err("That insert is no longer in the chain".to_owned());
        };
        slot.load_if_needed(rate);
        let Some(plugin) = slot.plugin.as_ref() else {
            return Err(slot
                .error
                .clone()
                .unwrap_or_else(|| "The plugin is not loaded".to_owned()));
        };
        slot.parameters = host_reset_parameters(plugin)?;
        slot.pref.params.clear();
        slot.state_stale = true;
        Ok(())
    }

    /// Runs the enabled effects over one stereo block.  A plugin whose lock is
    /// held (its editor is doing UI work) is skipped for this block, and a
    /// plugin that fails is left in the chain with its error so the person can
    /// see why it is silent.
    ///
    /// Returns whether a slot gained an error in this block, so the caller
    /// can ask the UI thread to republish the insert views (the audio thread
    /// does not build them itself).
    pub(crate) fn process(&mut self, left: &mut [f32], right: &mut [f32], frames: usize) -> bool {
        let frames = frames.min(left.len()).min(right.len());
        if frames == 0 {
            return false;
        }
        let mut failed = false;
        let Self {
            slots,
            buffers,
            dry_l,
            dry_r,
            ..
        } = self;
        for slot in slots.iter_mut() {
            if !slot.active() {
                continue;
            }
            let mix = slot.pref.mix.clamp(0.0, 1.0);
            if mix <= 0.0 {
                continue;
            }
            // The slot keeps its own reference while the audio thread holds
            // the chain lock, so dropping this clone is never the last
            // release: a plugin (and its CLAP module, whose release takes
            // the module registry lock and may `deinit`) is never torn down
            // on the audio thread.
            let Some(handle) = slot.plugin.clone() else {
                continue;
            };
            match handle {
                PluginHandle::Vst3(plugin) => {
                    // The plugin's editor holds this lock while it runs UI work.
                    // Skipping the insert for this block is better than blocking
                    // the audio callback behind the UI thread.
                    let mut plugin = match plugin.try_lock() {
                        Ok(plugin) => plugin,
                        Err(TryLockError::Poisoned(poison)) => poison.into_inner(),
                        Err(TryLockError::WouldBlock) => continue,
                    };
                    let mut offset = 0;
                    while offset < frames {
                        let count = (frames - offset).min(MAX_BLOCK);
                        buffers.block_size = count;
                        // The VST3 host works out how many frames to run from the
                        // length of these vectors, not from `block_size`.  They
                        // are allocated at MAX_BLOCK, so without trimming them to
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
                        buffers.inputs[0].copy_from_slice(&left[offset..offset + count]);
                        buffers.inputs[1].copy_from_slice(&right[offset..offset + count]);
                        buffers.outputs[0].fill(0.0);
                        buffers.outputs[1].fill(0.0);
                        if mix < 1.0 {
                            dry_l[..count].copy_from_slice(&left[offset..offset + count]);
                            dry_r[..count].copy_from_slice(&right[offset..offset + count]);
                        }
                        match plugin.process_audio(buffers) {
                            Ok(()) => {
                                for sample in 0..count {
                                    let wet_l = buffers.outputs[0][sample];
                                    let wet_r = buffers.outputs[1][sample];
                                    if mix < 1.0 {
                                        let dry = 1.0 - mix;
                                        left[offset + sample] = dry_l[sample] * dry + wet_l * mix;
                                        right[offset + sample] = dry_r[sample] * dry + wet_r * mix;
                                    } else {
                                        left[offset + sample] = wet_l;
                                        right[offset + sample] = wet_r;
                                    }
                                }
                            }
                            Err(error) => {
                                drop(plugin);
                                slot.error = Some(format!("Processing failed: {error}"));
                                failed = true;
                                break;
                            }
                        }
                        offset += count;
                    }
                }
                PluginHandle::Clap(plugin) => {
                    let mut plugin = match plugin.try_lock() {
                        Ok(plugin) => plugin,
                        Err(TryLockError::Poisoned(poison)) => poison.into_inner(),
                        Err(TryLockError::WouldBlock) => continue,
                    };
                    if let Err(error) = plugin.process(left, right, frames, mix) {
                        drop(plugin);
                        slot.error = Some(format!("Processing failed: {error}"));
                        failed = true;
                    }
                }
            }
        }
        failed
    }
}

impl Drop for PluginChain {
    fn drop(&mut self) {
        // A bridge that outlived `replace_session` drops the old session on
        // the bridge thread.  Plugins have to be destroyed on the UI thread,
        // so the slots wait for [`super::drain_deferred_plugins`].
        if self.slots.is_empty() || super::super::clap::may_destroy_plugins_here() {
            return;
        }
        let slots = std::mem::take(&mut self.slots);
        defer_plugin_slots(slots);
    }
}
