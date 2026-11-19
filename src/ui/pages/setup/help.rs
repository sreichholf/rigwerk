//! The guide for this platform, as collapsible sections, and About.
//!
//! The text is the old Guide page.  The step drawings are gone.  Windows
//! and Linux each keep the notes that page showed for that system.

use eframe::egui;
use egui::RichText;

use crate::app::RigWerkApp;
use crate::ui::fonts::Face;
use crate::ui::pages::setup::panel;
use crate::ui::theme::{font, palette, TypeRole};

impl RigWerkApp {
    pub(super) fn setup_help(&mut self, ui: &mut egui::Ui) {
        let windows = cfg!(target_os = "windows");
        let intro = format!(
            "RigWerk sits between the driver your interface insists on and the speakers you actually want to use. The input and the clock output keep their own rates.\n\n{} · {}",
            self.inventory.os.to_uppercase(),
            self.inventory.low_latency_label
        );
        panel(ui, "Help", None, None, |ui| {
            let colours = palette(ui.ctx());
            section(ui, "One app, two clocks", &intro);
            section(
                ui,
                "Choose an input",
                if windows {
                    "Pick the driver and the input channels carrying your guitar. ASIO, WASAPI, DirectSound and MME are all listed."
                } else {
                    "Pick the driver and the input channels carrying your guitar. PipeWire, direct ALSA and PulseAudio are listed. JACK appears only when that host is available."
                },
            );
            section(
                ui,
                "Choose a clock output",
                "Speakers, headphones, an interface or a virtual cable. It does not need to be the same driver or the same protocol. Exactly one output is the clock output.",
            );
            section(
                ui,
                "Start the bridge",
                "The bridge buffer absorbs drift while the input and the clock output keep their own rates.",
            );
            if windows {
                section(
                    ui,
                    "Windows",
                    "The ASIO driver scan reads the Windows registry.\n\
                     ASIO can be the input, the clock output, or both.\n\
                     WASAPI extra outputs and the VB-Cable virtual output sit beside the clock output.",
                );
                section(
                    ui,
                    "Latency",
                    "On a WASAPI clock output, pick Shared (the buffer size you set), \
                     Shared, smallest buffer (the smallest shared buffer, and other apps keep playing), \
                     or Exclusive (this device alone, often a smaller buffer than Shared, \
                     and other apps go silent on that device). \
                     ASIO, DirectSound and MME have no output mode. \
                     ASIO uses the driver's own buffer size.\n\n\
                     The driver line checks a Shared driver as soon as the app opens and again when devices change. \
                     Use Check driver after installing a driver.\n\n\
                     Many drivers only offer their 10 ms default for Shared. \
                     That is the driver's limit, and the driver line says so when it applies. \
                     Windows' in-box High Definition Audio Device driver often allows about 3 ms shared. \
                     If Exclusive is refused, RigWerk falls back to Shared with the chosen buffer size.",
                );
                section(
                    ui,
                    "Windows devices",
                    "An ASIO input with an ASIO or system clock output is wired through. \
                     Extra outputs can be WASAPI and play the master. \
                     The virtual output plays into VB-Audio Cable when it is installed, \
                     and other apps record CABLE Output. \
                     Whether one interface can play while its ASIO driver holds it depends on that driver.",
                );
            } else {
                section(
                    ui,
                    "Linux",
                    "Each PipeWire input and output is its own row.\n\
                     Extra outputs and RigWerk Out join when the clock output is PipeWire.\n\
                     hw: and plughw: stay direct. The ALSA buffer size is compared with the PipeWire graph buffer.",
                );
                section(
                    ui,
                    "Latency",
                    "PipeWire is how RigWerk opens devices on Linux. \
                     It opens its own input and output and links them to the devices you pick, \
                     so each device is its own row. On PipeWire the graph is the clock.\n\n\
                     Low latency sets a 64-sample buffer on RigWerk (about 1.3 ms at 48 kHz) \
                     and asks the PipeWire graph for the same 64-sample buffer.\n\n\
                     A direct ALSA clock output still asks for the smallest buffer size the card allows \
                     (at least 32 samples). That size is compared with the graph buffer: \
                     equal or evenly divisible values fit. \
                     A larger buffer, or one that does not divide the graph buffer, \
                     is flagged on the clock output and in the activity log. \
                     A graph buffer above 256 samples is the desktop's shared size.\n\n\
                     hw: and plughw: bypass the desktop mixer. \
                     If PipeWire is not running, the ALSA pipewire device stays in the list.",
                );
                section(
                    ui,
                    "Linux devices",
                    "PipeWire is the preferred graph. RigWerk opens its own connections \
                     and can send the master to extra outputs and to RigWerk Out. \
                     The graph buffer size is requested with pw-metadata and follows the desktop's PipeWire settings. \
                     Without a PipeWire server, the ALSA pipewire device and direct hw: and plughw: stay available.",
                );
            }
            ui.add_space(8.0);
            if ui
                .button(
                    RichText::new("About")
                        .font(font(Face::Sans, TypeRole::Control))
                        .color(colours.accent_text),
                )
                .clicked()
            {
                self.show_about = true;
            }
        });
    }
}

fn section(ui: &mut egui::Ui, title: &str, body: &str) {
    let colours = palette(ui.ctx());
    egui::CollapsingHeader::new(
        RichText::new(title)
            .font(font(Face::Sans, TypeRole::Control))
            .color(colours.text),
    )
    .default_open(false)
    .show(ui, |ui| {
        ui.label(
            RichText::new(body)
                .font(font(Face::Sans, TypeRole::Body))
                .color(colours.text2),
        );
    });
}
