//! Alignment lines paths up on one device.  It does not line devices up
//! with each other: an extra output's bridge buffer and each device's
//! own buffer still differ.

use crate::app::RigWerkApp;
use crate::audio::{AlignmentReport, MAX_PATHS};

/// "Out 1, Out 2" for 0-based channels.
pub(crate) fn channel_list(channels: &[usize]) -> String {
    channels
        .iter()
        .map(|channel| format!("Out {}", channel + 1))
        .collect::<Vec<_>>()
        .join(", ")
}

/// What the alignment does on one output device.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct DeviceAlignment {
    pub(crate) align: bool,
    /// The slowest path it plays, when align is on and another is faster.
    pub(crate) waits_for: Option<usize>,
    /// The slowest path it plays, including its trim.
    pub(crate) total_ms: f32,
    /// The largest delay aligning and the trims add beyond a path's chain.
    pub(crate) added_ms: f32,
    /// `None` when the device does not play the path.
    pub(crate) per_path: [Option<f32>; MAX_PATHS],
    /// Chain latency plus the delay here.  `None` when not played.
    pub(crate) arrival_ms: [Option<f32>; MAX_PATHS],
    /// Exact comparisons: one frame at 192 kHz is about 5 µs.
    pub(crate) arrival_frames: [Option<u32>; MAX_PATHS],
    /// A path's chain is longer than the alignment covers.
    pub(crate) clamped: bool,
    /// From the running mixer.  False when worked out from the settings.
    pub(crate) live: bool,
}

impl DeviceAlignment {
    pub(crate) fn path_count(&self) -> usize {
        self.per_path.iter().flatten().count()
    }

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
    /// From the running mixer when its switch matches; otherwise from the
    /// settings.  The engine reports only while a bridge runs and the
    /// device is fed.
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

    pub(crate) fn device_align(&self, device: usize) -> bool {
        self.device_settings()
            .get(device)
            .is_some_and(|settings| settings.align)
    }

    /// The next push crossfades the delays.
    pub(crate) fn set_device_align(&mut self, device: usize, align: bool) {
        if let Some(settings) = self.device_settings_mut().nth(device) {
            settings.align = align;
        }
    }
}

/// Why the Signal Routing page warns about one device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RoutingWarningKind {
    /// Paths add on one channel and comb-filter.
    Sum,
    /// Paths on different channels shift the image.
    Shift,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RoutingWarning {
    pub(crate) kind: RoutingWarningKind,
    /// Display order.
    pub(crate) paths: Vec<usize>,
    /// `Sum`: channels carrying several paths.  `Shift`: every playing one.
    pub(crate) channels: Vec<usize>,
}

impl RigWerkApp {
    /// Every live path counts, muted or not.  `Sum` also fires on an
    /// aligned device when trims pull paths apart, and it wins over `Shift`.
    pub(crate) fn routing_warnings(&self, device: usize) -> Vec<RoutingWarning> {
        let Some(settings) = self.device_settings().get(device).copied() else {
            return Vec::new();
        };
        let arrival = self
            .device_alignment(device)
            .map(|alignment| alignment.arrival_frames)
            .unwrap_or_default();
        // Paths line up only when they arrive on the same frame.
        let apart = |paths: &[bool; MAX_PATHS]| {
            let mut times = (0..MAX_PATHS)
                .filter(|&path| paths[path])
                .map(|path| arrival[path].unwrap_or(0));
            times
                .next()
                .is_some_and(|first| times.any(|time| time != first))
        };
        let channels = self.device_channels(device);
        let live = self.live_path_mask();
        let order = self.path_slots();
        let mut sounding = [false; MAX_PATHS];
        let mut summed = [false; MAX_PATHS];
        let mut sum_channels = Vec::new();
        let mut playing_channels = Vec::new();
        for channel in 0..channels {
            let on = settings.paths_on_channel(live, channels, channel);
            let count = on.iter().filter(|on| **on).count();
            if count > 0 {
                playing_channels.push(channel);
            }
            let unaligned = count > 1 && apart(&on);
            if unaligned {
                sum_channels.push(channel);
            }
            for path in 0..MAX_PATHS {
                sounding[path] |= on[path];
                summed[path] |= on[path] && unaligned;
            }
        }
        let in_order = |flags: [bool; MAX_PATHS]| -> Vec<usize> {
            order.iter().copied().filter(|&slot| flags[slot]).collect()
        };
        if !sum_channels.is_empty() {
            return vec![RoutingWarning {
                kind: RoutingWarningKind::Sum,
                paths: in_order(summed),
                channels: sum_channels,
            }];
        }
        let paths = in_order(sounding);
        if !settings.align && paths.len() > 1 && apart(&sounding) {
            return vec![RoutingWarning {
                kind: RoutingWarningKind::Shift,
                paths,
                channels: playing_channels,
            }];
        }
        Vec::new()
    }
}

#[cfg(test)]
mod tests;
