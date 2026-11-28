//! Audio side of one path's effect chain.  The UI never touches the
//! runtime while an audio thread holds it.
//!
//! A structural edit arrives as a command, with the new slot already
//! loaded and activated on the UI thread.  The runtime applies commands
//! at the start of a block and hands every slot it lets go of back through
//! the return queue.  The audio thread never waits, allocates, loads, or
//! destroys a plugin.
//!
//! While the runtime is parked, the UI applies commands itself.  A block
//! that finds the home empty plays the input delayed by the chain's
//! latency.

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
use super::slot::{Slot, SlotPark};
#[cfg(test)]
use super::MAX_ALIGN;

/// The runtime's slot list is allocated for this many, so an insert never
/// grows it on the audio thread.
pub(crate) const MAX_EFFECTS: usize = 32;

/// A full queue is not an error: the model keeps the rest and sends it
/// once the runtime has caught up.
pub(super) const COMMAND_QUEUE: usize = 4 * MAX_EFFECTS;

/// A command that lets go of a slot is applied only while this has room,
/// so the runtime never drops one.
pub(super) const RETURN_QUEUE: usize = 2 * MAX_EFFECTS;

/// Indices are positions once every earlier command has applied, which is
/// the model's list.  A slot travels boxed, so moving it never allocates
/// on the audio thread.
pub(crate) enum ChainCommand {
    Insert {
        index: usize,
        slot: Box<Slot>,
    },
    Remove {
        index: usize,
    },
    Swap {
        a: usize,
        b: usize,
    },
    Replace {
        index: usize,
        slot: Box<Slot>,
    },
    /// Fades and declicks use this rate.
    SetRate(u32),
}

/// Destroys the slot on a thread that may, otherwise parks it for the
/// main thread.  Every teardown of a slot that may have run goes through
/// here.  A debug build asserts this is not inside an audio callback.
///
/// A stranded slot is leaked on any thread: its plugin is still inside
/// `process`, so it is never deferred and never destroyed.
pub(crate) fn release_slot(slot: Box<Slot>, park: &SlotPark) {
    debug_assert!(
        !super::super::rt_guard::in_audio_callback(),
        "a plugin slot was released inside an audio callback: it would destroy a plugin or take the deferred-slot lock"
    );
    if slot.stranded() {
        // Still inside `process`: leak it here.  A drain would destroy it.
        slot.destroy();
        return;
    }
    if super::may_destroy_plugins_here() {
        drop(slot);
    } else {
        park.defer(vec![*slot]);
    }
}

/// A command that never reached the runtime.  Same rule as [`release_slot`].
pub(super) fn release_command(command: ChainCommand, park: &SlotPark) {
    match command {
        ChainCommand::Insert { slot, .. } | ChainCommand::Replace { slot, .. } => {
            release_slot(slot, park);
        }
        ChainCommand::Remove { .. } | ChainCommand::Swap { .. } | ChainCommand::SetRate(_) => {}
    }
}

/// Touched by one thread at a time: the audio thread through a
/// [`ChainLease`], or the UI while parked.
///
/// The chain total includes bypassed effects (see [`Slot::latency_samples`]).
/// A path-level bypass skips every effect and still counts in that total.
pub(crate) struct ChainRuntime {
    /// Allocated for [`MAX_EFFECTS`]; never grows.  Slots arrive and leave
    /// as a `Box`, so moving one here never allocates.
    #[allow(clippy::vec_box)]
    slots: Vec<Box<Slot>>,
    sample_rate: u32,
    latency: u32,
    /// The UI stores it; [`Self::process`] loads it.  It does not write
    /// each slot's own bypass.
    chain_bypass: Arc<AtomicBool>,
    commands: Consumer<ChainCommand>,
    returns: Producer<Box<Slot>>,
    /// The engine owns it, so a drop off the UI thread still has a park.
    slot_park: Arc<SlotPark>,
}

impl ChainRuntime {
    pub(super) fn new(
        sample_rate: u32,
        chain_bypass: Arc<AtomicBool>,
        commands: Consumer<ChainCommand>,
        returns: Producer<Box<Slot>>,
        slot_park: Arc<SlotPark>,
    ) -> Self {
        Self {
            slots: Vec::with_capacity(MAX_EFFECTS),
            sample_rate,
            latency: 0,
            chain_bypass,
            commands,
            returns,
            slot_park,
        }
    }

    /// No model.  Nothing sends this a command.
    #[cfg(test)]
    pub(crate) fn for_test(sample_rate: u32) -> Self {
        let slot_park = SlotPark::new();
        let command_park = Arc::clone(&slot_park);
        let return_park = Arc::clone(&slot_park);
        let (_, commands) =
            super::queue::queue(1, move |command| release_command(command, &command_park));
        let (returns, _) = super::queue::queue(1, move |slot| release_slot(slot, &return_park));
        Self::new(
            sample_rate,
            Arc::new(AtomicBool::new(false)),
            commands,
            returns,
            slot_park,
        )
    }

