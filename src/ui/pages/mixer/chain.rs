//! Shared master-chain paint for Mixer, Live strip, and Plugins routing.
//!
//! Which stage is selected lives in `app::chain`.  This file is the
//! colours and the summaries the strips draw.

use eframe::egui::Color32;

use crate::app::chain::ChainSelection;
use crate::audio::{EqSettings, GateSettings, GateSlot, PluginView};
use crate::ui::theme::{AMBER, BLUE, DIM, RED, TEAL};

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

/// The Path row's summary: level, or muted, and solo.
pub(crate) fn path_summary(level_db: f32, mute: bool, solo: bool) -> String {
    let mut text = if mute {
        "muted".to_owned()
    } else {
        format!("{level_db:+.1} dB")
    };
    if solo {
        text.push_str(" · solo");
    }
    text
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

/// Tooltip for the path-level insert bypass, shared by Live and Mixer.
pub(crate) const CHAIN_BYPASS_HINT: &str =
    "Skips the inserts on this path and keeps their latency.";

/// Turns the Live PLUGINS stage hot while the path's chain bypass is on:
/// amber, and the summary reads `bypassed`.  Off leaves the stages as
/// [`live_chain_entries`] built them.
pub(crate) fn apply_chain_bypass(entries: &mut [ChainEntry], chain_bypass: bool) {
    if !chain_bypass {
        return;
    }
    if let Some(plugins) = entries
        .iter_mut()
        .find(|entry| entry.selection == ChainSelection::Plugins)
    {
        plugins.summary = "bypassed".to_owned();
        plugins.live = true;
    }
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
        ChainSelection::Input | ChainSelection::Path => TEAL,
        ChainSelection::Output | ChainSelection::OutputDevice(_) | ChainSelection::AddOutput => {
            BLUE
        }
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
mod tests;
