//! The density step for the console, from the available width and the strip
//! counts.
//!
//! Widths and the sum are §5.2 of `docs/ui-spec.md`.  The widest step whose
//! strips fit wins.  Step 5 keeps Input, Master and the outputs on screen
//! and gives the paths the width that remains.  Path counts outside 1–4 and
//! output counts outside 1–5 are clamped into that range, so a strip gap is
//! never asked for on an empty group.

// The strips call `fit` once they are drawn.
#![cfg_attr(
    not(test),
    expect(dead_code, reason = "the console calls fit once the strips are drawn")
)]

/// Window padding on the console row.
const WINDOW_PADDING: f32 = 24.0;
/// Gaps between Input, Paths, Master and Outputs.
const GROUP_GAPS: f32 = 36.0;
/// The two add columns and the gaps beside them.
const ADD_COLUMNS: f32 = 80.0;
const CHROME: f32 = WINDOW_PADDING + GROUP_GAPS + ADD_COLUMNS;

/// Strip widths for steps 1–4.  Step 5 uses the last row and scrolls.
const STEPS: [Strips; 4] = [
    Strips {
        input: 188.0,
        path: 224.0,
        master: 112.0,
        output: 160.0,
    },
    Strips {
        input: 188.0,
        path: 224.0,
        master: 112.0,
        output: 96.0,
    },
    Strips {
        input: 132.0,
        path: 172.0,
        master: 88.0,
        output: 96.0,
    },
    Strips {
        input: 132.0,
        path: 172.0,
        master: 88.0,
        output: 64.0,
    },
];

struct Strips {
    input: f32,
    path: f32,
    master: f32,
    output: f32,
}

/// How much of the console fits the window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Density {
    /// 1 shows every strip at full width.  5 scrolls the paths.
    pub(crate) step: u8,
    /// Visible width of the paths group at step 5.
    pub(crate) scroll_width: Option<f32>,
}

/// The widest density step that fits `available` points.
pub(crate) fn fit(available: f32, paths: usize, outputs: usize) -> Density {
    let paths = paths.clamp(1, 4);
    let outputs = outputs.clamp(1, 5);
    for (index, strips) in STEPS.iter().enumerate() {
        if width_needed(strips, paths, outputs) <= available {
            return Density {
                step: (index + 1) as u8,
                scroll_width: None,
            };
        }
    }
    // Step 5 uses step 4's strip widths.  Input, Master, the outputs and
    // the add columns stay put; the paths take whatever is left.
    let strips = &STEPS[3];
    let fixed = CHROME + strips.input + strips.master + group(outputs, strips.output);
    Density {
        step: 5,
        scroll_width: Some((available - fixed).max(0.0)),
    }
}

/// `n` strips of width `w` and a 1 pt line between neighbours.
fn group(count: usize, width: f32) -> f32 {
    count as f32 * width + (count - 1) as f32
}

fn width_needed(strips: &Strips, paths: usize, outputs: usize) -> f32 {
    CHROME
        + strips.input
        + group(paths, strips.path)
        + strips.master
        + group(outputs, strips.output)
}

#[cfg(test)]
mod tests;
