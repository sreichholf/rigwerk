//! The fixtures a screenshot run assembles: fake routing, effects, NAM
//! blocks, a cab IR, Match paths rows and the extras a checked screen needs.
//!
//! Dev-only.  Every builder reads a `RIGWERK_*` variable and the driver in
//! [`super`] (`RigWerkApp::handle_screenshot`) is the only caller, so a
//! normal session never reaches this module and never builds this state.
//! Nothing here decides when a run ends or what it exits with.

use crate::app::RigWerkApp;

impl RigWerkApp {
    /// Test hook for a filled Match paths sheet in screenshots:
    /// `RIGWERK_MATCH_PATHS=1` shows the result a measurement would produce.
    /// Nothing is applied and no bridge runs.
    pub(super) fn screenshot_match_paths(&mut self) {
        use crate::app::match_paths::{MatchPathRow, MatchPathsResult};
        let target = crate::app::match_paths::MATCH_PATHS_TARGET_DEFAULT_LUFS;
        let readings = [-20.5, -16.0, -22.0, -14.5];
        let rows = self
            .path_slots()
            .into_iter()
            .enumerate()
            .map(|(index, slot)| {
                let current_db = self.paths[slot]
                    .as_ref()
                    .map_or(0.0, |path| path.mix.match_db);
                let measured_lufs = readings[index.min(readings.len() - 1)];
                let new_db = (target - measured_lufs).clamp(
                    -crate::audio::PATH_MATCH_MAX_DB,
                    crate::audio::PATH_MATCH_MAX_DB,
                );
                MatchPathRow {
                    slot,
                    measured_lufs,
                    blocks: 40,
                    current_db,
                    new_db,
                    change_db: new_db - current_db,
                    reason: None,
                    ticked: true,
                    clamped: false,
                }
            })
            .collect();
        self.match_paths_result = Some(MatchPathsResult {
            rows,
            reference_lufs: target,
            reference_path: None,
        });
        self.match_paths_ticks =
            vec![true; self.match_paths_result.as_ref().map_or(0, |r| r.rows.len())];
        self.match_paths_status = None;
    }

    /// Test hook for screenshots of Signal Routing:
    /// `RIGWERK_ROUTING_SETUP=drywet` applies the Dry L / Wet R layout;
    /// `sum3` adds a third path (a copy of the first, playing) on top of it,
    /// routed L/R on the clock output, so two paths add up there.
    /// `blend` applies Blend clean with 128 samples of (fake) plugin
    /// latency on the driven path, aligned; `dualamp-off` applies Dual-amp
    /// with 64 samples on the second amp and the clock output's alignment
    /// off (the red warning); `live3` is `blend` plus an empty third path
    /// (unmuted, routed nowhere: an issues warning), for the console.
    /// `fx` is `blend` without the latency plus an effects return both
    /// paths send to, selected.
    /// `showcase` is the README setup: see [`Self::apply_screenshot_showcase`].
    /// `showcase4` adds two paths, every other sink as an extra output and
    /// a long chain (board 6): see [`Self::apply_screenshot_four_paths`].
    pub(super) fn apply_screenshot_routing(&mut self) {
        use crate::app::layouts::PathLayout;
        let setup = std::env::var("RIGWERK_ROUTING_SETUP").unwrap_or_default();
        match setup.as_str() {
            "drywet" | "sum3" => {
                if !self.apply_layout(PathLayout::DryWet) {
                    return;
                }
                if setup == "sum3" {
                    let first = self.path_slots()[0];
                    if let Some(copy) = self.duplicate_path(first) {
                        self.set_path_mute(copy, false);
                        self.clock_output.path_map[copy].map = crate::audio::ChannelMap::default();
                        self.select_path(first);
                    }
                }
            }
            "fx" => {
                // Blend clean plus an effects return both paths send to,
                // selected so a path sheet opens on it.
                if !self.apply_layout(PathLayout::Blend) {
                    return;
                }
                let slots = self.path_slots();
                self.rename_path(slots[0], "Clean");
                self.rename_path(slots[1], "Drive");
                if let Some(fx) = self.add_return() {
                    self.set_path_mute(fx, false);
                    self.clock_output.path_map[fx] = crate::audio::PathMapSettings::default();
                    self.set_path_send(slots[0], fx, -12.0);
                    self.set_path_send(slots[1], fx, -18.0);
                    self.push_paths_to_engine();
                    self.push_devices_to_engine();
                    self.select_path(fx);
                }
            }
            "blend" | "live3" => {
                if !self.apply_layout(PathLayout::Blend) {
                    return;
                }
                let slots = self.path_slots();
                self.rename_path(slots[0], "Clean");
                self.rename_path(slots[1], "Drive");
                self.engine.screenshot_path_latency(slots[1], 128);
                if setup == "live3" {
                    if let Some(added) = self.add_path() {
                        self.rename_path(added, "Stream");
                        // Unmuted: routed nowhere, so the issues drawer warns.
                        self.set_path_mute(added, false);
                    }
                    self.select_path(slots[0]);
                }
            }
            "showcase" => self.apply_screenshot_showcase(),
            "showcase4" => {
                self.apply_screenshot_showcase();
                self.apply_screenshot_four_paths();
            }
            "dualamp-off" => {
                if !self.apply_layout(PathLayout::DualAmp) {
                    return;
                }
                let slots = self.path_slots();
                self.rename_path(slots[0], "Amp A");
                self.rename_path(slots[1], "Amp B");
                self.engine.screenshot_path_latency(slots[1], 64);
                self.set_device_align(0, false);
            }
            _ => {}
        }
    }

