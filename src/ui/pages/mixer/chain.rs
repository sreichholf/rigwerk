//! Shared master-chain identity for Mixer, Live strip, and Plugins routing.

use eframe::egui::Color32;

use crate::audio::{EqSettings, EqSlot, GateSettings, GateSlot, PluginView};
use crate::ui::theme::{AMBER, BLUE, DIM, RED, TEAL};

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
    /// The master stage: master level and mute, and the device list.
    Output,
    /// One output device under the master; 0 is the clock device.
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

    /// Keep an output device index inside the device list.
    pub(crate) fn clamp_output(self, device_count: usize) -> Self {
        match self {
            Self::OutputDevice(index) if index >= device_count => Self::Output,
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

/// One chip/row on the shared chain views.
#[derive(Debug, Clone)]
pub(crate) struct ChainEntry {
    pub(crate) selection: ChainSelection,
    pub(crate) label: &'static str,
    pub(crate) summary: String,
    pub(crate) color: Color32,
    pub(crate) live: bool,
}

pub(crate) fn eq_summary(eq: &EqSettings) -> (String, bool) {
    if !eq.enabled {
        return ("bypassed".to_owned(), false);
    }
    let shaping = eq
        .bands
        .iter()
        .filter(|band| band.on && band.gain_db.abs() > 0.01)
        .count();
    if shaping == 0 {
        ("flat".to_owned(), false)
    } else {
        (
            format!("{shaping} band{}", if shaping == 1 { "" } else { "s" }),
            true,
        )
    }
}

pub(crate) fn gate_summary(gate: &GateSettings) -> (String, bool) {
    let slot = match gate.slot {
        GateSlot::Pre => "PRE",
        GateSlot::Post => "POST",
    };
    if !gate.enabled {
        (format!("bypassed · {slot}"), false)
    } else {
        (format!("on · {slot}"), true)
    }
}

/// The Output (master) row's summary: master level, or muted, and how many
/// output devices play the mix.
pub(crate) fn output_summary(output_gain: f32, mute: bool, devices: usize) -> String {
    let level = if mute {
        "muted".to_owned()
    } else {
        format!("{output_gain:+.1} dB")
    };
    format!(
        "{level} · {devices} device{}",
        if devices == 1 { "" } else { "s" }
    )
}

pub(crate) fn plugin_group_summary(views: &[PluginView]) -> (String, bool) {
    let active = views
        .iter()
        .filter(|view| !view.bypass && view.error.is_none())
        .count();
    if views.is_empty() {
        ("none".to_owned(), false)
    } else if active == 0 {
        (format!("{} bypassed", views.len()), false)
    } else if views.len() == 1 {
        (views[0].name.clone(), true)
    } else {
        (format!("{active} inserts"), active > 0)
    }
}

/// Fixed-order stages for Live (inserts collapsed to one chip).  Gate sits
/// before or after inserts according to its slot.
pub(crate) fn live_chain_entries(
    input_gain: f32,
    pre: &EqSettings,
    views: &[PluginView],
    gate: &GateSettings,
    post: &EqSettings,
    output_gain: f32,
    mute: bool,
) -> Vec<ChainEntry> {
    let (pre_summary, pre_on) = eq_summary(pre);
    let (post_summary, post_on) = eq_summary(post);
    let (plugins_summary, plugins_on) = plugin_group_summary(views);
    let (gate_summary, gate_on) = gate_summary(gate);
    let mut entries = vec![
        ChainEntry {
            selection: ChainSelection::Input,
            label: "INPUT",
            summary: format!("{input_gain:+.1} dB"),
            color: TEAL,
            live: true,
        },
        ChainEntry {
            selection: ChainSelection::PreEq,
            label: "PRE EQ",
            summary: pre_summary,
            color: AMBER,
            live: pre_on,
        },
    ];
    let plugins = ChainEntry {
        selection: ChainSelection::Plugins,
        label: "PLUGINS",
        summary: plugins_summary,
        color: AMBER,
        live: plugins_on,
    };
    let gate_entry = ChainEntry {
        selection: ChainSelection::Gate,
        label: "GATE",
        summary: gate_summary,
        color: AMBER,
        live: gate_on,
    };
    match gate.slot {
        GateSlot::Pre => {
            entries.push(gate_entry);
            entries.push(plugins);
        }
        GateSlot::Post => {
            entries.push(plugins);
            entries.push(gate_entry);
        }
    }
    entries.push(ChainEntry {
        selection: ChainSelection::PostEq,
        label: "POST EQ",
        summary: post_summary,
        color: AMBER,
        live: post_on,
    });
    entries.push(ChainEntry {
        selection: ChainSelection::Output,
        label: "OUTPUT",
        summary: if mute {
            "muted".to_owned()
        } else {
            format!("{output_gain:+.1} dB")
        },
        color: if mute { RED } else { BLUE },
        live: true,
    });
    entries
}

pub(crate) fn stage_color(selection: ChainSelection) -> Color32 {
    match selection {
        ChainSelection::Input => TEAL,
        ChainSelection::Output | ChainSelection::OutputDevice(_) => BLUE,
        ChainSelection::PreEq
        | ChainSelection::PostEq
        | ChainSelection::Gate
        | ChainSelection::Plugins
        | ChainSelection::Plugin(_) => AMBER,
    }
}

pub(crate) fn bypassed_color(live: bool, accent: Color32) -> Color32 {
    if live {
        accent
    } else {
        DIM
    }
}

#[cfg(test)]
mod tests {
    use super::{live_chain_entries, output_summary, ChainSelection};
    use crate::audio::{EqSettings, EqSlot, GateSettings, GateSlot};

    #[test]
    fn eq_edit_round_trips_through_selection() {
        assert_eq!(
            ChainSelection::from_eq_edit(EqSlot::Pre).eq_slot(),
            Some(EqSlot::Pre)
        );
        assert_eq!(
            ChainSelection::from_eq_edit(EqSlot::Post).eq_slot(),
            Some(EqSlot::Post)
        );
    }

    #[test]
    fn plugin_selection_clamps_after_remove() {
        assert_eq!(
            ChainSelection::Plugin(3).clamp(2),
            ChainSelection::Plugin(1)
        );
        assert_eq!(ChainSelection::Plugin(0).clamp(0), ChainSelection::Plugins);
    }

    #[test]
    fn selection_follows_its_insert_after_remove() {
        // Removing an earlier insert keeps the same insert selected.
        assert_eq!(
            ChainSelection::Plugin(2).after_removal(0, 2),
            ChainSelection::Plugin(1)
        );
        // Removing a later one leaves the selection alone.
        assert_eq!(
            ChainSelection::Plugin(0).after_removal(1, 2),
            ChainSelection::Plugin(0)
        );
        // Removing the selected last insert selects the new last one.
        assert_eq!(
            ChainSelection::Plugin(2).after_removal(2, 2),
            ChainSelection::Plugin(1)
        );
        assert_eq!(
            ChainSelection::Plugin(0).after_removal(0, 0),
            ChainSelection::Plugins
        );
        assert_eq!(
            ChainSelection::Gate.after_removal(0, 0),
            ChainSelection::Gate
        );
        // Removing the selected insert selects the one that slid into place.
        assert_eq!(
            ChainSelection::Plugin(1).after_removal(1, 2),
            ChainSelection::Plugin(1)
        );
        assert_eq!(
            ChainSelection::Plugins.after_removal(0, 1),
            ChainSelection::Plugins
        );
    }

    #[test]
    fn output_device_selection_falls_back_to_the_master() {
        assert_eq!(
            ChainSelection::OutputDevice(0).clamp_output(1),
            ChainSelection::OutputDevice(0)
        );
        assert_eq!(
            ChainSelection::OutputDevice(1).clamp_output(1),
            ChainSelection::Output
        );
        assert_eq!(ChainSelection::Gate.clamp_output(0), ChainSelection::Gate);
        // The insert clamp leaves output devices alone.
        assert_eq!(
            ChainSelection::OutputDevice(0).clamp(0),
            ChainSelection::OutputDevice(0)
        );
        assert_eq!(ChainSelection::OutputDevice(0).plugin_insert_at(2), None);
        assert_eq!(ChainSelection::OutputDevice(0).eq_slot(), None);
    }

    #[test]
    fn stage_keys_name_the_mixer_stages() {
        assert_eq!(
            ChainSelection::from_key("output"),
            Some(ChainSelection::Output)
        );
        assert_eq!(
            ChainSelection::from_key("device"),
            Some(ChainSelection::OutputDevice(0))
        );
        assert_eq!(
            ChainSelection::from_key("post_eq"),
            Some(ChainSelection::PostEq)
        );
        assert_eq!(ChainSelection::from_key("clock"), None);
        assert_eq!(ChainSelection::from_key("nope"), None);
    }

    #[test]
    fn output_summary_shows_master_level_and_device_count() {
        assert_eq!(output_summary(-1.5, false, 1), "-1.5 dB · 1 device");
        assert_eq!(output_summary(0.0, false, 3), "+0.0 dB · 3 devices");
        assert_eq!(output_summary(-1.5, true, 1), "muted · 1 device");
    }

    #[test]
    fn insert_point_follows_selected_plugin() {
        assert_eq!(ChainSelection::Plugin(1).plugin_insert_at(3), Some(2));
        assert_eq!(ChainSelection::Plugins.plugin_insert_at(3), None);
        assert_eq!(ChainSelection::PreEq.plugin_insert_at(3), None);
    }

    #[test]
    fn live_strip_puts_post_gate_after_plugins() {
        let gate = GateSettings {
            slot: GateSlot::Post,
            enabled: true,
            ..GateSettings::default()
        };
        let entries = live_chain_entries(
            0.0,
            &EqSettings::default(),
            &[],
            &gate,
            &EqSettings::default(),
            -1.5,
            false,
        );
        let labels: Vec<_> = entries.iter().map(|e| e.label).collect();
        assert_eq!(
            labels,
            ["INPUT", "PRE EQ", "PLUGINS", "GATE", "POST EQ", "OUTPUT"]
        );
    }

    #[test]
    fn live_strip_puts_pre_gate_before_plugins() {
        let gate = GateSettings {
            slot: GateSlot::Pre,
            enabled: true,
            ..GateSettings::default()
        };
        let entries = live_chain_entries(
            0.0,
            &EqSettings::default(),
            &[],
            &gate,
            &EqSettings::default(),
            -1.5,
            false,
        );
        let labels: Vec<_> = entries.iter().map(|e| e.label).collect();
        assert_eq!(
            labels,
            ["INPUT", "PRE EQ", "GATE", "PLUGINS", "POST EQ", "OUTPUT"]
        );
    }
}
