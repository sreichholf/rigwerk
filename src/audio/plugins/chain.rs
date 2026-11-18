//! One signal path's effect chain on the audio side: the [`ChainRuntime`]
//! the audio thread owns and runs, the [`ChainCommand`]s that change it,
//! and the [`ChainHome`] it waits in while no audio thread holds it.  The
//! UI side is `model` ([`super::ChainModel`]).
//!
//! The UI never touches the runtime while an audio thread holds it.  Every
//! structural edit (insert, remove, move, replace, a new rate) reaches it
//! as a command through a queue, with any new slot already built, loaded
//! and activated on the UI thread.  The runtime applies the commands at the
//! start of a block and hands every slot it lets go of back through the
//! return queue, for the UI thread to destroy.  So the audio thread never
//! waits, never allocates, and never loads or destroys a plugin for an edit.
//!
//! The mixer takes the runtime out of its path's home with its first block
//! of the path, as a [`ChainLease`], and parks it again when the path stops
//! being live or the mixer goes away (the lease's `Drop`).  While it is
//! parked, the UI takes it and applies its commands itself.  A block that
//! finds the home empty because the UI holds it for that moment, or another
//! mixer still does, plays the chain's input delayed by its latency
//! (`mixer::PathProcessor::run_effects`).

use std::mem::ManuallyDrop;
use std::ops::{Deref, DerefMut};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::Arc;

use super::super::delay::{DECLICK_SECONDS, TAP_FADE_SECONDS};
#[cfg(test)]
use super::handle::LoadedPlugin;
use super::handle::Outcome;
use super::load::MAX_BLOCK;
#[cfg(test)]
use super::pref::PluginFormat;
#[cfg(test)]
use super::pref::PluginPref;
#[cfg(test)]
use super::pref::PluginView;
use super::queue::{Consumer, Producer};
#[cfg(test)]
use super::slot::SlotLink;
use super::slot::{defer_plugin_slots, Slot};
#[cfg(test)]
use super::MAX_ALIGN;

/// The most effects one path holds.  The runtime's slot list is allocated
/// for this many, so an effect never grows it on the audio thread.
pub(crate) const MAX_EFFECTS: usize = 32;

/// Room in the command queue.  A full queue is not an error: the model
/// keeps the rest in its backlog and sends it once the runtime caught up.
pub(super) const COMMAND_QUEUE: usize = 4 * MAX_EFFECTS;

/// Room in the return queue.  The runtime applies a command that lets go of
/// a slot only while the queue has room for it, so it never has to drop one.
pub(super) const RETURN_QUEUE: usize = 2 * MAX_EFFECTS;

/// One structural edit, as the UI sends it to the runtime.  Indices are
/// positions in the chain as it stands once every earlier command applied,
/// which is the model's own list.  A slot travels boxed, so moving it in or
/// out of the runtime never allocates.
pub(crate) enum ChainCommand {
    /// Puts `slot` at `index` (at the end if past it).
    Insert { index: usize, slot: Box<Slot> },
    /// Takes the slot at `index` out and returns it.
    Remove { index: usize },
    /// Swaps two slots.
    Swap { a: usize, b: usize },
    /// Puts `slot` at `index` and returns the slot it replaces.
    Replace { index: usize, slot: Box<Slot> },
    /// The rate the slots are now built for (fades and declicks).
    SetRate(u32),
}

/// Destroys a slot here if this thread may destroy plugins, else hands it
/// to the deferred queue the UI thread drains.  What a queue does with the
/// values left in it when both its ends are gone.
pub(super) fn release_slot(slot: Box<Slot>) {
    if super::super::clap::may_destroy_plugins_here() {
        drop(slot);
    } else {
        defer_plugin_slots(vec![*slot]);
    }
}

