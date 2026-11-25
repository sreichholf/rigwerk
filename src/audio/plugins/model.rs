//! One signal path's effect chain on the UI side: the [`ChainModel`].  It
//! is the chain as the UI edits and shows it, one [`SlotLink`] per effect,
//! and the only sender of [`ChainCommand`]s to the path's
//! [`ChainRuntime`](super::ChainRuntime) (see `chain`).
//!
//! Every edit changes the links at once and queues the command that makes
//! the runtime match.  Slots are built here, on the UI thread: a plugin is
//! loaded and activated before its slot is sent.  [`ChainModel::flush`]
//! sends what is queued, applies it itself while the runtime is parked (no
//! bridge, or a path that is not live), and collects the slots the runtime
//! handed back, which the caller destroys on the UI thread.
//!
//! The model is UI-only.  `PathControls` keeps it behind a mutex the audio
//! thread never takes; the links it publishes are what drawing, servicing
//! and saving read.

use std::collections::VecDeque;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use super::chain::{
    release_command, release_slot, ChainCommand, ChainHome, ChainRuntime, COMMAND_QUEUE,
    MAX_EFFECTS, RETURN_QUEUE,
};
#[cfg(test)]
use super::handle::LoadedPlugin;
use super::handle::PluginHandle;
use super::pref::PluginPref;
use super::queue::{queue, Consumer, Producer};
use super::slot::{Slot, SlotLink, SlotPark};

/// Slots [`ChainModel::reload_plan`] wants opened at a new or first rate.
/// The caller builds them and hands them to [`ChainModel::install`].
pub(crate) struct ReloadPlan {
    pub(crate) previous_rate: u32,
    pub(crate) jobs: Vec<(usize, PluginPref)>,
    /// The jobs' indices whose plugin asked to be rebuilt.
    pub(crate) requested: Vec<usize>,
}

/// One path's effect chain as the UI edits it.
pub(crate) struct ChainModel {
    /// One per effect, in chain order: the chain as it stands once every
    /// queued command applied.
    links: Vec<SlotLink>,
    /// The rate the slots are built for.
    rate: u32,
    commands: Producer<ChainCommand>,
    /// Commands that did not fit the queue yet, oldest first.
    backlog: VecDeque<ChainCommand>,
    returns: Consumer<Box<Slot>>,
    home: Arc<ChainHome>,
    /// Where a slot this model or its queues let go of is released.  The
    /// engine owns it, so the park outlives every run.
    slot_park: Arc<SlotPark>,
}

impl ChainModel {
    /// An empty chain at `sample_rate` (0 means 48 kHz) and its runtime,
    /// parked in [`Self::home`].  `chain_bypass` is the path's bypass flag,
    /// which the runtime loads every block.
    pub(crate) fn new(
        sample_rate: u32,
        chain_bypass: Arc<AtomicBool>,
        slot_park: Arc<SlotPark>,
    ) -> Self {
        let rate = if sample_rate == 0 {
            48_000
        } else {
            sample_rate
        };
        let command_park = Arc::clone(&slot_park);
        let return_park = Arc::clone(&slot_park);
        let (commands, command_rx) = queue(COMMAND_QUEUE, move |command| {
            release_command(command, &command_park)
        });
        let (return_tx, returns) =
            queue(RETURN_QUEUE, move |slot| release_slot(slot, &return_park));
        let runtime = ChainRuntime::new(
            rate,
            chain_bypass,
            command_rx,
            return_tx,
            Arc::clone(&slot_park),
        );
        Self {
            links: Vec::new(),
            rate,
            commands,
            backlog: VecDeque::new(),
            returns,
            home: ChainHome::new(runtime),
            slot_park,
        }
    }

    /// Where the runtime waits for the audio thread.
    pub(crate) fn home(&self) -> Arc<ChainHome> {
        Arc::clone(&self.home)
    }

    pub(crate) fn rate(&self) -> u32 {
        self.rate
    }

    pub(crate) fn links(&self) -> &[SlotLink] {
        &self.links
    }

    pub(crate) fn is_full(&self) -> bool {
        self.links.len() >= MAX_EFFECTS
    }

    /// Whether a restored slot is still waiting for its first open.
    pub(crate) fn has_unopened(&self) -> bool {
        self.links.iter().any(SlotLink::needs_open)
    }

    fn send(&mut self, command: ChainCommand) {
        self.backlog.push_back(command);
    }

