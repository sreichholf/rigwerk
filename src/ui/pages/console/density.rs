//! The console's density step (spec §5.2): the strip widths for the window
//! width and the strip counts.  Steps 1 to 4 are tried in order and the
//! first that fits wins; step 5 keeps the step 4 widths and scrolls the
//! paths group sideways.  It is pure, so the spec's table is its test.
//!
//! Input and Master are one strip each.  The counts are clamped to 1 to
//! `MAX_PATHS` paths and 1 to the clock plus `MAX_FOLLOWERS` outputs, so a
//! caller never gets a layout for a setup that cannot exist.

use crate::audio::{MAX_FOLLOWERS, MAX_PATHS};

/// Window padding (24), the gaps between the groups (36), and the two add
/// columns with their gaps (80).
const FIXED_WIDTH: f32 = 24.0 + 36.0 + 80.0;

/// The clock output plus every follower.
const MAX_OUTPUTS: usize = 1 + MAX_FOLLOWERS;

/// Step 1 is the widest; step 5 scrolls the paths group.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DensityStep {
    Normal,
    CompactOutputs,
    NarrowPaths,
    MiniOutputs,
    Scroll,
}

#[cfg_attr(not(test), allow(dead_code))]
impl DensityStep {
    /// The steps that fit without scrolling, in the order they are tried.
    pub(crate) const FITTING: [Self; 4] = [
        Self::Normal,
        Self::CompactOutputs,
        Self::NarrowPaths,
        Self::MiniOutputs,
    ];

    /// The step's number in the spec, 1 to 5.
    pub(crate) fn number(self) -> u8 {
        match self {
            Self::Normal => 1,
            Self::CompactOutputs => 2,
            Self::NarrowPaths => 3,
            Self::MiniOutputs => 4,
            Self::Scroll => 5,
        }
    }

    /// The width of one strip of each kind at this step.
    pub(crate) fn widths(self) -> StripWidths {
        let (input, path, master, output) = match self {
            Self::Normal => (188.0, 224.0, 112.0, 160.0),
            Self::CompactOutputs => (188.0, 224.0, 112.0, 96.0),
            Self::NarrowPaths => (132.0, 172.0, 88.0, 96.0),
            Self::MiniOutputs | Self::Scroll => (132.0, 172.0, 88.0, 64.0),
        };
        StripWidths {
            input,
            path,
            master,
            output,
        }
    }
}

/// One strip's width per kind, in points.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct StripWidths {
    pub(crate) input: f32,
    pub(crate) path: f32,
    pub(crate) master: f32,
    pub(crate) output: f32,
}

/// The paths group at step 5: the part that shows, and all of it.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PathsScroll {
    /// The width the group gets, never below 0.
    pub(crate) visible: f32,
    /// Every path strip with the 1 pt gaps between them.
    pub(crate) content: f32,
}

#[cfg_attr(not(test), allow(dead_code))]
impl PathsScroll {
    /// How many path strips of `path_width` the visible width holds, with a
    /// fraction for the strip cut off at the edge.
    pub(crate) fn visible_paths(self, path_width: f32) -> f32 {
        // n strips take n × w + (n − 1), so n = (width + 1) / (w + 1).
        (self.visible + 1.0) / (path_width + 1.0)
    }
}

/// The console's layout for one window width.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Density {
    pub(crate) step: DensityStep,
    pub(crate) widths: StripWidths,
    /// Only at step 5.
    pub(crate) scroll: Option<PathsScroll>,
}

/// The widest step that fits `available` points with `paths` paths and
/// `outputs` outputs (the clock device included).  A width that is not a
/// number counts as 0.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn density(available: f32, paths: usize, outputs: usize) -> Density {
    let available = if available.is_nan() { 0.0 } else { available };
    let (paths, outputs) = clamp_counts(paths, outputs);
    for step in DensityStep::FITTING {
        if width_needed(step, paths, outputs) <= available {
            return Density {
                step,
                widths: step.widths(),
                scroll: None,
            };
        }
    }
    let step = DensityStep::Scroll;
    let widths = step.widths();
    let fixed = FIXED_WIDTH + widths.input + widths.master + strips(outputs, widths.output);
    Density {
        step,
        widths,
        scroll: Some(PathsScroll {
            visible: (available - fixed).max(0.0),
            content: strips(paths, widths.path),
        }),
    }
}

/// The window width `step` needs with every strip shown.  For step 5 that
/// is the step 4 figure: it only scrolls once that does not fit.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn width_needed(step: DensityStep, paths: usize, outputs: usize) -> f32 {
    let (paths, outputs) = clamp_counts(paths, outputs);
    let widths = step.widths();
    FIXED_WIDTH
        + widths.input
        + strips(paths, widths.path)
        + widths.master
        + strips(outputs, widths.output)
}

fn clamp_counts(paths: usize, outputs: usize) -> (usize, usize) {
    (paths.clamp(1, MAX_PATHS), outputs.clamp(1, MAX_OUTPUTS))
}

/// `count` strips of `width` with a 1 pt gap between neighbours.
fn strips(count: usize, width: f32) -> f32 {
    let count = count as f32;
    count * width + (count - 1.0)
}

#[cfg(test)]
mod tests;
