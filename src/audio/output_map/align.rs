//! The latency alignment of one device: the delay each path gets there
//! (see the module docs, "Alignment"), and the report the UI reads.

use super::super::path::MAX_PATHS;
use super::super::plugins::MAX_ALIGN;
use super::PATH_TRIM_MAX_MS;

/// A trim in ms as whole frames at `rate`, clamped to the trim range.
pub(crate) fn trim_frames(trim_ms: f32, rate: u32) -> i32 {
    if trim_ms.is_nan() {
        return 0;
    }
    let trim_ms = trim_ms.clamp(-PATH_TRIM_MAX_MS, PATH_TRIM_MAX_MS);
    (trim_ms * rate as f32 / 1000.0).round() as i32
}

/// The longest alignment delay, in frames: the longest chain latency
/// ([`MAX_ALIGN`]) plus room for the trims (twice the trim range at
/// 384 kHz), so a trim never has to be cut short.
pub(crate) const MAX_ALIGN_DELAY: u32 = MAX_ALIGN + 4096;

/// What the alignment does on one device: the delay each routed path gets
/// there, in frames, and which path the others wait for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct AlignmentPlan {
    /// Per path slot: the delay added on this device, `None` for a path the
    /// device does not play.
    pub(crate) delays: [Option<u32>; MAX_PATHS],
    /// The path the others wait for: the slowest routed one, when align is
    /// on and some other routed path is faster.
    pub(crate) waits_for: Option<usize>,
}

/// Works out one device's alignment from which paths it plays (`routed`),
/// their chain latency (frames, clamped to [`MAX_ALIGN`]) and their trims
/// (frames, signed).  Allocation free, for the audio thread.
///
/// With `align` on, path `p` waits `max(latency) − latency(p) + trim(p)`,
/// the maximum taken over the routed paths: every one of them, muted or
/// un-soloed included, so muting never moves the latency.  With it off,
/// only the trim applies.  Negative results shift every routed path by the
/// same amount, so the smallest delay is 0 (a negative trim makes the
/// others wait instead).  Delays are clamped to [`MAX_ALIGN_DELAY`].
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
    if align && routed_paths().any(|path| latency[path] < max) {
        plan.waits_for = Some(slowest);
    }
    plan
}

/// One device's alignment as the UI shows it, in frames at `rate`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AlignmentReport {
    /// The device's align switch.
    pub align: bool,
    /// The rate the frames are counted at.
    pub rate: u32,
    /// The path slot this device waits for (see [`AlignmentPlan`]).
    pub waits_for: Option<usize>,
    /// Per path slot: the alignment delay it gets on this device, `None`
    /// when the device does not play it.
    pub delays: [Option<u32>; MAX_PATHS],
    /// Per path slot: its chain latency plus its delay here, what the
    /// person hears it late by on this device.  `None` when not played.
    pub arrivals: [Option<u32>; MAX_PATHS],
    /// The largest of `arrivals`: the device's latency from the chains and
    /// the alignment (the slowest routed path, plus any trim).
    pub total: u32,
    /// The largest alignment delay on the device: what aligning (and the
    /// trims) adds on top of a path's own chain.
    pub added: u32,
    /// Some path this device plays has a chain latency above
    /// [`MAX_ALIGN`]: it is aligned (and counted here) as if it had
    /// exactly that much, so it arrives later than shown.
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

    /// Frames at this report's rate in ms.
    pub fn ms(&self, frames: u32) -> f32 {
        if self.rate == 0 {
            0.0
        } else {
            frames as f32 * 1000.0 / self.rate as f32
        }
    }
}
