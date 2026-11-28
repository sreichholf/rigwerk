//! The add column's device list.  A popover, and only devices this clock
//! can really open.

use eframe::egui;
use egui::{CornerRadius, RichText, Stroke, Vec2};

use crate::app::RigWerkApp;
use crate::audio::VIRTUAL_OUTPUT_NAME;
use crate::ui::fonts::Face;
use crate::ui::theme::{font, palette, Palette, TypeRole};
use crate::ui::widgets::row_info;

pub(crate) const ASIO_WASAPI_CONFLICT_HINT: &str =
    "ASIO may have locked this card — Windows can refuse the stream as in use";

impl RigWerkApp {
    pub(crate) fn add_output_list(&mut self, ui: &mut egui::Ui) {
        let colours = palette(ui.ctx());
        let clock_name = self.selected_output().name.clone();
        let pw_clock = self.pipewire_extra_outputs_enabled();
        let asio_clock = self.clock_is_asio();
        ui.set_max_width(380.0);
        ui.label(
            RichText::new("Add output")
                .font(font(Face::SansMedium, TypeRole::Title))
                .color(colours.text),
        );
        ui.label(
            RichText::new(format!(
                "New devices join as extra outputs of {clock_name}. Adding one is live."
            ))
            .font(font(Face::Sans, TypeRole::Secondary))
            .color(colours.text3),
        );
        ui.add_space(8.0);
        if let Some(reason) = self.add_output_disabled_reason() {
            ui.label(
                RichText::new(reason)
                    .font(font(Face::SansMedium, TypeRole::Body))
                    .color(colours.warn),
            );
            return;
        }
        if asio_clock {
            ui.label(
                RichText::new(ASIO_WASAPI_CONFLICT_HINT)
                    .font(font(Face::Sans, TypeRole::Secondary))
                    .color(colours.warn),
            );
            ui.add_space(6.0);
        }

        let clock_id = self.selected_output().id.clone();
        let mut used: std::collections::HashSet<String> = self
            .extra_outputs
            .iter()
            .map(|extra| extra.id.clone())
            .collect();
        used.insert(clock_id);
        let has_virtual = self.extra_outputs.iter().any(|extra| extra.virtual_output);
        #[cfg(target_os = "windows")]
        let vb_cable = self
            .devices
            .inventory
            .outputs
            .iter()
            .find(|device| crate::audio::is_vb_cable_render(&device.name))
            .cloned();
        let mut candidates: Vec<(String, String, String, bool)> = {
            #[cfg(target_os = "linux")]
            {
                self.devices
                    .inventory
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
                            format!("{} · {} ch", device.backend, device.channels),
                            used.contains(&device.id),
                        ))
                    })
                    .collect()
            }
            #[cfg(target_os = "windows")]
            {
                self.devices
                    .inventory
                    .outputs
                    .iter()
                    .filter_map(|device| {
                        if device.asio_clsid.is_some()
                            || !device.backend.eq_ignore_ascii_case("WASAPI")
                        {
                            return None;
                        }
                        // VB-Cable is under Virtual, not a plain extra.
                        if crate::audio::is_vb_cable_render(&device.name) {
                            return None;
                        }
                        Some((
                            device.id.clone(),
                            device.name.clone(),
                            format!("{} · {} ch", device.backend, device.channels),
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
        candidates.sort_by_cached_key(|candidate| candidate.1.to_lowercase());

        let section = if pw_clock {
            "PipeWire"
        } else if cfg!(target_os = "windows") {
            "WASAPI"
        } else {
            "Devices"
        };
        panel(ui, section, colours, |ui| {
            let mut picked = None;
            for (id, name, detail, already) in &candidates {
                let response = device_entry(ui, name, detail, *already, false, colours);
                if asio_clock && !already {
                    let _ = response.clone().on_hover_text(ASIO_WASAPI_CONFLICT_HINT);
                }
                if response.clicked() {
                    picked = Some(id.clone());
                }
            }
            if candidates.is_empty() {
                ui.label(
                    RichText::new("No device fits this clock yet.")
                        .font(font(Face::Sans, TypeRole::Secondary))
                        .color(colours.text3),
                );
            }
            if let Some(id) = picked {
                self.add_extra_output_device(&id);
            }
            #[cfg(target_os = "linux")]
            if pw_clock {
                ui.add_space(8.0);
                let response = device_entry(
                    ui,
                    VIRTUAL_OUTPUT_NAME,
                    "PipeWire source · created by RigWerk",
                    has_virtual,
                    true,
                    colours,
                );
                if has_virtual {
                    response.on_disabled_hover_text("Already added");
                } else if response.clicked() {
                    self.add_extra_output_device(crate::audio::VIRTUAL_OUTPUT_ID);
                }
            }
            #[cfg(target_os = "windows")]
            {
                ui.add_space(8.0);
                let has_cable = vb_cable.is_some();
                let detail = if has_cable {
                    "VB-Cable · other apps capture CABLE Output"
                } else {
                    "Install VB-Audio Cable to use it"
                };
                let response = device_entry(
                    ui,
                    VIRTUAL_OUTPUT_NAME,
                    detail,
                    has_virtual || !has_cable,
                    true,
                    colours,
                );
                if has_virtual {
                    response.on_disabled_hover_text("Already added");
                } else if !has_cable {
                    response.on_disabled_hover_text("Install VB-Audio Virtual Cable, then rescan.");
                } else if response.clicked() {
                    self.add_extra_output_device(crate::audio::VIRTUAL_OUTPUT_ID);
                }
            }
            #[cfg(not(any(target_os = "linux", target_os = "windows")))]
            {
                let _ = &has_virtual;
            }
        });
    }
}

fn panel(ui: &mut egui::Ui, title: &str, colours: &Palette, add: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(6.0);
    ui.label(
        RichText::new(title)
            .font(font(Face::SansMedium, TypeRole::GridHeader))
            .color(colours.text3),
    );
    ui.add_space(3.0);
    add(ui);
}

fn device_entry(
    ui: &mut egui::Ui,
    name: &str,
    detail: &str,
    disabled: bool,
    virtual_output: bool,
    colours: &Palette,
) -> egui::Response {
    let width = ui.available_width();
    let fill = if virtual_output {
        colours.accent_bg
    } else {
        colours.slot
    };
    let response = ui.add_enabled(
        !disabled,
        egui::Button::new(
            RichText::new(format!("{name}\n{detail}"))
                .font(font(Face::Sans, TypeRole::Body))
                .color(if disabled {
                    colours.text3
                } else {
                    colours.text
                }),
        )
        .fill(fill)
        .stroke(Stroke::new(
            1.0,
            if virtual_output {
                colours.accent
            } else {
                colours.line2
            },
        ))
        .corner_radius(CornerRadius::same(4))
        .min_size(Vec2::new(width, 44.0)),
    );
    row_info(
        &response,
        egui::Role::Button,
        !disabled,
        name,
        None,
        Some(detail.to_owned()),
    );
    if disabled {
        let _ = response.clone().on_disabled_hover_text("Already added");
    }
    response
}
