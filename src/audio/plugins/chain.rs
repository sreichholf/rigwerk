//! One signal path's insert chain.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use super::super::delay::{DECLICK_SECONDS, TAP_FADE_SECONDS};
#[cfg(test)]
use super::handle::LoadedPlugin;
use super::handle::Outcome;
use super::load::MAX_BLOCK;
#[cfg(test)]
use super::pref::PluginFormat;
use super::pref::PluginPref;
#[cfg(test)]
use super::pref::PluginView;
use super::slot::{defer_plugin_slots, Slot, SlotLink};
#[cfg(test)]
use super::MAX_ALIGN;

/// Slots [`PluginChain::reload_plan`] wants opened at a new or first rate.
/// The caller builds them without the chain lock.
pub(crate) struct ReloadPlan {
    pub(crate) previous_rate: u32,
    pub(crate) jobs: Vec<(usize, PluginPref)>,
}

/// One path's insert chain.  The audio callback only try-locks the chain and
/// each loaded plugin, so an edit or an open editor skips that processing for
/// a block instead of stalling the callback; UI paths may still block on the
/// chain lock.  A skip never changes the timing: a skipped insert plays its
/// input delayed by its latency, and a missed chain lock is covered the same
/// way by the mixer (see the `path::plugins` module docs, "Chain handover").
///
/// Latency: every insert reports the samples it delays the signal by (VST3
/// `getLatencySamples`, CLAP `latency.get`, read after each activation and
/// again when the plugin reports a change).  The chain total, bypassed
/// inserts included (see [`Slot::latency_samples`]), is
/// [`Self::latency_samples`], and is also kept in an atomic
/// ([`Self::latency_handle`]) that readers outside the chain lock can load.
/// A path-level chain bypass ([`Self::set_chain_bypass`]) skips every
/// insert the same way and still counts in that total.
pub(crate) struct PluginChain {
    slots: Vec<Slot>,
    sample_rate: u32,
    /// [`Self::latency_samples`] as last published.
    latency: Arc<AtomicU32>,
    /// Skips every insert's processing.  The audio thread loads it; the UI
    /// stores it without the chain lock.  It does not write each slot's
    /// own bypass.
    chain_bypass: Arc<AtomicBool>,
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
            sample_rate: rate,
            latency: Arc::new(AtomicU32::new(0)),
            chain_bypass: Arc::new(AtomicBool::new(false)),
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