    /// Sends the queued commands, applies them at once if the runtime is
    /// parked, and returns the boxes the runtime handed back, for the caller
    /// to release through [`release_slot`] on this (the UI) thread.  Never
    /// waits for the audio side: a runtime it holds applies the commands
    /// with its next block, and the slots come back with a later flush.
    #[must_use]
    #[allow(clippy::vec_box)] // The boxes travel queued: collecting them moves no slot.
    pub(crate) fn flush(&mut self) -> Vec<Box<Slot>> {
        let mut returned = Vec::new();
        loop {
            // First, so a runtime parked with a full return queue has room.
            returned.extend(std::iter::from_fn(|| self.returns.pop()));
            while let Some(command) = self.backlog.pop_front() {
                if let Err(command) = self.commands.push(command) {
                    self.backlog.push_front(command);
                    break;
                }
            }
            if self.commands.is_empty() {
                break;
            }
            // Parked again as the lease drops, before the slots it handed
            // back are collected.
            let applied = match self.home.take() {
                Some(mut runtime) => runtime.apply_commands(),
                None => false,
            };
            returned.extend(std::iter::from_fn(|| self.returns.pop()));
            // A full queue (or return queue) held commands back: go round
            // again with the room just made.
            if !applied {
                break;
            }
        }
        returned.extend(std::iter::from_fn(|| self.returns.pop()));
        returned
    }

    /// Whether commands are still on their way to the runtime.
    #[cfg(test)]
    pub(crate) fn pending(&self) -> bool {
        !self.backlog.is_empty() || !self.commands.is_empty()
    }

    /// Replaces every effect with unopened slots for `prefs`.  Only the
    /// first [`MAX_EFFECTS`] are kept.
    pub(crate) fn restore(&mut self, mut prefs: Vec<PluginPref>) {
        for index in (0..self.links.len()).rev() {
            self.send(ChainCommand::Remove { index });
        }
        self.links.clear();
        if prefs.len() > MAX_EFFECTS {
            eprintln!(
                "[plugins] a path holds at most {MAX_EFFECTS} effects; {} were not restored",
                prefs.len() - MAX_EFFECTS
            );
            prefs.truncate(MAX_EFFECTS);
        }
        for pref in prefs {
            self.append(Slot::empty(pref));
        }
    }

    /// Adds `slot` at the end, or hands it back when the chain is full
    /// (see [`Self::is_full`]).
    #[allow(clippy::result_large_err)] // The caller drops the slot it gets back.
    pub(crate) fn push(&mut self, slot: Slot) -> Result<(), Slot> {
        if self.is_full() {
            return Err(slot);
        }
        self.append(slot);
        Ok(())
    }

    fn append(&mut self, slot: Slot) {
        let index = self.links.len();
        self.links.push(slot.link());
        self.send(ChainCommand::Insert {
            index,
            slot: Box::new(slot),
        });
    }

    /// Takes the effect at `index` out.  The runtime hands its slot back.
    pub(crate) fn remove(&mut self, index: usize) {
        if index < self.links.len() {
            self.links.remove(index);
            self.send(ChainCommand::Remove { index });
        }
    }

    /// Swaps the effect at `index` with its neighbour `delta` places away
    /// (clamped to the chain).
    pub(crate) fn move_slot(&mut self, index: usize, delta: isize) {
        if index >= self.links.len() {
            return;
        }
        let target = (index as isize + delta).clamp(0, self.links.len() as isize - 1) as usize;
        if target != index {
            self.links.swap(index, target);
            self.send(ChainCommand::Swap {
                a: index,
                b: target,
            });
        }
    }

    /// Puts `slot` in place of the effect at `index`.  It keeps that
    /// effect's controls, so mix, bypass and the counters carry over.
    fn replace(&mut self, index: usize, mut slot: Slot) {
        let Some(link) = self.links.get_mut(index) else {
            return;
        };
        slot.controls = Arc::clone(&link.controls);
        *link = slot.link();
        self.send(ChainCommand::Replace {
            index,
            slot: Box::new(slot),
        });
    }

    fn set_rate(&mut self, rate: u32) {
        if rate != self.rate {
            self.rate = rate;
            self.send(ChainCommand::SetRate(rate));
        }
    }

    /// Lists slots to rebuild at `rate` without loading them: every slot
    /// when the rate changed, else the never-opened ones and those whose
    /// plugin asked to be rebuilt.  When the rate changed and nothing is
    /// installed, the rate is updated here.  The caller loads
    /// [`ReloadPlan::jobs`] and installs them with [`Self::install`].
    pub(crate) fn reload_plan(&mut self, rate: u32) -> Option<ReloadPlan> {
        let rate_changed = rate != self.rate;
        let jobs: Vec<(usize, PluginPref)> = self
            .links
            .iter()
            .enumerate()
            .filter(|(_, link)| rate_changed || link.needs_open() || link.reload_wanted())
            .map(|(index, link)| (index, link.restore_pref()))
            .collect();
        let requested = jobs
            .iter()
            .map(|(index, _)| *index)
            .filter(|&index| self.links[index].reload_wanted())
            .collect();
        if jobs.is_empty() {
            self.set_rate(rate);
            return None;
        }
        Some(ReloadPlan {
            previous_rate: self.rate,
            jobs,
            requested,
        })
    }