/// As [`release_slot`], for a command that never reached the runtime.
pub(super) fn release_command(command: ChainCommand) {
    match command {
        ChainCommand::Insert { slot, .. } | ChainCommand::Replace { slot, .. } => {
            release_slot(slot);
        }
        ChainCommand::Remove { .. } | ChainCommand::Swap { .. } | ChainCommand::SetRate(_) => {}
    }
}

/// One path's effect chain as the audio thread runs it.  It holds the
/// slots, each with its plugin's processor, and is touched by one thread at
/// a time: the audio thread through a [`ChainLease`], or the UI while it is
/// parked.
///
/// Latency: every effect reports the samples it delays the signal by (VST3
/// `getLatencySamples`, CLAP `latency.get`, read after each activation and
/// again when the plugin reports a change).  The chain total, bypassed
/// effects included (see [`Slot::latency_samples`]), is
/// [`Self::latency_samples`].  A path-level chain bypass skips every effect
/// the same way and still counts in that total.
pub(crate) struct ChainRuntime {
    /// Allocated for [`MAX_EFFECTS`]; never grows.  Boxed: a slot arrives
    /// and leaves through the queues as a `Box`, built and freed on the UI
    /// thread, so moving one in or out here never allocates.
    #[allow(clippy::vec_box)]
    slots: Vec<Box<Slot>>,
    sample_rate: u32,
    /// The total [`Self::publish_latency`] last saw.
    latency: u32,
    /// Skips every effect's processing.  The UI stores it; [`Self::process`]
    /// loads it.  It does not write each slot's own bypass.
    chain_bypass: Arc<AtomicBool>,
    commands: Consumer<ChainCommand>,
    returns: Producer<Box<Slot>>,
}

impl ChainRuntime {
    pub(super) fn new(
        sample_rate: u32,
        chain_bypass: Arc<AtomicBool>,
        commands: Consumer<ChainCommand>,
        returns: Producer<Box<Slot>>,
    ) -> Self {
        Self {
            slots: Vec::with_capacity(MAX_EFFECTS),
            sample_rate,
            latency: 0,
            chain_bypass,
            commands,
            returns,
        }
    }

    /// A runtime with no model, for tests that build and run a chain
    /// directly.  Nothing ever sends it a command.
    #[cfg(test)]
    pub(crate) fn for_test(sample_rate: u32) -> Self {
        let (_, commands) = super::queue::queue(1, release_command);
        let (returns, _) = super::queue::queue(1, release_slot);
        Self::new(
            sample_rate,
            Arc::new(AtomicBool::new(false)),
            commands,
            returns,
        )
    }

    /// Applies the commands the UI queued, in order, without allocating.
    /// It stops early, leaving the rest queued for the next call, when the
    /// return queue has no room for a slot one would hand back.  Returns
    /// whether it applied any.
    pub(crate) fn apply_commands(&mut self) -> bool {
        let mut applied = false;
        while let Some(command) = self.commands.peek() {
            if self.returns.room() == 0 && self.hands_back(command) {
                break;
            }
            let Some(command) = self.commands.pop() else {
                break;
            };
            self.apply(command);
            applied = true;
        }
        applied
    }

    /// Whether applying `command` hands a slot back.
    fn hands_back(&self, command: &ChainCommand) -> bool {
        match command {
            ChainCommand::Remove { .. } | ChainCommand::Replace { .. } => true,
            ChainCommand::Insert { .. } => self.slots.len() >= self.slots.capacity(),
            ChainCommand::Swap { .. } | ChainCommand::SetRate(_) => false,
        }
    }

