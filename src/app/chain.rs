//! Which Mixer stage is selected, and the order of a path's chain.
//!
//! This is navigation state on [`RigWerkApp`](crate::app::RigWerkApp), not
//! paint.  Stage colours stay in `ui`.  [`chain_stages`] is the signal
//! order the console draws and the sheet's ‹ › walk: the pitch shifter is
//! first, except on a return, which never runs it; the gate sits before or
//! after the effects as its slot says, and each NAM block runs before or
//! after them as its position says.  The cabinet always sits after the
//! effects and after the NAM blocks.

use crate::audio::{EqSlot, GateSlot, NamPosition, NamStack};

/// One stage of a path's chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChainStage {
    /// The pitch shifter, before the *Pre EQ*.  Never on a return.
    Pitch,
    PreEq,
    Gate,
    /// The NAM block at this index of the path's stack.
    Nam(usize),
    /// The effect at this index of the path's chain.
    Effect(usize),
    /// The cabinet, after the effects and after the NAM blocks.
    Cab,
    PostEq,
}

/// A chain's stages in the order the sound passes them: *Pitch* (not on a
/// return), *Pre EQ*, the gate
/// when it sits before the effects, the NAM blocks that sit there, the
/// effects, the NAM blocks that sit after them, the cabinet, the gate when
/// after, and *Post EQ*.
pub(crate) fn chain_stages(
    effects: usize,
    gate: GateSlot,
    nam: &NamStack,
    is_return: bool,
) -> Vec<ChainStage> {
    let mut stages = Vec::with_capacity(effects + nam.count() + 6);
    if !is_return {
        stages.push(ChainStage::Pitch);
    }
    stages.push(ChainStage::PreEq);
    if gate == GateSlot::Pre {
        stages.push(ChainStage::Gate);
    }
    // An empty stack still gets one row: the sheet's way in.
    let pre: Vec<usize> = nam
        .in_position(NamPosition::Pre)
        .map(|(index, _)| index)
        .collect();
    let post: Vec<usize> = nam
        .in_position(NamPosition::Post)
        .map(|(index, _)| index)
        .collect();
    let pre = if nam.is_empty() { vec![0] } else { pre };
    stages.extend(pre.into_iter().map(ChainStage::Nam));
    stages.extend((0..effects).map(ChainStage::Effect));
    stages.extend(post.into_iter().map(ChainStage::Nam));
    stages.push(ChainStage::Cab);
    if gate == GateSlot::Post {
        stages.push(ChainStage::Gate);
    }
    stages.push(ChainStage::PostEq);
    stages
}

/// The stage after `current` (`forward`) or before it.  `None` at either
/// end, and for an effect or block index the chain does not have.
pub(crate) fn neighbour_stage(
    current: ChainStage,
    effects: usize,
    gate: GateSlot,
    nam: &NamStack,
    is_return: bool,
    forward: bool,
) -> Option<ChainStage> {
    let stages = chain_stages(effects, gate, nam, is_return);
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
    Pitch,
    PreEq,
    /// The effect group header — not a single plugin.
    Plugins,
    Plugin(usize),
    Gate,
    /// The NAM block at this index of the path's stack.
    Nam(usize),
    Cab,
    PostEq,
    /// The master stage: master level and mute, and the device list.
    Output,
    /// One output device under the master; 0 is the clock output.
    OutputDevice(usize),
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

    /// Keep a NAM block index inside the stack after add/remove/reorder.
    pub(crate) fn clamp_nam(self, blocks: usize) -> Self {
        match self {
            Self::Nam(_) if blocks == 0 => Self::PreEq,
            Self::Nam(index) if index >= blocks => Self::Nam(blocks - 1),
            other => other,
        }
    }

    /// Keep an output device index inside the device list.
    pub(crate) fn clamp_output(self, device_count: usize) -> Self {
        match self {
            Self::OutputDevice(index) if index >= device_count => Self::Output,
            other => other,
        }
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
            ChainStage::Pitch => Self::Pitch,
            ChainStage::PreEq => Self::PreEq,
            ChainStage::Gate => Self::Gate,
            ChainStage::Nam(index) => Self::Nam(index),
            ChainStage::Effect(index) => Self::Plugin(index),
            ChainStage::Cab => Self::Cab,
            ChainStage::PostEq => Self::PostEq,
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

    /// Whether `stage` of path `slot` is on, or bypassed (the pitch
    /// shifter: on, or off and out of the chain).  An effect that
    /// failed to load keeps its own switch; its row shows the failure.
    /// `false` for a path or an effect that is not there.
    pub(crate) fn stage_on(&self, slot: usize, stage: ChainStage) -> bool {
        let Some(path) = self.paths.get(slot).and_then(Option::as_ref) else {
            return false;
        };
        match stage {
            ChainStage::Pitch => path.mix.pitch.enabled,
            ChainStage::PreEq => path.mix.eqs[0].enabled,
            ChainStage::PostEq => path.mix.eqs[1].enabled,
            ChainStage::Gate => path.mix.gate.enabled,
            ChainStage::Nam(index) => path
                .nam
                .get(index)
                .is_some_and(|block| block.controls.enabled),
            ChainStage::Cab => path.mix.cab.enabled,
            ChainStage::Effect(index) => self
                .engine
                .plugin_views(slot)
                .get(index)
                .is_some_and(|view| !view.bypass),
        }
    }

    /// Switches `stage` of path `slot` on, or bypasses it.  An EQ keeps its
    /// bands, the gate its settings and a NAM block its capture; an effect
    /// keeps its latency.  The pitch shifter keeps its settings; off, it
    /// takes its latency with it.
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
            ChainStage::Pitch => path.mix.pitch.enabled = on,
            ChainStage::PreEq => path.mix.eqs[0].enabled = on,
            ChainStage::PostEq => path.mix.eqs[1].enabled = on,
            ChainStage::Gate => path.mix.gate.enabled = on,
            ChainStage::Nam(index) => {
                if let Some(block) = path.nam.get_mut(index) {
                    block.controls.enabled = on;
                }
            }
            ChainStage::Cab => path.mix.cab.enabled = on,
            ChainStage::Effect(_) => {}
        }
        self.push_paths_to_engine();
    }
}

#[cfg(test)]
mod tests;