    /// Board 6 on top of the showcase: Drive (a copy of Room, playing) and
    /// an empty Stream path, every sink not yet in use as an extra output,
    /// and ten stand-in effects on Drive so its list scrolls.
    fn apply_screenshot_four_paths(&mut self) {
        let slots = self.path_slots();
        let Some(&room) = slots.get(1) else {
            return;
        };
        if let Some(drive) = self.duplicate_path(room) {
            self.rename_path(drive, "Drive");
            self.set_path_mute(drive, false);
            // Ten copies of the room's own effect, bypassed, so the chain
            // scrolls without ten reverbs running at once.
            let template =
                self.engine
                    .plugin_views(room)
                    .first()
                    .map(|view| crate::audio::PluginPref {
                        path: view.path.clone(),
                        format: view.format,
                        class_id: view.class_id.clone(),
                        name: view.name.clone(),
                        bypass: true,
                        ..crate::audio::PluginPref::default()
                    });
            for _ in 0..10 {
                match &template {
                    Some(template) => {
                        let _ = self.engine.add_plugin(drive, template.clone());
                    }
                    None => self.add_screenshot_effect_always(drive),
                }
            }
        }
        if let Some(stream) = self.add_path() {
            self.rename_path(stream, "Stream");
        }
        let used: Vec<String> = std::iter::once(self.selected_output().id.clone())
            .chain(self.extra_outputs.iter().map(|extra| extra.id.clone()))
            .collect();
        let more: Vec<String> = self
            .devices
            .inventory
            .outputs
            .iter()
            .map(|device| device.id.clone())
            .filter(|id| !used.contains(id) && !id.starts_with("vbcable:"))
            .collect();
        for id in more {
            self.add_extra_output_device(&id);
        }
        self.select_path(room);
    }

    /// The README setup, the same on every page: a second output sink as a
    /// extra output plus RigWerk Out, Blend clean (Clean and Room), and the
    /// effects in `RIGWERK_SCREENSHOT_PLUGINS`, a `;` list of
    /// `<path number>=<bundle>[#<effect name>]` (path 1 is Clean).  Room
    /// stays selected.  A chain without plugin latency gets none faked.
    /// The plugin library is scanned too, so the Plugins page has a list.
    /// The metronome runs at 96 BPM in 4/4, clicking on the extra output only,
    /// so the console and the output sheet show it in use.
    fn apply_screenshot_showcase(&mut self) {
        self.begin_plugin_scan();
        let extra_output = self
            .devices
            .inventory
            .outputs
            .iter()
            .map(|device| device.id.clone())
            .find(|id| *id != self.selected_output().id && !id.starts_with("vbcable:"));
        if let Some(id) = extra_output {
            self.add_extra_output_device(&id);
            // Device 1 is that extra output: the click goes to the headphones.
            self.set_click_channel(1, 0, true);
            self.set_click_channel(1, 1, true);
        }
        self.set_metronome_bpm(96.0);
        self.set_metronome_rhythm(crate::audio::ClickRhythm::FourFour);
        self.set_metronome_running(true);
        self.add_extra_output_device(crate::audio::VIRTUAL_OUTPUT_ID);
        if !self.apply_layout(crate::app::layouts::PathLayout::Blend) {
            return;
        }
        let slots = self.path_slots();
        self.rename_path(slots[0], "Clean");
        self.rename_path(slots[1], "Room");
        let list = std::env::var("RIGWERK_SCREENSHOT_PLUGINS").unwrap_or_default();
        for entry in list
            .split(';')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
        {
            let Some((number, bundle)) = entry.split_once('=') else {
                eprintln!("Screenshot plugin entry without a path number: {entry}");
                continue;
            };
            let Some(&slot) = number
                .trim()
                .parse::<usize>()
                .ok()
                .and_then(|number| slots.get(number.wrapping_sub(1)))
            else {
                eprintln!("Screenshot plugin entry for a path that is not there: {entry}");
                continue;
            };
            let (bundle, name) = match bundle.split_once('#') {
                Some((bundle, name)) => (bundle, Some(name.trim())),
                None => (bundle, None),
            };
            self.select_path(slot);
            if !self.add_plugin_from_bundle(std::path::Path::new(bundle.trim()), name) {
                eprintln!("Screenshot plugin not loaded: {entry}");
            }
        }
        self.apply_screenshot_nam_and_cab(&slots);
        self.select_path(slots[1]);
    }

    /// `RIGWERK_SCREENSHOT_NAM`, a `;` list of `<path number>=<file.nam>`
    /// (one block each, in order, before the effects), and
    /// `RIGWERK_SCREENSHOT_CAB`, the same list with one `.wav` per path.
    /// The folder of the last file of each becomes the sheet's folder.
    fn apply_screenshot_nam_and_cab(&mut self, slots: &[usize]) {
        for (number, file) in super::screenshot_files("RIGWERK_SCREENSHOT_NAM") {
            let Some(&slot) = slots.get(number.wrapping_sub(1)) else {
                eprintln!("Screenshot NAM entry for a path that is not there: {number}");
                continue;
            };
            let Some(index) = self.add_nam_block(slot) else {
                continue;
            };
            if let Some(folder) = std::path::Path::new(&file).parent() {
                self.set_models_folder(folder.display().to_string());
            }
            self.set_nam_model(slot, index, file);
        }
        for (number, file) in super::screenshot_files("RIGWERK_SCREENSHOT_CAB") {
            let Some(&slot) = slots.get(number.wrapping_sub(1)) else {
                eprintln!("Screenshot cab entry for a path that is not there: {number}");
                continue;
            };
            if let Some(folder) = std::path::Path::new(&file).parent() {
                self.set_cab_folder(folder.display().to_string());
            }
            self.set_path_cab_file(slot, file);
            self.set_stage_on(slot, crate::app::chain::ChainStage::Cab, true);
        }
    }
}