    /// The path-level bypass flag, shared so the UI can set it without the
    /// chain lock.  [`Self::process`] loads it on the audio thread.
    pub(crate) fn chain_bypass_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.chain_bypass)
    }

    /// Skips every insert's processing until cleared.  Each slot's own
    /// bypass is left as it is, so clearing this restores those states.
    /// Latency stays counted.  The running path writes the same atomic;
    /// this setter is for the chain's own tests.
    #[cfg(test)]
    pub(crate) fn set_chain_bypass(&self, bypass: bool) {
        self.chain_bypass.store(bypass, Ordering::Relaxed);
    }

    /// Publishes the latency total after a test changed an insert's latency
    /// in place.  The running path republishes from its links instead
    /// (`PathControls::service_plugins`).
    #[cfg(test)]
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
            .map(|(index, slot)| (index, slot.restore_pref()))
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
                    .is_some_and(|slot| slot.restores_with(pref))
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
        self.sample_rate = rate;
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
    pub(crate) fn insert_clap_for_test(&mut self, plugin: LoadedPlugin) {
        let slot = Slot::with_plugin_for_test(
            PluginPref {
                name: "Fake".to_owned(),
                format: PluginFormat::Clap,
                ..PluginPref::default()
            },
            plugin,
            self.sample_rate,
        );
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

    #[cfg(test)]
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

    /// What the UI needs to reach each insert without the chain lock (see
    /// [`SlotLink`]).  Published with the views.
    pub(crate) fn slot_links(&self) -> Vec<SlotLink> {
        self.slots.iter().map(Slot::link).collect()
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

    /// Sets one normalized plugin control, opening a never-opened slot
    /// first.  A loaded slot is reached through its link instead (see
    /// `PathControls::set_plugin_parameter`), without the chain lock.
    pub(crate) fn set_parameter(
        &mut self,
        index: usize,
        id: u32,
        value: f64,
    ) -> Result<(), String> {
        self.open_handle(index)?
            .set_parameter(id, value)
            .map(|_| ())
    }

    /// Returns every control to the plugin's own defaults and forgets the
    /// saved overrides, opening a never-opened slot first.
    pub(crate) fn reset_parameters(&mut self, index: usize) -> Result<(), String> {
        self.open_handle(index)?.reset_parameters().map(|_| ())
    }

    /// The handle of slot `index`, opened if it never was.
    fn open_handle(&mut self, index: usize) -> Result<super::PluginHandle, String> {
        let rate = self.sample_rate;
        let Some(slot) = self.slots.get_mut(index) else {
            return Err("That insert is no longer in the chain".to_owned());
        };
        if slot.load_if_needed(rate) {
            self.publish_latency();
        }
        let slot = &self.slots[index];
        match &slot.plugin {
            Some(plugin) => Ok(plugin.handle().clone()),
            None => Err(slot
                .error
                .clone()
                .unwrap_or_else(|| "The plugin is not loaded".to_owned())),
        }
    }

    /// Runs the enabled effects over one stereo block, in chunks of at most
    /// [`MAX_BLOCK`] frames.  A plugin whose lock is held (its editor is
    /// doing UI work) is skipped for that chunk, and a plugin that fails is
    /// left in the chain with its error so the person can see why it is
    /// silent.  Every skipped insert still delays by its latency, and an
    /// insert's dry/wet mix blends with its input delayed by that latency,
    /// so the block comes out [`Self::latency_samples`] late, with the dry
    /// and wet sides of every insert lined up.  The path's chain bypass
    /// skips every insert for the block the same way, without writing each
    /// slot's bypass.
    ///
    /// Returns whether a slot gained an error in this block, or the chain
    /// latency changed, so the caller can ask the UI thread to republish
    /// the insert views (the audio thread does not build them itself).
    ///
    /// Also refreshes [`Self::latency_handle`], so a latency change made on
    /// the main thread (outside the chain lock) reaches it by the next block.
    pub(crate) fn process(&mut self, left: &mut [f32], right: &mut [f32], frames: usize) -> bool {
        // One relaxed load.  The flag is an atomic, so this does not lock
        // or allocate; the callback already holds the chain only by try_lock.
        let chain_bypass = self.chain_bypass.load(Ordering::Relaxed);
        let failed = self.process_inserts(left, right, frames, chain_bypass);
        let latency_changed = self.publish_latency();
        failed || latency_changed
    }

    fn process_inserts(
        &mut self,
        left: &mut [f32],
        right: &mut [f32],
        frames: usize,
        chain_bypass: bool,
    ) -> bool {
        let frames = frames.min(left.len()).min(right.len());
        if frames == 0 {
            return false;
        }
        let timing = SlotTiming {
            fade: TAP_FADE_SECONDS * self.sample_rate as f32,
            declick: (DECLICK_SECONDS * self.sample_rate as f32) as u32,
        };
        let mut failed = false;
        let Self { slots, .. } = self;
        // Chunks of at most MAX_BLOCK, so each insert's dry line only needs
        // its latency plus one chunk.
        let mut offset = 0;
        while offset < frames {
            let count = (frames - offset).min(MAX_BLOCK);
            let left = &mut left[offset..offset + count];
            let right = &mut right[offset..offset + count];
            for slot in slots.iter_mut() {
                failed |= process_slot(slot, left, right, timing, chain_bypass);
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

/// Runs one insert over one chunk (at most [`MAX_BLOCK`] frames) in place.
/// The input always goes into the insert's dry line first.  A processed
/// chunk with a partial mix is blended with that dry side delayed by the
/// insert's latency; a skipped chunk (the path's chain bypass, the slot's
/// own bypass, mix at 0, the plugin's lock held by its editor, a CLAP
/// restart's passthrough) plays the input delayed by that latency.  So the
/// insert delays by its reported latency
/// whatever happens to it.  A switch between the plugin running and being
/// skipped (bypass toggled, mix through 0, a plugin-lock skip, a CLAP
/// restart) cannot crossfade, since only one of the two exists for the
/// chunk; its step is smoothed over `DECLICK_SECONDS` instead.  Returns
/// whether the insert failed.
fn process_slot(
    slot: &mut Slot,
    left: &mut [f32],
    right: &mut [f32],
    timing: SlotTiming,
    chain_bypass: bool,
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
    // Chain bypass skips the plugin and leaves the slot's own bypass flag
    // alone, so clearing it restores whatever each insert was set to.
    let wet = !chain_bypass && slot.active() && mix > 0.0;
    let outcome = run_plugin(slot, left, right, wet);
    let failed = matches!(outcome, Outcome::Failed);
    let processed = matches!(outcome, Outcome::Processed);
    if processed {
        // The editor sync needs to know a block ran: only then can a VST3
        // plugin's reported changes have become visible.
        slot.controls.note_processed();
    }
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

/// Runs the insert's plugin over one chunk, fully wet when `wet`.  A loaded
/// plugin is brought here every chunk, so its pause handshake is served even
/// while it is skipped (see [`LoadedPlugin::run`](super::handle::LoadedPlugin::run)).
/// A plugin whose lock is held (its editor is doing UI work) is skipped for
/// this chunk rather than blocking the callback; a plugin that fails keeps
/// its failure so the person can see why it is silent.
fn run_plugin(slot: &mut Slot, left: &mut [f32], right: &mut [f32], wet: bool) -> Outcome {
    #[cfg(test)]
    if let Some(test) = slot.test_delay.as_mut() {
        if !wet {
            return Outcome::Skipped;
        }
        slot.controls.note_attempt();
        let count = left.len().min(right.len());
        let latency = test.latency.min(MAX_ALIGN) as usize;
        for index in 0..count {
            test.line.push([left[index], right[index]]);
            [left[index], right[index]] = test.line.read(latency);
        }
        return Outcome::Processed;
    }
    match slot.plugin.as_mut() {
        Some(plugin) => plugin.run(&slot.controls, left, right, wet),
        None => Outcome::Skipped,
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