    /// Lists only the slots whose plugin asked to be rebuilt, at the
    /// chain's rate: a reload request opens nothing else.
    pub(crate) fn request_plan(&mut self) -> Option<ReloadPlan> {
        let jobs: Vec<(usize, PluginPref)> = self
            .links
            .iter()
            .enumerate()
            .filter(|(_, link)| link.reload_wanted())
            .map(|(index, link)| (index, link.restore_pref()))
            .collect();
        (!jobs.is_empty()).then(|| ReloadPlan {
            previous_rate: self.rate,
            requested: jobs.iter().map(|(index, _)| *index).collect(),
            jobs,
        })
    }

    /// Puts slots built for a [`ReloadPlan`] in place, then moves the chain
    /// to `rate`.  Each keeps the controls of the effect it replaces.
    pub(crate) fn install(&mut self, rate: u32, built: Vec<(usize, Slot)>) {
        for (index, slot) in built {
            self.replace(index, slot);
        }
        self.set_rate(rate);
    }

    /// Opens a saved slot so its controls can be shown before playback
    /// starts, or reads a loaded one's control list again.
    pub(crate) fn ensure_loaded(&mut self, index: usize) {
        if self.open_if_needed(index) {
            return;
        }
        if let Some(handle) = self.links.get(index).and_then(|link| link.plugin.as_ref()) {
            handle.refresh_parameters();
        }
    }

    /// Opens the never-opened slot at `index`.  Returns whether it loaded
    /// one (failed or not), which also read the fresh control list.
    fn open_if_needed(&mut self, index: usize) -> bool {
        let Some(link) = self.links.get(index) else {
            return false;
        };
        if !link.needs_open() {
            return false;
        }
        let slot = Slot::loaded(link.saved_pref(), self.rate);
        self.replace(index, slot);
        true
    }

    /// Sets one normalized plugin control, opening a never-opened slot
    /// first.  A loaded slot is reached through its link instead (see
    /// `PathControls::set_plugin_parameter`).
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
        self.open_handle(index)?.reset_parameters()
    }

    /// The handle of slot `index`, opened if it never was.
    fn open_handle(&mut self, index: usize) -> Result<PluginHandle, String> {
        self.open_if_needed(index);
        let Some(link) = self.links.get(index) else {
            return Err("That effect is no longer in the chain".to_owned());
        };
        match &link.plugin {
            Some(plugin) => Ok(plugin.clone()),
            None => Err(link
                .error
                .clone()
                .unwrap_or_else(|| "The plugin is not loaded".to_owned())),
        }
    }

    /// Sets one effect's bypass.  The UI goes through the published links
    /// instead (`PathControls::set_plugin_bypass`).
    #[cfg(test)]
    pub(crate) fn set_bypass(&mut self, index: usize, bypass: bool) {
        if let Some(link) = self.links.get(index) {
            link.controls.set_bypass(bypass);
        }
    }

    /// Adds an already loaded CLAP plugin as a slot, the way a real load
    /// fills one in.
    #[cfg(test)]
    pub(crate) fn insert_clap_for_test(&mut self, plugin: LoadedPlugin) {
        self.append(super::chain::clap_slot_for_test(plugin, self.rate));
    }

    /// Adds the pure-delay test effect (see `Slot::delay_for_test`).
    #[cfg(test)]
    pub(crate) fn insert_delay_for_test(&mut self, latency: u32) {
        self.append(Slot::delay_for_test(latency));
    }

    #[cfg(test)]
    pub(crate) fn insert_tracked_slot_for_test(&mut self) {
        self.append(Slot::tracked_for_test());
    }

    #[cfg(test)]
    pub(crate) fn built_rates(&self) -> Vec<Option<u32>> {
        self.links.iter().map(|link| link.built_rate).collect()
    }
}

impl Drop for ChainModel {
    /// Commands that never reached the runtime and slots it handed back go
    /// where a chain edit's leftovers go (see `chain::release_slot`).
    fn drop(&mut self) {
        // The links hold handles of the plugins in those slots: gone first,
        // so a slot released to the park is its plugin's last owner, and the
        // main thread destroys it.
        self.links.clear();
        for command in self.backlog.drain(..) {
            release_command(command, &self.slot_park);
        }
        while let Some(slot) = self.returns.pop() {
            release_slot(slot, &self.slot_park);
        }
    }
}

#[cfg(test)]
mod tests;