    fn apply(&mut self, command: ChainCommand) {
        match command {
            ChainCommand::Insert { index, slot } => {
                if self.slots.len() < self.slots.capacity() {
                    self.slots.insert(index.min(self.slots.len()), slot);
                } else {
                    // The model never sends more than MAX_EFFECTS.
                    self.hand_back(slot);
                }
            }
            ChainCommand::Remove { index } => {
                if index < self.slots.len() {
                    let slot = self.slots.remove(index);
                    self.hand_back(slot);
                }
            }
            ChainCommand::Swap { a, b } => {
                if a < self.slots.len() && b < self.slots.len() {
                    self.slots.swap(a, b);
                }
            }
            ChainCommand::Replace { index, slot } => match self.slots.get_mut(index) {
                Some(current) => {
                    let old = std::mem::replace(current, slot);
                    self.hand_back(old);
                }
                None => self.hand_back(slot),
            },
            ChainCommand::SetRate(rate) => self.sample_rate = rate,
        }
    }

    /// Queues a slot for the UI to destroy.  [`Self::apply_commands`] made
    /// sure there is room; were there none, the slot is leaked rather than
    /// destroyed here.
    fn hand_back(&mut self, slot: Box<Slot>) {
        if let Err(slot) = self.returns.push(slot) {
            std::mem::forget(slot);
        }
    }

    /// The latency in samples the chain *should* have: the sum over its
    /// effects (see [`Slot::latency_samples`]).  A bypassed effect still
    /// counts (so bypass does not move the alignment); one that is not
    /// loaded or failed counts 0.  Atomics only.
    ///
    /// It is also the delay the chain really has, block by block: an effect
    /// skipped for a block (bypass, mix at 0, a restart's pause) plays its
    /// input delayed by its latency, and a partial mix delays its dry side
    /// by it (see [`Self::process`]).
    pub(crate) fn latency_samples(&self) -> u32 {
        self.slots
            .iter()
            .map(|slot| slot.latency_samples())
            .fold(0u32, u32::saturating_add)
    }

    /// Sums the latency again and returns whether it moved since the last
    /// call.  A plugin that changes its latency on the main thread shows up
    /// here by the next block.
    pub(crate) fn publish_latency(&mut self) -> bool {
        let latency = self.latency_samples();
        let changed = latency != self.latency;
        self.latency = latency;
        changed
    }

    /// The total as [`Self::publish_latency`] last summed it.
    pub(crate) fn latency(&self) -> u32 {
        self.latency
    }

    pub(crate) fn active(&self) -> usize {
        self.slots.iter().filter(|slot| slot.active()).count()
    }

    /// Whether a restored slot is still waiting for its first open.
    pub(crate) fn has_unopened(&self) -> bool {
        self.slots.iter().any(|slot| slot.needs_open())
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.slots.len()
    }

    #[cfg(test)]
    pub(crate) fn rate(&self) -> u32 {
        self.sample_rate
    }

    #[cfg(test)]
    pub(crate) fn set_chain_bypass(&self, bypass: bool) {
        self.chain_bypass.store(bypass, Ordering::Relaxed);
    }

    /// Adds an already loaded CLAP plugin as a slot, the way a real load
    /// fills one in.
    #[cfg(test)]
    pub(crate) fn insert_clap_for_test(&mut self, plugin: LoadedPlugin) {
        self.push_for_test(clap_slot_for_test(plugin, self.sample_rate));
    }

    /// Adds a test effect that is a pure delay of `latency` frames and
    /// reports that latency (see [`Slot::delay_for_test`]).
    #[cfg(test)]
    pub(crate) fn insert_delay_for_test(&mut self, latency: u32) {
        self.push_for_test(Slot::delay_for_test(latency));
    }

    #[cfg(test)]
    pub(crate) fn push_for_test(&mut self, slot: Slot) {
        self.slots.push(Box::new(slot));
        self.publish_latency();
    }

    /// Mutable access to one slot, for tests that put it in a state.
    #[cfg(test)]
    pub(crate) fn slot_mut_for_test(&mut self, index: usize) -> &mut Slot {
        &mut self.slots[index]
    }

    #[cfg(test)]
    pub(crate) fn set_bypass(&mut self, index: usize, bypass: bool) {
        self.slots[index].controls.set_bypass(bypass);
    }

