//! Which Mixer stage is selected.
//!
//! This is navigation state on [`RigWerkApp`](crate::app::RigWerkApp), not
//! paint.  Stage colours stay in `ui`.

use crate::audio::EqSlot;

/// What the Mixer inspector (and Live chips) currently point at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChainSelection {
    Input,
    PreEq,
    /// The insert group header — not a single plugin.
    Plugins,
    Plugin(usize),
    Gate,
    PostEq,
    /// The selected path's level, mute and solo (shown with 2+ paths).
    Path,
    /// The master stage: master level and mute, and the device list.
    Output,
    /// One output device under the master; 0 is the clock device.
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

    /// The selection after the insert at `removed` left the chain, with
    /// `plugin_count` inserts remaining: a later insert keeps its selection as
    /// it slides up. Removing the selected insert selects the next one, or the
    /// previous one when it was the last.
    pub(crate) fn after_removal(self, removed: usize, plugin_count: usize) -> Self {
        match self {
            Self::Plugin(index) if index > removed => Self::Plugin(index - 1),
            other => other.clamp(plugin_count),
        }
    }

    /// Where a newly added insert should land. `None` means append.
    pub(crate) fn plugin_insert_at(self, plugin_count: usize) -> Option<usize> {
        match self {
            Self::Plugin(index) => Some((index + 1).min(plugin_count)),
            Self::Plugins => None,
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests;
