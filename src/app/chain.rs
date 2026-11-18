//! Which Mixer stage is selected, and the order of a path's chain.
//!
//! This is navigation state on [`RigWerkApp`](crate::app::RigWerkApp), not
//! paint.  Stage colours stay in `ui`.  [`chain_stages`] is the signal
//! order the console draws and the sheet's ‹ › walk: the gate sits before
//! or after the effects as its slot says.

use crate::audio::{EqSlot, GateSlot};

/// One stage of a path's chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChainStage {
    PreEq,
    Gate,
    /// The effect at this index of the path's chain.
    Effect(usize),
    PostEq,
}

/// A chain's stages in the order the sound passes them: *Pre EQ*, the gate
/// when it sits before the effects, the effects, the gate when after, and
/// *Post EQ*.
pub(crate) fn chain_stages(effects: usize, gate: GateSlot) -> Vec<ChainStage> {
    let mut stages = Vec::with_capacity(effects + 3);
    stages.push(ChainStage::PreEq);
    if gate == GateSlot::Pre {
        stages.push(ChainStage::Gate);
    }
    stages.extend((0..effects).map(ChainStage::Effect));
    if gate == GateSlot::Post {
        stages.push(ChainStage::Gate);
    }
    stages.push(ChainStage::PostEq);
    stages
}

/// The stage after `current` (`forward`) or before it.  `None` at either
/// end, and for an effect index the chain does not have.
pub(crate) fn neighbour_stage(
    current: ChainStage,
    effects: usize,
    gate: GateSlot,
    forward: bool,
) -> Option<ChainStage> {
    let stages = chain_stages(effects, gate);
    let at = stages.iter().position(|stage| *stage == current)?;
    if forward {
        stages.get(at + 1).copied()
    } else {
        at.checked_sub(1)
            .and_then(|before| stages.get(before).copied())
    }
}

/// What the Mixer inspector (and Live chips) currently point at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChainSelection {
    Input,
    PreEq,
    /// The effect group header — not a single plugin.
    Plugins,
    Plugin(usize),
    Gate,
    PostEq,
    /// The selected path's level, mute and solo (shown with 2+ paths).
    Path,
    /// The master stage: master level and mute, and the device list.
    Output,
    /// One output device under the master; 0 is the clock output.
    OutputDevice(usize),
    /// The add-output-device list under the Output stage.
    AddOutput,
}

impl ChainSelection {
    pub(crate) fn from_eq_edit(slot: EqSlot) -> Self {
        match slot {
            EqSlot::Pre => Self::PreEq,
            EqSlot::Post => Self::PostEq,
        }
    }

    pub(crate) fn eq_slot(self) -> Option<EqSlot> {
        match self {
            Self::PreEq => Some(EqSlot::Pre),
            Self::PostEq => Some(EqSlot::Post),
            _ => None,
        }
    }

    /// Keep a plugin index inside the live chain after add/remove/reorder.
    pub(crate) fn clamp(self, plugin_count: usize) -> Self {
        match self {
            Self::Plugin(_) if plugin_count == 0 => Self::Plugins,
            Self::Plugin(index) if index >= plugin_count => Self::Plugin(plugin_count - 1),
            other => other,
        }
    }

    /// Keep an output device index inside the device list.
    pub(crate) fn clamp_output(self, device_count: usize) -> Self {
        match self {
            Self::OutputDevice(index) if index >= device_count => Self::Output,
            Self::AddOutput if device_count == 0 => Self::Output,
            other => other,
        }
    }

    /// The Path stage only exists with several paths; with one it falls
    /// back to the stage above it.
    pub(crate) fn clamp_path(self, path_count: usize) -> Self {
        match self {
            Self::Path if path_count < 2 => Self::PostEq,
            other => other,
        }
    }

    /// The stage a `RIGWERK_MIXER_STAGE` key names (screenshots and checks).
    pub(crate) fn from_key(key: &str) -> Option<Self> {
        Some(match key {
            "input" => Self::Input,
            "pre_eq" => Self::PreEq,
            "plugins" => Self::Plugins,
            "gate" => Self::Gate,
            "post_eq" => Self::PostEq,
            "path" => Self::Path,
            "output" => Self::Output,
            "device" => Self::OutputDevice(0),
            _ => return None,
        })
    }

    /// The selection after the effect at `removed` left the chain, with
    /// `plugin_count` effects remaining: a later effect keeps its selection as
    /// it slides up. Removing the selected effect selects the next one, or the
    /// previous one when it was the last.
    pub(crate) fn after_removal(self, removed: usize, plugin_count: usize) -> Self {
        match self {
            Self::Plugin(index) if index > removed => Self::Plugin(index - 1),
            other => other.clamp(plugin_count),
        }
    }

    /// The selection that marks `stage` in the Mixer and on the console.
    pub(crate) fn from_stage(stage: ChainStage) -> Self {
        match stage {
            ChainStage::PreEq => Self::PreEq,
            ChainStage::Gate => Self::Gate,
            ChainStage::Effect(index) => Self::Plugin(index),
            ChainStage::PostEq => Self::PostEq,
        }
    }

    /// Where a newly added effect should land. `None` means append.
    pub(crate) fn plugin_effect_at(self, plugin_count: usize) -> Option<usize> {
        match self {
            Self::Plugin(index) => Some((index + 1).min(plugin_count)),
            Self::Plugins => None,
            _ => None,
        }
    }
}

impl crate::app::RigWerkApp {
    /// Marks `selection` in the Mixer and on the console.  An EQ is also
    /// remembered as the one shown last (`eq_edit`).
    pub(crate) fn select_chain(&mut self, selection: ChainSelection) {
        self.chain_selected = selection;
        if let Some(slot) = selection.eq_slot() {
            self.eq_edit = slot;
        }
    }

    /// Whether `stage` of path `slot` is on, or bypassed.  An effect that
    /// failed to load keeps its own switch; its row shows the failure.
    /// `false` for a path or an effect that is not there.
    pub(crate) fn stage_on(&self, slot: usize, stage: ChainStage) -> bool {
        let Some(path) = self.paths.get(slot).and_then(Option::as_ref) else {
            return false;
        };
        match stage {
            ChainStage::PreEq => path.mix.eqs[0].enabled,
            ChainStage::PostEq => path.mix.eqs[1].enabled,
            ChainStage::Gate => path.mix.gate.enabled,
            ChainStage::Effect(index) => self
                .engine
                .plugin_views(slot)
                .get(index)
                .is_some_and(|view| !view.bypass),
        }
    }

    /// Switches `stage` of path `slot` on, or bypasses it.  An EQ keeps its
    /// bands and the gate its settings; an effect keeps its latency.
    pub(crate) fn set_stage_on(&mut self, slot: usize, stage: ChainStage, on: bool) {
        if let ChainStage::Effect(index) = stage {
            if self.paths.get(slot).is_some_and(Option::is_some) {
                self.set_plugin_bypass(slot, index, !on);
            }
            return;
        }
        let Some(path) = self.paths.get_mut(slot).and_then(Option::as_mut) else {
            return;
        };
        match stage {
            ChainStage::PreEq => path.mix.eqs[0].enabled = on,
            ChainStage::PostEq => path.mix.eqs[1].enabled = on,
            ChainStage::Gate => path.mix.gate.enabled = on,
            ChainStage::Effect(_) => {}
        }
    }
}

#[cfg(test)]
mod tests;
