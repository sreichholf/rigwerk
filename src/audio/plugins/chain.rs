//! The master insert chain.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, TryLockError};

use vst3_host::AudioBuffers;

use super::super::clap::{ClapPlugin, HostState};
use super::super::delay::{DECLICK_SECONDS, TAP_FADE_SECONDS};
use super::load::{
    host_format_parameter, host_reset_parameters, host_set_parameter, merge_synced_params, Capture,
    PluginHandle, StateRequest, Vst3Service, MAX_BLOCK,
};
#[cfg(test)]
use super::pref::PluginFormat;
use super::pref::{PluginParamPref, PluginParamView, PluginPref, PluginView};
use super::slot::{defer_plugin_slots, Slot, SlotControls};
#[cfg(test)]
use super::MAX_ALIGN;

/// Slots [`PluginChain::reload_plan`] wants opened at a new or first rate.
/// The caller builds them without the chain lock.
/// One insert as the UI reaches it without the chain lock (see
/// [`PluginChain::slot_links`]).
#[derive(Clone)]
pub(crate) struct SlotLink {
    pub(crate) controls: Arc<SlotControls>,
    pub(crate) plugin: Option<PluginHandle>,
    pub(crate) vst3: Option<Arc<Vst3Service>>,
}

pub(crate) struct ReloadPlan {
    pub(crate) previous_rate: u32,
    pub(crate) jobs: Vec<(usize, PluginPref)>,
}