    /// Stops early, leaving the rest queued, when the return queue has no
    /// room for a slot one would hand back.  Does not allocate.
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
                    // The model never sends more than `MAX_EFFECTS`.
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

    /// [`Self::apply_commands`] checked there is room.  Were there none,
    /// the slot is leaked rather than destroyed here.
    fn hand_back(&mut self, slot: Box<Slot>) {
        if let Err(slot) = self.returns.push(slot) {
            std::mem::forget(slot);
        }
    }

    /// Sum of [`Slot::latency_samples`].  Atomics only.  A bypassed effect
    /// still counts, so bypass does not move the alignment; an unloaded or
    /// failed one counts 0.
    ///
    /// This is also the delay the chain really has: a skipped effect and a
    /// partial mix delay by that latency (see [`Self::process`]).
    pub(crate) fn latency_samples(&self) -> u32 {
        self.slots
            .iter()
            .map(|slot| slot.latency_samples())
            .fold(0u32, u32::saturating_add)
    }

    /// A plugin that changes its latency on the main thread shows up here
    /// by the next block.
    pub(crate) fn publish_latency(&mut self) -> bool {
        let latency = self.latency_samples();
        let changed = latency != self.latency;
        self.latency = latency;
        changed
    }

    pub(crate) fn latency(&self) -> u32 {
        self.latency
    }

    pub(crate) fn active(&self) -> usize {
        self.slots.iter().filter(|slot| slot.active()).count()
    }

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

    #[cfg(test)]
    pub(crate) fn insert_clap_for_test(&mut self, plugin: LoadedPlugin) {
        self.push_for_test(clap_slot_for_test(plugin, self.sample_rate));
    }

    #[cfg(test)]
    pub(crate) fn insert_delay_for_test(&mut self, latency: u32) {
        self.push_for_test(Slot::delay_for_test(latency));
    }

    #[cfg(test)]
    pub(crate) fn push_for_test(&mut self, slot: Slot) {
        self.slots.push(Box::new(slot));
        self.publish_latency();
    }

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

    /// The test destroys them on its own thread, not through the park.
    #[cfg(test)]
    pub(crate) fn clear_for_test(&mut self) -> Vec<Slot> {
        let slots = self.slots.drain(..).map(|slot| *slot).collect();
        self.publish_latency();
        slots
    }

    /// Chunks of at most [`MAX_BLOCK`].  A skipped effect (bypass, mix at
    /// 0, a pause, the path's chain bypass) and a partial mix still delay
    /// by the effect's latency, so the block comes out
    /// [`Self::latency_samples`] late.  Chain bypass does not write each
    /// slot's own bypass.
    ///
    /// True when a slot failed or the latency changed, so the UI can
    /// republish.  The audio thread does not build views.
    pub(crate) fn process(&mut self, left: &mut [f32], right: &mut [f32], frames: usize) -> bool {
        // Relaxed: an atomic, so this does not lock or allocate.
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
    /// The last owner may be a bridge thread.  Plugins are destroyed on
    /// the UI thread, so the slots go to the park ([`release_slot`]).
    ///
    /// The park takes a lock no audio callback may take.  The last owner
    /// is the UI thread or the bridge; [`release_slot`] asserts that.
    fn drop(&mut self) {
        for slot in self.slots.drain(..) {
            release_slot(slot, &self.slot_park);
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

/// One pointer, swapped in and out, so taking and parking is lock and
/// allocation free.  Exactly one runtime: parked here or in a [`ChainLease`].
pub(crate) struct ChainHome {
    parked: AtomicPtr<ChainRuntime>,
}

impl ChainHome {
    pub(super) fn new(runtime: ChainRuntime) -> Arc<Self> {
        Arc::new(Self {
            parked: AtomicPtr::new(Box::into_raw(Box::new(runtime))),
        })
    }

    /// `None` while someone else holds it.  Lock and allocation free, so
    /// the audio thread may call it.
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

/// Dropping the lease parks the runtime again, lock and allocation free.
pub(crate) struct ChainLease {
    runtime: ManuallyDrop<Box<ChainRuntime>>,
    home: Arc<ChainHome>,
}

impl ChainLease {
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

#[derive(Clone, Copy)]
struct SlotTiming {
    fade: f32,
    declick: u32,
}

/// A switch between the plugin and the delayed input cannot crossfade:
/// only one of the two exists for the chunk, so the step is declicked.
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
    // Cannot crossfade: only the plugin or the delayed input exists for
    // this chunk (bypass, mix through 0, a skip, a restart).
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

/// Every loaded plugin is brought here every chunk, so its pause
/// handshake is served even while it is skipped.
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
