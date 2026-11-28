//! One device's alignment delays, and the report the UI reads.

use super::super::path::MAX_PATHS;
use super::super::plugins::MAX_ALIGN;
use super::PATH_TRIM_MAX_MS;

/// A NaN trim is 0.
pub(crate) fn trim_frames(trim_ms: f32, rate: u32) -> i32 {
    if trim_ms.is_nan() {
        return 0;
    }
    let trim_ms = trim_ms.clamp(-PATH_TRIM_MAX_MS, PATH_TRIM_MAX_MS);
    (trim_ms * rate as f32 / 1000.0).round() as i32
}

/// [`MAX_ALIGN`] plus the trim range at 384 kHz, so a trim is never
/// cut short.
pub(crate) const MAX_ALIGN_DELAY: u32 = MAX_ALIGN + 4096;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct AlignmentPlan {
    /// `None` for a path the device does not play.
    pub(crate) delays: [Option<u32>; MAX_PATHS],
    /// The slowest routed path, when align is on and another path is faster.
    pub(crate) waits_for: Option<usize>,
    /// The delay a dry, untrimmed path gets.  The metronome waits this.
    pub(crate) dry: u32,
}

/// Muted paths count, so mute does not move the latency.  A negative
/// trim shifts every path so the smallest delay is 0.  Allocation free.
pub(crate) fn plan_alignment(
    align: bool,
    routed: [bool; MAX_PATHS],
    latency: [u32; MAX_PATHS],
    trims: [i32; MAX_PATHS],
) -> AlignmentPlan {
    let latency = latency.map(|value| value.min(MAX_ALIGN) as i64);
    let routed_paths = || (0..MAX_PATHS).filter(|&path| routed[path]);
    let slowest = routed_paths().max_by(|&a, &b| latency[a].cmp(&latency[b]).then(b.cmp(&a)));
    let Some(slowest) = slowest else {
        return AlignmentPlan::default();
    };
    let max = latency[slowest];
    let raw = |path: usize| {
        let wait = if align { max - latency[path] } else { 0 };
        wait + trims[path] as i64
    };
    let lowest = routed_paths().map(raw).min().unwrap_or(0);
    let shift = (-lowest).max(0);
    let mut plan = AlignmentPlan::default();
    for path in routed_paths() {
        let delay = (raw(path) + shift).clamp(0, MAX_ALIGN_DELAY as i64);
        plan.delays[path] = Some(delay as u32);
    }
    let dry = if align { max } else { 0 };
    plan.dry = (dry + shift).clamp(0, MAX_ALIGN_DELAY as i64) as u32;
    if align && routed_paths().any(|path| latency[path] < max) {
        plan.waits_for = Some(slowest);
    }
    plan
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AlignmentReport {
    pub align: bool,
    pub rate: u32,
    pub waits_for: Option<usize>,
    /// `None` when the device does not play the path.
    pub delays: [Option<u32>; MAX_PATHS],
    /// Chain latency plus the delay here: how late the path is heard.
    /// `None` when not played.
    pub arrivals: [Option<u32>; MAX_PATHS],
    pub total: u32,
    /// What aligning and the trims add on top of a path's own chain.
    pub added: u32,
    /// A chain past [`MAX_ALIGN`] is aligned as if it had exactly that,
    /// so it arrives later than shown.
    pub clamped: bool,
}

impl AlignmentReport {
    pub(crate) fn new(
        plan: AlignmentPlan,
        align: bool,
        latency: [u32; MAX_PATHS],
        rate: u32,
    ) -> Self {
        let arrivals: [Option<u32>; MAX_PATHS] = std::array::from_fn(|path| {
            plan.delays[path].map(|delay| delay + latency[path].min(MAX_ALIGN))
        });
        Self {
            align,
            rate,
            waits_for: plan.waits_for,
            delays: plan.delays,
            arrivals,
            total: arrivals.iter().flatten().copied().max().unwrap_or(0),
            added: plan.delays.iter().flatten().copied().max().unwrap_or(0),
            clamped: (0..MAX_PATHS)
                .any(|path| plan.delays[path].is_some() && latency[path] > MAX_ALIGN),
        }
    }

    pub fn ms(&self, frames: u32) -> f32 {
        if self.rate == 0 {
            0.0
        } else {
            frames as f32 * 1000.0 / self.rate as f32
        }
    }
}