/// One path's insert chain.  The audio callback only try-locks the chain and
/// each loaded plugin, so an edit or an open editor skips that processing for
/// a block instead of stalling the callback; UI paths may still block on the
/// chain lock.  A skip never changes the timing: a skipped insert plays its
/// input delayed by its latency, and a missed chain lock is covered the same
/// way by the mixer (see the `path` module docs, "Chain handover").
///
/// Latency: every insert reports the samples it delays the signal by (VST3
/// `getLatencySamples`, CLAP `latency.get`, read after each activation and
/// again when the plugin reports a change).  The chain total, bypassed
/// inserts included (see [`Slot::latency_samples`]), is
/// [`Self::latency_samples`], and is also kept in an atomic
/// ([`Self::latency_handle`]) that readers outside the chain lock can load.
pub(crate) struct PluginChain {
    slots: Vec<Slot>,
    buffers: AudioBuffers,
    sample_rate: u32,
    /// [`Self::latency_samples`] as last published.
    latency: Arc<AtomicU32>,
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
            sample_rate: rate,
            latency: Arc::new(AtomicU32::new(0)),
        }
    }

    /// The latency in samples the chain *should* have: the sum over its
    /// inserts (see [`Slot::latency_samples`]).  A bypassed insert still
    /// counts (so bypass does not move the alignment); one that is not
    /// loaded or failed counts 0.  Atomics only, so the audio thread may
    /// call it under the chain lock.
    ///
    /// It is also the delay the chain really has, block by block: an insert
    /// skipped for a block (bypass, mix at 0, a missed try-lock, a CLAP
    /// restart's passthrough) plays its input delayed by its latency, and a
    /// partial mix delays its dry side by it (see [`Self::process`]).
    pub(crate) fn latency_samples(&self) -> u32 {
        self.slots
            .iter()
            .map(Slot::latency_samples)
            .fold(0u32, u32::saturating_add)
    }

    /// The chain total as an atomic, for readers that do not hold the chain
    /// lock (the audio thread's alignment, for one).  It is refreshed on
    /// every edit and every processed block, so a plugin that changes its
    /// latency on the main thread shows up by the next block.
    pub(crate) fn latency_handle(&self) -> Arc<AtomicU32> {
        Arc::clone(&self.latency)
    }

    /// Publishes the latency total after a change made outside the chain
    /// lock (a plugin restarted on the main thread), without waiting for
    /// the next processed block.
    pub(crate) fn refresh_latency(&mut self) {
        self.publish_latency();
    }

    /// Stores [`Self::latency_samples`] in the shared atomic and returns
    /// whether that changed it.  Allocation and lock free.
    fn publish_latency(&self) -> bool {
        let total = self.latency_samples();
        self.latency.swap(total, Ordering::AcqRel) != total
    }

    /// Replaces the slots and hands the old ones back, so the caller can
    /// drop (and destroy) them after releasing the chain lock.
    pub(crate) fn restore(&mut self, prefs: Vec<PluginPref>) -> Vec<Slot> {
        let old = std::mem::replace(
            &mut self.slots,
            prefs.into_iter().map(Slot::empty).collect(),
        );
        self.publish_latency();
        old
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
        self.publish_latency();
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
        for (index, _, mut slot) in built {
            // The UI may have moved mix or bypass meanwhile; it keeps
            // writing to the same controls.
            slot.controls = Arc::clone(&self.slots[index].controls);
            old.push(std::mem::replace(&mut self.slots[index], slot));
        }
        self.apply_rate(rate);
        self.publish_latency();
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
        slot.dry = Slot::dry_line();
        self.slots.push(slot);
        self.publish_latency();
    }

    /// Adds a test insert that is a pure delay of `latency` frames and
    /// reports that latency (see [`Slot::delay_for_test`]).
    #[cfg(test)]
    pub(crate) fn insert_delay_for_test(&mut self, latency: u32) {
        self.slots.push(Slot::delay_for_test(latency));
        self.publish_latency();
    }

    /// Mutable access to one slot, for tests that put it in a state.
    #[cfg(test)]
    pub(crate) fn slot_mut_for_test(&mut self, index: usize) -> &mut Slot {
        &mut self.slots[index]
    }

    pub(crate) fn rate(&self) -> u32 {
        self.sample_rate
    }

    pub(crate) fn views(&self) -> Vec<PluginView> {
        self.slots.iter().map(Slot::view).collect()
    }

    pub(crate) fn prefs(&self) -> Vec<PluginPref> {
        self.slots.iter().map(Slot::saved_pref).collect()
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
    /// callback replaces the chain by a plain delay whenever it cannot get the lock.
    pub(crate) fn remove(&mut self, index: usize) -> Option<Slot> {
        let removed = (index < self.slots.len()).then(|| self.slots.remove(index));
        self.publish_latency();
        removed
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

    /// Sets one insert's bypass.  The UI goes through
    /// [`Self::slot_links`] instead, without the chain lock.
    #[cfg(test)]
    pub(crate) fn set_bypass(&mut self, index: usize, bypass: bool) {
        if let Some(slot) = self.slots.get(index) {
            slot.controls.set_bypass(bypass);
        }
    }

    /// Sets one insert's mix (see [`Self::set_bypass`]).
    #[cfg(test)]
    pub(crate) fn set_mix(&mut self, index: usize, mix: f32) {
        if let Some(slot) = self.slots.get(index) {
            slot.controls.set_mix(mix);
        }
    }

    /// What the UI needs to reach each insert without the chain lock: its
    /// mix and bypass, its plugin and its VST3 service.  Published with
    /// the views.
    pub(crate) fn slot_links(&self) -> Vec<SlotLink> {
        self.slots
            .iter()
            .map(|slot| SlotLink {
                controls: Arc::clone(&slot.controls),
                plugin: slot.plugin.clone(),
                vst3: slot.vst3.clone(),
            })
            .collect()
    }

    /// Stores a control value the UI already set on the plugin outside the
    /// chain lock (see `PathControls::set_plugin_parameter`), provided the
    /// slot still holds the same plugin instance.
    pub(crate) fn store_parameter(
        &mut self,
        index: usize,
        handle: &PluginHandle,
        id: u32,
        value: f64,
        display: Option<String>,
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
    }

    /// Opens a saved slot so its controls can be shown before playback starts.
    pub(crate) fn ensure_loaded(&mut self, index: usize) {
        let rate = self.sample_rate;
        if let Some(slot) = self.slots.get_mut(index) {
            slot.ensure_loaded(rate);
            slot.nudge_vst3();
        }
        self.publish_latency();
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

    /// The VST3 plugins that need main-thread servicing (restart requests
    /// and latency changes).  The caller runs [`Vst3Service::service`] on
    /// the UI thread after releasing the chain lock.
    pub(crate) fn vst3_services(&self) -> Vec<Arc<Vst3Service>> {
        self.slots
            .iter()
            .filter_map(|slot| slot.vst3.clone())
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
        slot.nudge_vst3();
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
                slot.nudge_vst3();
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
        if slot.load_if_needed(rate) {
            self.publish_latency();
        }
        let Some(slot) = self.slots.get_mut(index) else {
            return Err("That insert is no longer in the chain".to_owned());
        };
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
        let display = host_format_parameter(plugin, id, value);
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
        slot.nudge_vst3();
        Ok(())
    }

    /// Returns every control to the plugin's own defaults and forgets the
    /// saved overrides.
    pub(crate) fn reset_parameters(&mut self, index: usize) -> Result<(), String> {
        let rate = self.sample_rate;
        let Some(slot) = self.slots.get_mut(index) else {
            return Err("That insert is no longer in the chain".to_owned());
        };
        if slot.load_if_needed(rate) {
            self.publish_latency();
        }
        let Some(slot) = self.slots.get_mut(index) else {
            return Err("That insert is no longer in the chain".to_owned());
        };
        let Some(plugin) = slot.plugin.as_ref() else {
            return Err(slot
                .error
                .clone()
                .unwrap_or_else(|| "The plugin is not loaded".to_owned()));
        };
        slot.parameters = host_reset_parameters(plugin)?;
        slot.pref.params.clear();
        slot.state_stale = true;
        slot.nudge_vst3();
        Ok(())
    }

    /// Runs the enabled effects over one stereo block, in chunks of at most
    /// [`MAX_BLOCK`] frames.  A plugin whose lock is held (its editor is
    /// doing UI work) is skipped for that chunk, and a plugin that fails is
    /// left in the chain with its error so the person can see why it is
    /// silent.  Every skipped insert still delays by its latency, and an
    /// insert's dry/wet mix blends with its input delayed by that latency,
    /// so the block comes out [`Self::latency_samples`] late, with the dry
    /// and wet sides of every insert lined up.
    ///
    /// Returns whether a slot gained an error in this block, or the chain
    /// latency changed, so the caller can ask the UI thread to republish
    /// the insert views (the audio thread does not build them itself).
    ///
    /// Also refreshes [`Self::latency_handle`], so a latency change made on
    /// the main thread (outside the chain lock) reaches it by the next block.
    pub(crate) fn process(&mut self, left: &mut [f32], right: &mut [f32], frames: usize) -> bool {
        let failed = self.process_inserts(left, right, frames);
        let latency_changed = self.publish_latency();
        failed || latency_changed
    }

    fn process_inserts(&mut self, left: &mut [f32], right: &mut [f32], frames: usize) -> bool {
        let frames = frames.min(left.len()).min(right.len());
        if frames == 0 {
            return false;
        }
        let timing = SlotTiming {
            fade: TAP_FADE_SECONDS * self.sample_rate as f32,
            declick: (DECLICK_SECONDS * self.sample_rate as f32) as u32,
        };
        let mut failed = false;
        let Self { slots, buffers, .. } = self;
        // Chunks of at most MAX_BLOCK, so each insert's dry line only needs
        // its latency plus one chunk.
        let mut offset = 0;
        while offset < frames {
            let count = (frames - offset).min(MAX_BLOCK);
            let left = &mut left[offset..offset + count];
            let right = &mut right[offset..offset + count];
            for slot in slots.iter_mut() {
                failed |= process_slot(slot, buffers, left, right, timing);
            }
            offset += count;
        }
        failed
    }
}

/// Per-chunk constants for [`process_slot`], in frames.
#[derive(Clone, Copy)]
struct SlotTiming {
    /// How long a change of the dry delay crossfades.
    fade: f32,
    /// How long a switch between plugin output and delayed input is
    /// smoothed.
    declick: u32,
}

/// What one insert did with one chunk.
enum Outcome {
    /// The plugin's output (fully wet) is in the buffers.
    Processed,
    /// The buffers still hold the insert's input.
    Skipped,
    /// The plugin failed; the buffers hold its input.
    Failed,
}

/// Runs one insert over one chunk (at most [`MAX_BLOCK`] frames) in place.
/// The input always goes into the insert's dry line first.  A processed
/// chunk with a partial mix is blended with that dry side delayed by the
/// insert's latency; a skipped chunk (bypass, mix at 0, the plugin's lock
/// held by its editor, a CLAP restart's passthrough) plays the input
/// delayed by that latency.  So the insert delays by its reported latency
/// whatever happens to it.  A switch between the plugin running and being
/// skipped (bypass toggled, mix through 0, a plugin-lock skip, a CLAP
/// restart) cannot crossfade, since only one of the two exists for the
/// chunk; its step is smoothed over `DECLICK_SECONDS` instead.  Returns
/// whether the insert failed.
fn process_slot(
    slot: &mut Slot,
    buffers: &mut AudioBuffers,
    left: &mut [f32],
    right: &mut [f32],
    timing: SlotTiming,
) -> bool {
    let count = left.len().min(right.len());
    if count == 0 {
        return false;
    }
    for (l, r) in left.iter().zip(right.iter()) {
        slot.dry.push([*l, *r]);
    }
    let latency = slot.latency_samples();
    let first = !slot.dry_started;
    if first {
        slot.dry_tap.snap(latency);
        slot.dry_started = true;
    } else {
        slot.dry_tap.set_target(latency, timing.fade);
    }
    let mix = slot.controls.mix();
    let outcome = if !slot.active() || mix <= 0.0 {
        Outcome::Skipped
    } else {
        run_plugin(slot, buffers, left, right)
    };
    let failed = matches!(outcome, Outcome::Failed);
    let processed = matches!(outcome, Outcome::Processed);
    match outcome {
        Outcome::Processed if mix < 1.0 => {
            let dry = 1.0 - mix;
            for index in 0..count {
                let delayed = slot.dry_tap.next(&slot.dry, count - 1 - index);
                left[index] = delayed[0] * dry + left[index] * mix;
                right[index] = delayed[1] * dry + right[index] * mix;
            }
        }
        Outcome::Processed => {
            // Nothing read the dry side: it follows the latency at once, so
            // a later bypass or mix change starts from the right delay.
            slot.dry_tap.snap(latency);
        }
        Outcome::Skipped | Outcome::Failed => {
            if latency == 0 && slot.dry_tap.is_settled() {
                // Nothing to delay: the input passes as it is.
                slot.dry_tap.snap(0);
            } else {
                for index in 0..count {
                    let delayed = slot.dry_tap.next(&slot.dry, count - 1 - index);
                    left[index] = delayed[0];
                    right[index] = delayed[1];
                }
            }
        }
    }
    // A switch between the plugin's output and the delayed input (bypass,
    // mix through 0, a skipped chunk, a CLAP restart) would step: the step
    // fades out over a few ms instead.
    if !first && processed != slot.was_processed {
        slot.declick.start([left[0], right[0]], timing.declick);
    }
    slot.was_processed = processed;
    if slot.declick.is_active() {
        for index in 0..count {
            [left[index], right[index]] = slot.declick.apply([left[index], right[index]]);
        }
    }
    slot.declick.note(left, right);
    failed
}

/// Runs the insert's plugin over one chunk, fully wet.  A plugin whose lock
/// is held (its editor is doing UI work) is skipped for this chunk rather
/// than blocking the callback; a plugin that fails keeps its error in the
/// slot so the person can see why it is silent.
fn run_plugin(
    slot: &mut Slot,
    buffers: &mut AudioBuffers,
    left: &mut [f32],
    right: &mut [f32],
) -> Outcome {
    let count = left.len().min(right.len());
    #[cfg(test)]
    if let Some(test) = slot.test_delay.as_mut() {
        let latency = test.latency.min(MAX_ALIGN) as usize;
        for index in 0..count {
            test.line.push([left[index], right[index]]);
            [left[index], right[index]] = test.line.read(latency);
        }
        return Outcome::Processed;
    }
    // The slot keeps its own reference while the audio thread holds the
    // chain lock, so dropping this clone is never the last release: a
    // plugin (and its CLAP module, whose release takes the module registry
    // lock and may `deinit`) is never torn down on the audio thread.
    let Some(handle) = slot.plugin.clone() else {
        return Outcome::Skipped;
    };
    match handle {
        PluginHandle::Vst3(plugin) => {
            // The plugin's editor holds this lock while it runs UI work.
            let mut plugin = match plugin.try_lock() {
                Ok(plugin) => plugin,
                Err(TryLockError::Poisoned(poison)) => poison.into_inner(),
                Err(TryLockError::WouldBlock) => return Outcome::Skipped,
            };
            buffers.block_size = count;
            // The VST3 host works out how many frames to run from the length
            // of these vectors, not from `block_size`.  They are allocated at
            // MAX_BLOCK, so without trimming them to `count` every short
            // callback would still advance the plugin by a full 512 frames —
            // the plugin would run too fast and time-based effects would
            // sound detuned.  The vectors keep their capacity, so this does
            // not allocate.
            for channel in &mut buffers.inputs {
                channel.resize(count, 0.0);
            }
            for channel in &mut buffers.outputs {
                channel.resize(count, 0.0);
            }
            buffers.inputs[0].copy_from_slice(&left[..count]);
            buffers.inputs[1].copy_from_slice(&right[..count]);
            buffers.outputs[0].fill(0.0);
            buffers.outputs[1].fill(0.0);
            match plugin.process_audio(buffers) {
                Ok(()) => {
                    left[..count].copy_from_slice(&buffers.outputs[0]);
                    right[..count].copy_from_slice(&buffers.outputs[1]);
                    Outcome::Processed
                }
                Err(error) => {
                    drop(plugin);
                    slot.error = Some(format!("Processing failed: {error}"));
                    Outcome::Failed
                }
            }
        }
        PluginHandle::Clap(plugin) => {
            let mut plugin = match plugin.try_lock() {
                Ok(plugin) => plugin,
                Err(TryLockError::Poisoned(poison)) => poison.into_inner(),
                Err(TryLockError::WouldBlock) => return Outcome::Skipped,
            };
            match plugin.process(left, right, count, 1.0) {
                Ok(true) => Outcome::Processed,
                Ok(false) => Outcome::Skipped,
                Err(error) => {
                    drop(plugin);
                    slot.error = Some(format!("Processing failed: {error}"));
                    Outcome::Failed
                }
            }
        }
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
