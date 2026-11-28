//! The pitch shifter is first, except on a return, which never runs it.
//! The gate and each NAM block sit before or after the effects as their
//! slot says.  The cabinet always sits after the effects and the NAM blocks.

use crate::audio::{EqSlot, GateSlot, NamPosition, NamStack};

/// One stage of a path's chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChainStage {
    Pitch,
    PreEq,
    Gate,
    Nam(usize),
    Effect(usize),
    Cab,
    PostEq,
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChainSelection {
    Pitch,
    PreEq,
    /// The effect group header, not one plugin.
    Plugins,
    Plugin(usize),
    Gate,
    Nam(usize),
    Cab,
    PostEq,
    Output,
    /// 0 is the clock output.
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

    pub(crate) fn clamp(self, plugin_count: usize) -> Self {
        match self {
            Self::Plugin(_) if plugin_count == 0 => Self::Plugins,
            Self::Plugin(index) if index >= plugin_count => Self::Plugin(plugin_count - 1),
            other => other,
        }
    }

    pub(crate) fn clamp_nam(self, blocks: usize) -> Self {
        match self {
            Self::Nam(_) if blocks == 0 => Self::PreEq,
            Self::Nam(index) if index >= blocks => Self::Nam(blocks - 1),
            other => other,
        }
    }

    pub(crate) fn clamp_output(self, device_count: usize) -> Self {
        match self {
            Self::OutputDevice(index) if index >= device_count => Self::Output,
            other => other,
        }
    }

    /// Removing the selected effect selects the next one, or the previous
    /// when it was the last.  A later effect keeps its selection as it
    /// slides up.
    pub(crate) fn after_removal(self, removed: usize, plugin_count: usize) -> Self {
        match self {
            Self::Plugin(index) if index > removed => Self::Plugin(index - 1),
            other => other.clamp(plugin_count),
        }
    }

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
    pub(crate) fn select_chain(&mut self, selection: ChainSelection) {
        self.chain_selected = selection;
        if let Some(slot) = selection.eq_slot() {
            self.eq_edit = slot;
        }
    }

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

    /// Off, the pitch shifter takes its latency with it.  An effect keeps
    /// its latency; the other stages keep their settings.
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
