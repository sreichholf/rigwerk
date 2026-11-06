//! The latency alignment per output device, as the UI reads and sets it.
//!
//! The engine lines the paths up on each device (see `audio::output_map`,
//! "Alignment"); this is the app side: the per-device align switch, and
//! what the alignment does on a device in ms, for the Signal Routing page
//! (latency line, alignment delay per path) and the Live summary.

// The alignment UI (U3) builds on this.
#![allow(dead_code)]

use crate::app::RigWerkApp;
use crate::audio::{AlignmentReport, MAX_PATHS};

/// What the alignment does on one output device.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct DeviceAlignment {
    /// The device's align switch.
    pub(crate) align: bool,
    /// The path slot this device waits for: the slowest path it plays,
    /// when align is on and some other path it plays is faster.
    pub(crate) waits_for: Option<usize>,
    /// The device's latency from the chains and the alignment: the slowest
    /// path it plays, including its trim, ms.
    pub(crate) total_ms: f32,
    /// The largest alignment delay on the device (what aligning and the
    /// trims add on top of a path's own chain), ms.
    pub(crate) added_ms: f32,
    /// Per path slot: the alignment delay the path gets here, ms; `None`
    /// when the device does not play it.
    pub(crate) per_path: [Option<f32>; MAX_PATHS],
    /// Per path slot: its chain latency plus its delay here (when the
    /// person hears it on this device), ms; `None` when not played.
    pub(crate) arrival_ms: [Option<f32>; MAX_PATHS],
    /// The same in frames, for exact comparisons (one frame at 192 kHz is
    /// about 5 µs).
    pub(crate) arrival_frames: [Option<u32>; MAX_PATHS],
    /// Some path played here has more chain latency than the alignment
    /// covers (see `AlignmentReport::clamped`).
    pub(crate) clamped: bool,
    /// Read from the running mixer (`true`) or worked out from the
    /// settings because no bridge runs (`false`).
    pub(crate) live: bool,
}

impl DeviceAlignment {
    fn from_report(report: &AlignmentReport, live: bool) -> Self {
        Self {
            align: report.align,
            waits_for: report.waits_for,
            total_ms: report.ms(report.total),
            added_ms: report.ms(report.added),
            per_path: report
                .delays
                .map(|delay| delay.map(|frames| report.ms(frames))),
            arrival_ms: report
                .arrivals
                .map(|arrival| arrival.map(|frames| report.ms(frames))),
            arrival_frames: report.arrivals,
            clamped: report.clamped,
            live,
        }
    }
}

impl RigWerkApp {
    /// What the alignment does on output device `device` (0 = clock
    /// device, then followers).  From the running mixer when it has
    /// published one with the same switch (the engine only reports while a
    /// bridge runs and the device is fed), else worked out from the
    /// settings, the paths' chain latency and the plugin chains' rate.
    /// `None` for a device that does not exist.
    pub(crate) fn device_alignment(&self, device: usize) -> Option<DeviceAlignment> {
        let settings = *self.device_settings().get(device).copied()?;
        if let Some(report) = self.engine.device_alignment(device) {
            if report.align == settings.align {
                return Some(DeviceAlignment::from_report(&report, true));
            }
        }
        let report = settings.alignment(
            self.live_path_mask(),
            self.device_channels(device),
            self.engine.path_latencies(),
            self.engine.plugin_rate().max(1),
        );
        Some(DeviceAlignment::from_report(&report, false))
    }

    /// Output device `device`'s align switch.
    pub(crate) fn device_align(&self, device: usize) -> bool {
        self.device_settings()
            .get(device)
            .is_some_and(|settings| settings.align)
    }

    /// Turns latency alignment on output device `device` on or off.  Takes
    /// effect with the next push to the engine (every frame), with the
    /// delays crossfading.
    pub(crate) fn set_device_align(&mut self, device: usize, align: bool) {
        if let Some(settings) = self.device_settings_mut().nth(device) {
            settings.align = align;
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::app::blank_app;
    use crate::app::settings::FollowerPref;

    #[test]
    fn alignment_follows_the_switch_and_the_trims() {
        let mut app = blank_app();
        app.followers.push(FollowerPref::virtual_out());
        let second = app
            .duplicate_path(app.path_slots()[0])
            .expect("a free slot");
        assert!(app.device_align(0) && app.device_align(1));
        // No latency: nothing waits and nothing is added.
        // No bridge runs: the engine reports nothing, the app works it out.
        assert!(app.engine.device_alignment(0).is_none());
        let aligned = app.device_alignment(0).expect("the clock device");
        assert!(aligned.align && !aligned.live);
        assert_eq!(aligned.waits_for, None);
        assert_eq!(aligned.per_path[0], Some(0.0));
        assert_eq!(aligned.per_path[second], Some(0.0));
        assert_eq!(aligned.added_ms, 0.0);

        // A 1 ms trim on the second path delays it by 1 ms.
        let mut entry = app.device_path_map(0, second).unwrap();
        entry.trim_ms = 1.0;
        app.set_device_path_map(0, second, entry);
        let trimmed = app.device_alignment(0).unwrap();
        assert!((trimmed.per_path[second].unwrap() - 1.0).abs() < 1e-3);
        assert!((trimmed.total_ms - 1.0).abs() < 1e-3);
        // A negative trim makes the other path wait instead.
        entry.trim_ms = -2.0;
        app.set_device_path_map(0, second, entry);
        let early = app.device_alignment(0).unwrap();
        assert_eq!(early.per_path[second], Some(0.0));
        assert!((early.per_path[0].unwrap() - 2.0).abs() < 1e-3);

        app.set_device_align(1, false);
        assert!(!app.device_align(1) && app.device_align(0));
        assert!(!app.device_alignment(1).unwrap().align);
        assert!(app.device_alignment(2).is_none());
    }
}
