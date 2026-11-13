//! Output devices under the master Output stage: their rows in the chain, the
//! device list in the master inspector and the per-device inspectors.
//!
//! PipeWire clock device: followers and RigWerk Out can be added live (in-graph).
//! ALSA / other cpal clock: generic followers on their own streams (step 3).

mod rows;

pub(crate) use rows::block;
use rows::{
    chain_device_row, click_mask_block, device_level_block, list_device_row, mute_button,
    title_name_width, ClickEdit,
};

use eframe::egui;
use egui::{Align, Color32, CornerRadius, FontFamily, FontId, Layout, RichText, Stroke, Vec2};

use crate::app::chain::ChainSelection;
use crate::app::output_devices::OutputDeviceRow;
use crate::app::settings::{
    cushion_from_pref, cushion_to_pref, rate_label, FollowerPref, CUSHION_MS_MAX, CUSHION_MS_MIN,
};
use crate::app::RigWerkApp;
use crate::audio::{
    CushionMode, EngineState, OutputDeviceSettings, DEVICE_LEVEL_MAX_DB, DEVICE_LEVEL_MIN_DB,
    MAX_FOLLOWERS, SAFETY_GR_DB, VIRTUAL_OUT_ID, VIRTUAL_OUT_NAME,
};
use crate::ui::pages::mixer::rack::RackAction;
use crate::ui::theme::ts;
use crate::ui::theme::{with_alpha, AMBER, BLUE, BORDER_SOFT, DIM, GREEN, MUTED, RED, TEXT};
use crate::ui::widgets::{
    clock_tag, db_slider, fit_text, meter_row, micro_label, mono, small_fact, tag_width,
    virtual_tag,
};

/// At the follower cap.
pub(crate) const ADD_DEVICE_HINT_FULL: &str = "Four followers is the maximum for now";
/// Soft warn when adding a WASAPI follower while the clock is ASIO.
pub(crate) const ASIO_WASAPI_CONFLICT_HINT: &str =
    "ASIO may have locked this card — Windows can refuse the stream as in use";

/// Gain reduction as the rows show it, once it is past the point where
/// Live Issues also says the device is against the ceiling.
pub(crate) fn gain_reduction_text(reduction_db: f32) -> Option<String> {
    (reduction_db.is_finite() && reduction_db >= SAFETY_GR_DB)
        .then(|| format!("{reduction_db:.1} dB"))
}

/// The device level as the rows show it.
pub(crate) fn device_level_text(settings: &OutputDeviceSettings) -> String {
    if settings.mute {
        "muted".to_owned()
    } else {
        format!("{:+.1} dB", settings.level_db)
    }
}

/// A device's state word and colour.
pub(crate) fn device_state(row: &OutputDeviceRow, state: EngineState) -> (&'static str, Color32) {
    if row.missing {
        return ("missing", RED);
    }
    if row.virtual_out && !row.publish {
        return ("not published", DIM);
    }
    match state {
        EngineState::Running => ("running", GREEN),
        EngineState::Starting => ("starting", AMBER),
        EngineState::Preview => ("preview", AMBER),
        EngineState::Error => ("error", RED),
        EngineState::Idle | EngineState::Stopping => ("stopped", DIM),
    }
}

/// What a screen reader hears after a row's name.
pub(crate) fn device_row_value(row: &OutputDeviceRow, state: EngineState) -> String {
    let role = if row.clock {
        "clock device"
    } else if row.virtual_out {
        "virtual"
    } else {
        "follower"
    };
    let level = device_level_text(&row.settings);
    let level = match gain_reduction_text(row.reduction_db) {
        Some(reduction) => format!("{level} · {reduction}"),
        None => level,
    };
    format!("{role} · {level} · {}", device_state(row, state).0)
}

impl RigWerkApp {
    /// Whether the clock device is a native PipeWire node (in-graph followers
    /// and RigWerk Out).
    pub(crate) fn pipewire_followers_enabled(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            crate::audio::is_native_id(&self.selected_output().id)
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }

    /// Whether followers can be added for the current clock.
    pub(crate) fn followers_enabled(&self) -> bool {
        cfg!(any(target_os = "linux", target_os = "windows"))
    }

    /// Why add is disabled, or `None` when it is enabled.
    pub(crate) fn add_output_disabled_reason(&self) -> Option<&'static str> {
        if !self.followers_enabled() {
            Some("Extra output devices are not available on this platform")
        } else if self.followers.len() >= MAX_FOLLOWERS {
            Some(ADD_DEVICE_HINT_FULL)
        } else {
            None
        }
    }

    /// Clock device is an ASIO driver (followers are still WASAPI-only).
    pub(crate) fn clock_is_asio(&self) -> bool {
        self.selected_output().asio_clsid.is_some()
    }

    /// The device rows under the Output stage in the chain list, and the
    /// add button (enabled when the clock is PipeWire).
    pub(super) fn output_device_children(
        &mut self,
        ui: &mut egui::Ui,
        rows: &[OutputDeviceRow],
        select: &mut Option<ChainSelection>,
    ) {
        let state = self.engine.state;
        let mut remove: Option<usize> = None;
        for (index, row) in rows.iter().enumerate() {
            let selected = self.chain_selected == ChainSelection::OutputDevice(index);
            let (clicked, toggle_mute, remove_clicked) = chain_device_row(ui, row, selected, state);
            if toggle_mute {
                if row.clock {
                    self.clock_device.mute = !self.clock_device.mute;
                } else if let Some(follower) = row.follower_index {
                    if let Some(pref) = self.followers.get_mut(follower) {
                        pref.settings.mute = !pref.settings.mute;
                    }
                }
            }
            if remove_clicked {
                if let Some(follower) = row.follower_index {
                    remove = Some(follower);
                }
            }
            if clicked {
                *select = Some(ChainSelection::OutputDevice(index));
            }
        }
        if let Some(follower) = remove {
            self.remove_follower(follower);
            *select = Some(ChainSelection::Output);
        }
        ui.add_space(4.0);
        let add_selected = self.chain_selected == ChainSelection::AddOutput;
        let disabled = self.add_output_disabled_reason();
        let add = ui.add_enabled(
            disabled.is_none(),
            egui::Button::new(
                RichText::new("+ add output device")
                    .size(ts(10.0))
                    .color(if add_selected { BLUE } else { DIM }),
            )
            .fill(if add_selected {
                with_alpha(BLUE, 14)
            } else {
                Color32::TRANSPARENT
            })
            .stroke(Stroke::new(
                1.0,
                if add_selected {
                    with_alpha(BLUE, 45)
                } else {
                    BORDER_SOFT
                },
            ))
            .corner_radius(CornerRadius::same(8))
            .min_size(Vec2::new(ui.available_width(), 30.0)),
        );
        if let Some(reason) = disabled {
            add.on_disabled_hover_text(reason);
        } else if add.clicked() {
            *select = Some(ChainSelection::AddOutput);
        }
    }

    /// Removes a follower live; the bridge keeps running.
    pub(crate) fn remove_follower(&mut self, index: usize) {
        if index < self.followers.len() {
            self.followers.remove(index);
            self.chain_selected = self
                .chain_selected
                .clamp_output(self.output_device_rows().len());
        }
    }

    /// Adds a hardware follower by inventory index, or RigWerk Out.
    pub(crate) fn add_follower_device(&mut self, device_id: &str) {
        if self.add_output_disabled_reason().is_some() {
            return;
        }
        if device_id == VIRTUAL_OUT_ID || device_id.starts_with("vbcable:") {
            if self.followers.iter().any(|f| f.virtual_out) {
                return;
            }
            #[cfg(target_os = "linux")]
            {
                if !self.pipewire_followers_enabled() {
                    return;
                }
                let mut follower = FollowerPref::virtual_out();
                // A new device plays what the clock device plays, path by
                // path, instead of every path summed on its default map.
                follower.settings.path_map = self.clock_device.path_map;
                self.followers.push(follower);
            }
            #[cfg(target_os = "windows")]
            {
                let Some(cable) = self
                    .inventory
                    .outputs
                    .iter()
                    .find(|device| crate::audio::is_vb_cable_render(&device.name))
                    .cloned()
                else {
                    return;
                };
                self.followers.push(FollowerPref {
                    id: cable.id,
                    backend: cable.backend,
                    name: VIRTUAL_OUT_NAME.to_owned(),
                    virtual_out: true,
                    publish: true,
                    // Plays what the clock device plays, path by path.
                    settings: crate::audio::OutputDeviceSettings {
                        path_map: self.clock_device.path_map,
                        ..Default::default()
                    },
                    ..FollowerPref::default()
                });
            }
            #[cfg(not(any(target_os = "linux", target_os = "windows")))]
            {
                let _ = device_id;
                return;
            }
        } else {
            if self.followers.iter().any(|f| f.id == device_id)
                || self.selected_output().id == device_id
            {
                return;
            }
            let Some(device) = self
                .inventory
                .outputs
                .iter()
                .find(|device| device.id == device_id)
                .cloned()
            else {
                return;
            };
            #[cfg(target_os = "linux")]
            {
                let pw_clock = self.pipewire_followers_enabled();
                let is_pw = crate::audio::linux_node_name(&device.id).is_some();
                // PipeWire clock: only PW sinks (in-graph).  ALSA/cpal clock:
                // only generic (non-native) devices on their own streams.
                if pw_clock != is_pw {
                    return;
                }
            }
            #[cfg(target_os = "windows")]
            {
                // WASAPI shared-mode followers only (no ASIO / DS / MME).
                if device.asio_clsid.is_some() || !device.backend.eq_ignore_ascii_case("WASAPI") {
                    return;
                }
            }
            let settings = crate::audio::OutputDeviceSettings {
                path_map: self.clock_device.path_map,
                ..Default::default()
            };
            self.followers.push(FollowerPref {
                id: device.id,
                backend: device.backend,
                name: device.name,
                settings,
                ..FollowerPref::default()
            });
        }
        let index = self.followers.len(); // clock + this follower
        self.chain_selected = ChainSelection::OutputDevice(index);
    }

    /// The "Output devices" block of the master inspector.
    pub(super) fn output_devices_block(
        &mut self,
        ui: &mut egui::Ui,
        rows: &[OutputDeviceRow],
        select: &mut Option<ChainSelection>,
    ) {
        let state = self.engine.state;
        let disabled = self.add_output_disabled_reason();
        block(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(micro_label("OUTPUT DEVICES", BLUE));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let add = ui.add_enabled(
                        disabled.is_none(),
                        egui::Button::new(
                            RichText::new("+ Add output device")
                                .size(ts(9.5))
                                .color(BLUE),
                        )
                        .fill(with_alpha(BLUE, 14))
                        .stroke(Stroke::new(1.0, with_alpha(BLUE, 45)))
                        .corner_radius(CornerRadius::same(7)),
                    );
                    if let Some(reason) = disabled {
                        add.on_disabled_hover_text(reason);
                    } else if add.clicked() {
                        *select = Some(ChainSelection::AddOutput);
                    }
                });
            });
            ui.add_space(6.0);
            for (index, row) in rows.iter().enumerate() {
                let selected = self.chain_selected == ChainSelection::OutputDevice(index);
                if list_device_row(ui, row, selected, state) {
                    *select = Some(ChainSelection::OutputDevice(index));
                }
            }
            ui.add_space(4.0);
            ui.label(
                RichText::new(
                    "The clock device drives the chain: plugins, gate and EQ run on its callback.",
                )
                .size(ts(10.5))
                .color(MUTED),
            );
        });
    }

    /// Inspector dispatcher for one device row (clock, follower or virtual).
    pub(super) fn output_device_inspector(
        &mut self,
        ui: &mut egui::Ui,
        row: &OutputDeviceRow,
        actions: &mut Vec<RackAction>,
    ) {
        if row.clock {
            self.clock_device_inspector(ui, row, actions);
        } else if row.virtual_out {
            self.virtual_device_inspector(ui, row);
        } else {
            self.follower_device_inspector(ui, row);
        }
    }

    /// Add-output list: PipeWire / ALSA on Linux, WASAPI (+ VB-Cable) on Windows.
    pub(super) fn add_output_inspector(&mut self, ui: &mut egui::Ui) {
        let clock_name = self.selected_output().name.clone();
        let pw_clock = self.pipewire_followers_enabled();
        let asio_clock = self.clock_is_asio();
        ui.label(
            RichText::new("Add output device")
                .size(ts(16.0))
                .color(TEXT)
                .strong(),
        );
        ui.label(
            RichText::new(format!(
                "New devices join as followers of {clock_name}. Adding one is live."
            ))
            .size(ts(11.0))
            .color(DIM),
        );
        ui.add_space(12.0);
        if let Some(reason) = self.add_output_disabled_reason() {
            ui.label(RichText::new(reason).size(ts(11.0)).color(AMBER));
            return;
        }
        if asio_clock {
            ui.label(
                RichText::new(ASIO_WASAPI_CONFLICT_HINT)
                    .size(ts(11.0))
                    .color(AMBER),
            );
            ui.add_space(8.0);
        }
        let clock_id = self.selected_output().id.clone();
        let used: std::collections::HashSet<String> = self
            .followers
            .iter()
            .map(|f| f.id.clone())
            .chain(std::iter::once(clock_id.clone()))
            .collect();
        let has_virtual = self.followers.iter().any(|f| f.virtual_out);
        let vb_cable = self
            .inventory
            .outputs
            .iter()
            .find(|device| crate::audio::is_vb_cable_render(&device.name))
            .cloned();
        let mut candidates: Vec<(String, String, String, bool)> = {
            #[cfg(target_os = "linux")]
            {
                self.inventory
                    .outputs
                    .iter()
                    .filter_map(|device| {
                        let is_pw = crate::audio::linux_node_name(&device.id).is_some();
                        if pw_clock != is_pw {
                            return None;
                        }
                        Some((
                            device.id.clone(),
                            device.name.clone(),
                            format!("{} · {} CH", device.backend.to_uppercase(), device.channels),
                            used.contains(&device.id),
                        ))
                    })
                    .collect()
            }
            #[cfg(target_os = "windows")]
            {
                self.inventory
                    .outputs
                    .iter()
                    .filter_map(|device| {
                        if device.asio_clsid.is_some()
                            || !device.backend.eq_ignore_ascii_case("WASAPI")
                        {
                            return None;
                        }
                        // VB-Cable is offered under VIRTUAL, not as a plain follower.
                        if crate::audio::is_vb_cable_render(&device.name) {
                            return None;
                        }
                        Some((
                            device.id.clone(),
                            device.name.clone(),
                            format!("{} · {} CH", device.backend.to_uppercase(), device.channels),
                            used.contains(&device.id),
                        ))
                    })
                    .collect()
            }
            #[cfg(not(any(target_os = "linux", target_os = "windows")))]
            {
                let _ = (&used, pw_clock);
                Vec::new()
            }
        };
        candidates.sort_by_key(|a| a.1.to_lowercase());

        block(ui, |ui| {
            let section = if pw_clock {
                "PIPEWIRE"
            } else if cfg!(target_os = "windows") {
                "WASAPI"
            } else {
                "DEVICES"
            };
            ui.label(micro_label(section, BLUE));
            ui.add_space(4.0);
            let mut picked = None;
            for (id, name, detail, disabled) in &candidates {
                let response = ui.add_enabled(
                    !disabled,
                    egui::Button::new(
                        RichText::new(format!("{name}\n{detail}"))
                            .size(ts(11.0))
                            .color(TEXT),
                    )
                    .fill(with_alpha(TEXT, 4))
                    .stroke(Stroke::new(1.0, BORDER_SOFT))
                    .corner_radius(CornerRadius::same(7))
                    .min_size(Vec2::new(ui.available_width(), 40.0)),
                );
                if asio_clock && !disabled {
                    response.clone().on_hover_text(ASIO_WASAPI_CONFLICT_HINT);
                }
                if response.clicked() {
                    picked = Some(id.clone());
                }
                ui.add_space(2.0);
            }
            if let Some(id) = picked {
                self.add_follower_device(&id);
            }
            if pw_clock {
                ui.add_space(8.0);
                ui.label(micro_label("VIRTUAL", GREEN));
                ui.add_space(4.0);
                let virt = ui.add_enabled(
                    !has_virtual,
                    egui::Button::new(
                        RichText::new(format!(
                            "{VIRTUAL_OUT_NAME}\nPIPEWIRE SOURCE · created by RigWerk"
                        ))
                        .size(ts(11.0))
                        .color(TEXT),
                    )
                    .fill(with_alpha(GREEN, 10))
                    .stroke(Stroke::new(1.0, with_alpha(GREEN, 45)))
                    .corner_radius(CornerRadius::same(7))
                    .min_size(Vec2::new(ui.available_width(), 40.0)),
                );
                if has_virtual {
                    virt.on_disabled_hover_text("already added");
                } else if virt.clicked() {
                    self.add_follower_device(VIRTUAL_OUT_ID);
                }
            }
            #[cfg(target_os = "windows")]
            {
                ui.add_space(8.0);
                ui.label(micro_label("PUBLISH", GREEN));
                ui.add_space(4.0);
                let has_cable = vb_cable.is_some();
                let virt = ui.add_enabled(
                    has_cable && !has_virtual,
                    egui::Button::new(
                        RichText::new(if has_cable {
                            format!(
                                "{VIRTUAL_OUT_NAME}\nVB-CABLE · other apps capture CABLE Output"
                            )
                        } else {
                            format!("{VIRTUAL_OUT_NAME}\nInstall VB-Audio Cable to publish")
                        })
                        .size(ts(11.0))
                        .color(TEXT),
                    )
                    .fill(with_alpha(GREEN, 10))
                    .stroke(Stroke::new(1.0, with_alpha(GREEN, 45)))
                    .corner_radius(CornerRadius::same(7))
                    .min_size(Vec2::new(ui.available_width(), 40.0)),
                );
                if has_virtual {
                    virt.on_disabled_hover_text("already added");
                } else if !has_cable {
                    virt.on_disabled_hover_text(
                        "Install VB-Audio Virtual Cable, then refresh devices.",
                    );
                } else if virt.clicked() {
                    self.add_follower_device(VIRTUAL_OUT_ID);
                }
            }
            #[cfg(not(target_os = "windows"))]
            {
                let _ = vb_cable;
            }
        });
    }

    /// Follower inspector: level, mute, remove; PW health or generic cushion.
    pub(super) fn follower_device_inspector(&mut self, ui: &mut egui::Ui, row: &OutputDeviceRow) {
        let Some(follower) = row.follower_index else {
            return;
        };
        let pipewire = self.pipewire_followers_enabled();
        ui.horizontal(|ui| {
            ui.label(RichText::new(&row.name).size(ts(16.0)).color(TEXT).strong());
            ui.label(
                RichText::new("FOLLOWS")
                    .size(ts(9.0))
                    .color(MUTED)
                    .family(FontFamily::Monospace),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if let Some(pref) = self.followers.get_mut(follower) {
                    if mute_button(ui, pref.settings.mute, &row.name).clicked() {
                        pref.settings.mute = !pref.settings.mute;
                    }
                }
            });
        });
        ui.label(
            RichText::new(if pipewire {
                "Follower · plays the finished mix. Handled by PipeWire."
            } else {
                "Follower · plays the finished mix on its own clock."
            })
            .size(ts(11.0))
            .color(DIM),
        );
        if row.missing {
            ui.add_space(8.0);
            ui.label(
                RichText::new(
                    "Device not found. The bridge runs without it; it stays in the session.",
                )
                .size(ts(11.0))
                .color(RED),
            );
        }
        ui.add_space(12.0);
        if pipewire {
            block(ui, |ui| {
                ui.label(micro_label("HEALTH", BLUE));
                ui.add_space(4.0);
                ui.label(
                    RichText::new("Handled by PipeWire")
                        .size(ts(12.0))
                        .color(GREEN),
                );
                ui.label(
                    RichText::new("Same graph clock as the clock device · no ring or cushion.")
                        .size(ts(10.5))
                        .color(MUTED),
                );
            });
        } else {
            self.follower_cushion_block(ui, follower);
        }
        ui.add_space(10.0);
        if let Some(pref) = self.followers.get_mut(follower) {
            device_level_block(
                ui,
                &mut pref.settings,
                row.channels,
                &self.meter_follower_channels[follower],
                &self.peak_follower_channels[follower],
                row.reduction_db,
            );
            ui.add_space(10.0);
            self.click_block(ui, follower + 1, row.click_channels);
        }
        ui.add_space(10.0);
        block(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Removes the device from the session. The others keep playing.")
                        .size(ts(10.5))
                        .color(MUTED),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .add(
                            egui::Button::new(RichText::new("Remove").size(ts(10.0)).color(RED))
                                .fill(with_alpha(RED, 18))
                                .stroke(Stroke::new(1.0, with_alpha(RED, 55)))
                                .corner_radius(CornerRadius::same(7)),
                        )
                        .clicked()
                    {
                        self.remove_follower(follower);
                    }
                });
            });
        });
    }

    /// The CLICK block of output device `device` (0 = clock, then
    /// followers).  The app applies the press.
    fn click_block(&mut self, ui: &mut egui::Ui, device: usize, channels: usize) {
        let Some(mask) = self
            .device_settings()
            .get(device)
            .map(|settings| settings.click_mask)
        else {
            return;
        };
        match click_mask_block(ui, mask, channels) {
            Some(ClickEdit::Pair) => self.apply_click_pair(device),
            Some(ClickEdit::Channel { channel, on }) => {
                self.set_click_channel(device, channel, on);
            }
            None => {}
        }
    }

    /// Cushion controls for a generic (non-PipeWire) follower.
    fn follower_cushion_block(&mut self, ui: &mut egui::Ui, follower: usize) {
        let Some(pref) = self.followers.get_mut(follower) else {
            return;
        };
        let mut mode = cushion_from_pref(pref.cushion_mode);
        block(ui, |ui| {
            ui.label(micro_label("CUSHION", BLUE));
            ui.add_space(4.0);
            ui.label(
                RichText::new("Own ring between the mix and this device. Absorb clock drift here.")
                    .size(ts(10.5))
                    .color(MUTED),
            );
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                for (option, label) in [
                    (CushionMode::Safe, "Safe"),
                    (CushionMode::LowLatency, "Low latency"),
                    (CushionMode::Manual, "Manual"),
                ] {
                    if ui
                        .selectable_label(mode == option, RichText::new(label).size(ts(10.0)))
                        .clicked()
                    {
                        mode = option;
                    }
                }
            });
            if mode != cushion_from_pref(pref.cushion_mode) {
                pref.cushion_mode = cushion_to_pref(mode);
            }
            if mode == CushionMode::Manual {
                ui.add_space(4.0);
                let mut ms = pref.cushion_ms;
                let response = ui.add(
                    egui::Slider::new(&mut ms, CUSHION_MS_MIN..=CUSHION_MS_MAX)
                        .suffix(" ms")
                        .show_value(true),
                );
                if response.changed() {
                    pref.cushion_ms = ms;
                }
            }
        });
    }

    /// RigWerk Out inspector: publish toggle, level, remove.
    pub(super) fn virtual_device_inspector(&mut self, ui: &mut egui::Ui, row: &OutputDeviceRow) {
        let Some(follower) = row.follower_index else {
            return;
        };
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(VIRTUAL_OUT_NAME)
                    .size(ts(16.0))
                    .color(TEXT)
                    .strong(),
            );
            virtual_tag(ui);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if let Some(pref) = self.followers.get_mut(follower) {
                    let label = if pref.publish { "PUBLISHED" } else { "OFF" };
                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new(label).size(ts(9.0)).color(if pref.publish {
                                    GREEN
                                } else {
                                    DIM
                                }),
                            )
                            .fill(if pref.publish {
                                with_alpha(GREEN, 18)
                            } else {
                                Color32::TRANSPARENT
                            })
                            .stroke(Stroke::new(
                                1.0,
                                if pref.publish {
                                    with_alpha(GREEN, 55)
                                } else {
                                    BORDER_SOFT
                                },
                            ))
                            .corner_radius(CornerRadius::same(6)),
                        )
                        .clicked()
                    {
                        pref.publish = !pref.publish;
                    }
                }
            });
        });
        let pipewire = self.pipewire_followers_enabled();
        ui.label(
            RichText::new(if pipewire {
                "Endpoint RigWerk creates · the finished mix for other apps to capture."
            } else {
                "Plays into VB-Cable Input · other apps capture CABLE Output."
            })
            .size(ts(11.0))
            .color(DIM),
        );
        ui.add_space(12.0);
        block(ui, |ui| {
            ui.label(micro_label("ENDPOINT", BLUE));
            ui.add_space(4.0);
            let published = self.followers.get(follower).is_some_and(|f| f.publish);
            ui.label(
                RichText::new(if pipewire {
                    if published {
                        "Published as a PipeWire source. Other apps see RigWerk Out while the bridge runs."
                    } else {
                        "Not publishing. Other apps can't capture the mix."
                    }
                } else if published {
                    "Playing into VB-Cable. Other apps should pick CABLE Output."
                } else {
                    "Publish is off. VB-Cable is silent until you turn it on."
                })
                .size(ts(11.0))
                .color(MUTED),
            );
            ui.label(mono(
                if pipewire {
                    "Audio/Source/Virtual · 2 CH"
                } else {
                    "WASAPI · VB-Audio Cable Input"
                },
                DIM,
                9.5,
            ));
        });
        ui.add_space(10.0);
        if pipewire {
            block(ui, |ui| {
                ui.label(micro_label("HEALTH", BLUE));
                ui.add_space(4.0);
                ui.label(
                    RichText::new("Handled by PipeWire")
                        .size(ts(12.0))
                        .color(GREEN),
                );
            });
        } else {
            self.follower_cushion_block(ui, follower);
        }
        ui.add_space(10.0);
        if let Some(pref) = self.followers.get_mut(follower) {
            device_level_block(
                ui,
                &mut pref.settings,
                row.channels.max(2),
                &self.meter_follower_channels[follower],
                &self.peak_follower_channels[follower],
                row.reduction_db,
            );
            ui.add_space(10.0);
            self.click_block(ui, follower + 1, row.click_channels);
        }
        ui.add_space(10.0);
        block(ui, |ui| {
            if ui
                .add(
                    egui::Button::new(RichText::new("Remove").size(ts(10.0)).color(RED))
                        .fill(with_alpha(RED, 18))
                        .stroke(Stroke::new(1.0, with_alpha(RED, 55)))
                        .corner_radius(CornerRadius::same(7)),
                )
                .clicked()
            {
                self.remove_follower(follower);
            }
        });
    }

    /// Inspector for the clock device: what it is, its clock, and its own
    /// level and mute after the master.
    pub(super) fn clock_device_inspector(
        &mut self,
        ui: &mut egui::Ui,
        row: &OutputDeviceRow,
        actions: &mut Vec<RackAction>,
    ) {
        ui.horizontal(|ui| {
            // Shorten the name so the CLOCK tag and MUTE always fit.
            let title_font = FontId::new(ts(16.0), FontFamily::Proportional);
            let mute_w = ui
                .painter()
                .layout_no_wrap(
                    "MUTED".to_owned(),
                    FontId::new(ts(9.0), FontFamily::Proportional),
                    TEXT,
                )
                .size()
                .x
                + ui.spacing().button_padding.x * 2.0;
            let name_w = title_name_width(
                ui.available_width(),
                tag_width(ui, "CLOCK"),
                mute_w,
                ui.spacing().item_spacing.x,
            );
            let name = fit_text(ui, &row.name, &title_font, name_w);
            ui.label(RichText::new(name).font(title_font).color(TEXT).strong())
                .on_hover_text(&row.name);
            clock_tag(ui);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if mute_button(ui, self.clock_device.mute, &row.name).clicked() {
                    self.clock_device.mute = !self.clock_device.mute;
                }
            });
        });
        ui.label(
            RichText::new("Clock device · the chain runs on this device's callback.")
                .size(ts(11.0))
                .color(DIM),
        );
        ui.add_space(12.0);

        let rate = if self.output_rate == 0 {
            "Match input".to_owned()
        } else {
            rate_label(self.output_rate)
        };
        let buffer = format!("{} smp", self.output_buffer);
        let step = match self.output_step() {
            Some((frames, ms)) => format!("{frames} smp · {ms:.1} ms"),
            None => "—".to_owned(),
        };
        block(ui, |ui| {
            ui.label(micro_label("DEVICE", BLUE));
            ui.add_space(4.0);
            ui.label(RichText::new(&row.name).size(ts(12.0)).color(TEXT));
            ui.label(mono(
                format!("{} · {} CH", row.backend.to_uppercase(), row.channels),
                DIM,
                9.5,
            ));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                small_fact(ui, "OUT RATE", &rate);
                ui.add_space(14.0);
                small_fact(ui, "BUFFER", &buffer);
                ui.add_space(14.0);
                small_fact(ui, "DRIVER STEP", &step);
            });
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new("Device, rate and buffer are set on Live Bridge.")
                        .size(ts(10.5))
                        .color(MUTED),
                );
                if ui
                    .add(
                        egui::Button::new(
                            RichText::new("Change on Live Bridge")
                                .size(ts(9.5))
                                .color(BLUE),
                        )
                        .fill(with_alpha(BLUE, 14))
                        .stroke(Stroke::new(1.0, with_alpha(BLUE, 45)))
                        .corner_radius(CornerRadius::same(7)),
                    )
                    .on_hover_text(
                        "Open Live Bridge, where the output device and its clock are picked",
                    )
                    .clicked()
                {
                    actions.push(RackAction::OpenLive);
                }
            });
        });
        ui.add_space(10.0);

        block(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(micro_label("DEVICE LEVEL", BLUE));
                let reduction = if self.engine.state == EngineState::Running {
                    self.snapshot.out_reduction_db
                } else {
                    0.0
                };
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(mono(
                        device_level_text(&self.clock_device),
                        if self.clock_device.mute { RED } else { TEXT },
                        12.0,
                    ));
                    if let Some(text) = gain_reduction_text(reduction) {
                        ui.label(mono(text, AMBER, 12.0));
                    }
                });
            });
            ui.add_space(4.0);
            db_slider(
                ui,
                &mut self.clock_device.level_db,
                DEVICE_LEVEL_MIN_DB,
                DEVICE_LEVEL_MAX_DB,
                (BLUE, "Device level"),
            );
            ui.add_space(6.0);
            for channel in 0..row.channels {
                meter_row(
                    ui,
                    &(channel + 1).to_string(),
                    self.meter_out_channels[channel],
                    self.peak_out_channels[channel],
                    BLUE,
                );
            }
            ui.add_space(4.0);
            ui.label(
                RichText::new("Trims this device only, after the master level.")
                    .size(ts(10.5))
                    .color(MUTED),
            );
        });
        ui.add_space(10.0);
        self.click_block(ui, 0, row.click_channels);
        ui.add_space(10.0);

        block(ui, |ui| {
            ui.label(
                RichText::new(
                    "The clock device can't be removed. To move the clock, pick a different output on Live Bridge.",
                )
                .size(ts(10.5))
                .color(MUTED),
            );
        });
    }
}
#[cfg(test)]
mod tests;