    #[cfg(test)]
    pub(crate) fn set_mix(&mut self, index: usize, mix: f32) {
        self.slots[index].controls.set_mix(mix);
    }

    #[cfg(test)]
    pub(crate) fn views(&self) -> Vec<PluginView> {
        self.slots.iter().map(|slot| slot.view()).collect()
    }

    #[cfg(test)]
    pub(crate) fn slot_links(&self) -> Vec<SlotLink> {
        self.slots.iter().map(|slot| slot.link()).collect()
    }

    /// Takes every slot out, so a test destroys them on its own thread
    /// rather than through the deferred queue.
    #[cfg(test)]
    pub(crate) fn clear_for_test(&mut self) -> Vec<Slot> {
        let slots = self.slots.drain(..).map(|slot| *slot).collect();
        self.publish_latency();
        slots
    }

    /// Runs the enabled effects over one stereo block, in chunks of at most
    /// [`MAX_BLOCK`] frames.  A paused plugin (a restart) is skipped for
    /// that chunk, and a plugin that fails is left in the chain with its
    /// error so the person can see why it is silent.  Every skipped effect
    /// still delays by its latency, and an effect's dry/wet mix blends with
    /// its input delayed by that latency, so the block comes out
    /// [`Self::latency_samples`] late, with the dry and wet sides of every
    /// effect lined up.  The path's chain bypass skips every effect for the
    /// block the same way, without writing each slot's bypass.
    ///
    /// Returns whether a slot gained an error in this block, or the chain
    /// latency changed, so the caller can ask the UI thread to republish
    /// the effect views (the audio thread does not build them itself).
    pub(crate) fn process(&mut self, left: &mut [f32], right: &mut [f32], frames: usize) -> bool {
        // One relaxed load: an atomic, so this does not lock or allocate.
        let chain_bypass = self.chain_bypass.load(Ordering::Relaxed);
        let failed = self.process_effects(left, right, frames, chain_bypass);
        let latency_changed = self.publish_latency();
        failed || latency_changed
    }

