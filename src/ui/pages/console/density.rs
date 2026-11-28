//! Strip widths for the window and the counts.  Pure, so the spec's table
//! is the test.

use crate::audio::{MAX_EXTRA_OUTPUTS, MAX_PATHS};

/// Window padding (24), the gaps between the groups (36), and the two add
/// columns with their gaps (80).
const FIXED_WIDTH: f32 = 24.0 + 36.0 + 80.0;

/// The clock output plus every extra output.
const MAX_OUTPUTS: usize = 1 + MAX_EXTRA_OUTPUTS;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DensityStep {
    Normal,
    CompactOutputs,
    NarrowPaths,
    MiniOutputs,
    Scroll,
}

impl DensityStep {
    /// The steps that fit without scrolling, in the order they are tried.
    pub(crate) const FITTING: [Self; 4] = [
        Self::Normal,
        Self::CompactOutputs,
        Self::NarrowPaths,
        Self::MiniOutputs,
    ];

    #[cfg(test)]
    pub(crate) fn number(self) -> u8 {
        match self {
            Self::Normal => 1,
            Self::CompactOutputs => 2,
            Self::NarrowPaths => 3,
            Self::MiniOutputs => 4,
            Self::Scroll => 5,
        }
    }

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

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct StripWidths {
    pub(crate) input: f32,
    pub(crate) path: f32,
    pub(crate) master: f32,
    pub(crate) output: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PathsScroll {
    pub(crate) visible: f32,
    pub(crate) content: f32,
}

impl PathsScroll {
    #[cfg(test)]
    pub(crate) fn visible_paths(self, path_width: f32) -> f32 {
        // n strips take n × w + (n − 1), so n = (width + 1) / (w + 1).
        (self.visible + 1.0) / (path_width + 1.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Density {
    pub(crate) step: DensityStep,
    pub(crate) widths: StripWidths,
    /// Only at step 5.
    pub(crate) scroll: Option<PathsScroll>,
}

/// A width that is not a number counts as 0.
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

/// For step 5 this is the step 4 figure: scrolling starts once that
/// does not fit.
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

fn strips(count: usize, width: f32) -> f32 {
    let count = count as f32;
    count * width + (count - 1.0)
}

#[cfg(test)]
mod tests;