    fn process_effects(
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
        // Chunks of at most MAX_BLOCK, so each effect's dry line only needs
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

impl Drop for ChainRuntime {
    /// The last owner may be a bridge thread that outlived its run:
    /// plugins are destroyed on the UI thread, so the slots go to the
    /// deferred queue there (see [`release_slot`]).  Queued commands and
    /// returned slots go the same way when their queue drops.
    fn drop(&mut self) {
        for slot in self.slots.drain(..) {
            release_slot(slot);
        }
    }
}

/// A CLAP test slot built the way a real load fills one in.
#[cfg(test)]
pub(super) fn clap_slot_for_test(plugin: LoadedPlugin, rate: u32) -> Slot {
    Slot::with_plugin_for_test(
        PluginPref {
            name: "Fake".to_owned(),
            format: PluginFormat::Clap,
            ..PluginPref::default()
        },
        plugin,
        rate,
    )
}

// `ChainHome` moves the runtime between threads through an `AtomicPtr`,
// which is `Send` and `Sync` whatever it points to: this keeps the
// runtime itself `Send`.
const _: fn() = || {
    fn send<T: Send>() {}
    send::<ChainRuntime>();
};

/// Where one path's [`ChainRuntime`] waits while no audio thread holds it:
/// one pointer, swapped in and out, so taking and parking it is lock and
/// allocation free.  There is exactly one runtime per home, either parked
/// here or in a [`ChainLease`].
pub(crate) struct ChainHome {
    parked: AtomicPtr<ChainRuntime>,
}

impl ChainHome {
    pub(super) fn new(runtime: ChainRuntime) -> Arc<Self> {
        Arc::new(Self {
            parked: AtomicPtr::new(Box::into_raw(Box::new(runtime))),
        })
    }

    /// Takes the runtime, or `None` while someone else holds it.  Lock and
    /// allocation free, so the audio thread may call it.
    pub(crate) fn take(self: &Arc<Self>) -> Option<ChainLease> {
        let runtime = self.parked.swap(ptr::null_mut(), Ordering::AcqRel);
        if runtime.is_null() {
            return None;
        }
        Some(ChainLease {
            // Parked by `new` or `park`, from a `Box`, and swapped out
            // above: this is its only owner.
            runtime: ManuallyDrop::new(unsafe { Box::from_raw(runtime) }),
            home: Arc::clone(self),
        })
    }

    fn park(&self, runtime: Box<ChainRuntime>) {
        let previous = self.parked.swap(Box::into_raw(runtime), Ordering::AcqRel);
        // One runtime per home, so the home was empty.  Were it not, the
        // other one is leaked rather than destroyed here, which may be the
        // audio thread.
        debug_assert!(previous.is_null(), "two runtimes for one home");
    }
}

impl Drop for ChainHome {
    fn drop(&mut self) {
        let runtime = std::mem::replace(self.parked.get_mut(), ptr::null_mut());
        if !runtime.is_null() {
            // As in `take`: parked from a `Box`, and this is the last owner.
            drop(unsafe { Box::from_raw(runtime) });
        }
    }
}

/// The runtime while one thread holds it.  Dropping the lease parks the
/// runtime in its home again (lock and allocation free).
pub(crate) struct ChainLease {
    runtime: ManuallyDrop<Box<ChainRuntime>>,
    home: Arc<ChainHome>,
}

impl ChainLease {
    /// Whether this lease was taken from `home`.
    pub(crate) fn is_from(&self, home: &Arc<ChainHome>) -> bool {
        Arc::ptr_eq(&self.home, home)
    }
}

impl Deref for ChainLease {
    type Target = ChainRuntime;

    fn deref(&self) -> &ChainRuntime {
        &self.runtime
    }
}

impl DerefMut for ChainLease {
    fn deref_mut(&mut self) -> &mut ChainRuntime {
        &mut self.runtime
    }
}

impl Drop for ChainLease {
    fn drop(&mut self) {
        // Taken once, here; the field is not touched again.
        let runtime = unsafe { ManuallyDrop::take(&mut self.runtime) };
        self.home.park(runtime);
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

/// Runs one effect over one chunk (at most [`MAX_BLOCK`] frames) in place.
/// The input always goes into the effect's dry line first.  A processed
/// chunk with a partial mix is blended with that dry side delayed by the
/// effect's latency; a skipped chunk (the path's chain bypass, the slot's
/// own bypass, mix at 0, a restart's pause) plays the input delayed by that
/// latency.  So the effect delays by its reported latency whatever happens
/// to it.  A switch between the plugin running and being skipped (bypass
/// toggled, mix through 0, a restart) cannot crossfade, since only one of
/// the two exists for the chunk; its step is smoothed over
/// `DECLICK_SECONDS` instead.  Returns whether the effect failed.
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
    // alone, so clearing it restores whatever each effect was set to.
    let wet = !chain_bypass && slot.active() && mix > 0.0;
    let outcome = run_plugin(slot, left, right, wet);
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

/// Runs the effect's plugin over one chunk, fully wet when `wet`.  A loaded
/// plugin is brought here every chunk, so its pause handshake is served even
/// while it is skipped (see [`LoadedPlugin::run`](super::handle::LoadedPlugin::run)).
/// A paused plugin is skipped for this chunk; a plugin that fails keeps its
/// failure so the person can see why it is silent.
fn run_plugin(slot: &mut Slot, left: &mut [f32], right: &mut [f32], wet: bool) -> Outcome {
    #[cfg(test)]
    if let Some(test) = slot.test_delay.as_mut() {
        if !wet {
            return Outcome::Skipped;
        }
        slot.controls.note_attempt();
        let count = left.len().min(right.len());
        let latency = test.latency.load(Ordering::Relaxed).min(MAX_ALIGN) as usize;
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

#[cfg(test)]
mod tests;
