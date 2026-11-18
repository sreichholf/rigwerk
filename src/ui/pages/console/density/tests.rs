use super::*;

/// The setups of the spec's table: paths, outputs, the width steps 1 to 4
/// need, and the step at 1440, 1280 and 1040.
const TABLE: [(usize, usize, [f32; 4], [u8; 3]); 4] = [
    (1, 1, [824.0, 760.0, 628.0, 596.0], [1, 1, 1]),
    (2, 3, [1371.0, 1179.0, 995.0, 899.0], [1, 2, 3]),
    (4, 4, [1982.0, 1726.0, 1438.0, 1310.0], [3, 5, 5]),
    (4, 5, [2143.0, 1823.0, 1535.0, 1375.0], [4, 5, 5]),
];

const WIDTHS: [f32; 3] = [1440.0, 1280.0, 1040.0];

/// The visible path count to one decimal, as the spec writes it.
fn tenths(paths: f32) -> f32 {
    (paths * 10.0).floor() / 10.0
}

fn visible_paths(available: f32, paths: usize, outputs: usize) -> f32 {
    let density = density(available, paths, outputs);
    let scroll = density.scroll.expect("step 5 scrolls");
    scroll.visible_paths(density.widths.path)
}

#[test]
fn width_needed_matches_the_table() {
    for (paths, outputs, needs, _) in TABLE {
        for (step, need) in DensityStep::FITTING.into_iter().zip(needs) {
            assert_eq!(
                width_needed(step, paths, outputs),
                need,
                "{paths} paths, {outputs} outputs, step {}",
                step.number()
            );
        }
    }
}

#[test]
fn step_matches_the_table() {
    for (paths, outputs, _, steps) in TABLE {
        for (available, step) in WIDTHS.into_iter().zip(steps) {
            let density = density(available, paths, outputs);
            assert_eq!(
                density.step.number(),
                step,
                "{paths} paths, {outputs} outputs at {available}"
            );
            assert_eq!(density.widths, density.step.widths());
            assert_eq!(density.scroll.is_some(), step == 5);
        }
    }
}

#[test]
fn step_widths_match_the_table() {
    let rows = [
        (DensityStep::Normal, 1, [188.0, 224.0, 112.0, 160.0]),
        (DensityStep::CompactOutputs, 2, [188.0, 224.0, 112.0, 96.0]),
        (DensityStep::NarrowPaths, 3, [132.0, 172.0, 88.0, 96.0]),
        (DensityStep::MiniOutputs, 4, [132.0, 172.0, 88.0, 64.0]),
        (DensityStep::Scroll, 5, [132.0, 172.0, 88.0, 64.0]),
    ];
    for (step, number, [input, path, master, output]) in rows {
        assert_eq!(step.number(), number);
        assert_eq!(
            step.widths(),
            StripWidths {
                input,
                path,
                master,
                output
            }
        );
    }
}

#[test]
fn step_5_shows_the_table_path_counts() {
    assert_eq!(tenths(visible_paths(1280.0, 4, 4)), 3.8);
    assert_eq!(tenths(visible_paths(1040.0, 4, 4)), 2.4);
    assert_eq!(tenths(visible_paths(1280.0, 4, 5)), 3.4);
    assert_eq!(tenths(visible_paths(1040.0, 4, 5)), 2.0);
}

#[test]
fn step_5_scroll_widths() {
    // 4 paths, 4 outputs at 1280: 140 + 132 + 88 + (4 × 64 + 3) is fixed.
    let scroll = density(1280.0, 4, 4).scroll.unwrap();
    assert_eq!(scroll.visible, 1280.0 - 619.0);
    assert_eq!(scroll.content, 4.0 * 172.0 + 3.0);
    // 4 paths, 4 outputs at 1040, same fixed row.
    let scroll = density(1040.0, 4, 4).scroll.unwrap();
    assert_eq!(scroll.visible, 1040.0 - 619.0);
    assert_eq!(scroll.content, 691.0);
    // 4 paths, 5 outputs at 1280: 140 + 132 + 88 + (5 × 64 + 4) is fixed.
    let scroll = density(1280.0, 4, 5).scroll.unwrap();
    assert_eq!(scroll.visible, 1280.0 - 684.0);
    assert_eq!(scroll.content, 691.0);
    // 4 paths, 5 outputs at 1040, same fixed row.
    let scroll = density(1040.0, 4, 5).scroll.unwrap();
    assert_eq!(scroll.visible, 1040.0 - 684.0);
    assert_eq!(scroll.content, 691.0);
}

#[test]
fn an_exactly_fitting_width_takes_that_step() {
    for (paths, outputs, needs, _) in TABLE {
        for (step, need) in DensityStep::FITTING.into_iter().zip(needs) {
            assert_eq!(density(need, paths, outputs).step, step);
        }
        // Just short of step 4 scrolls, and the visible part is one point
        // less than all of it.
        let density = density(needs[3] - 1.0, paths, outputs);
        assert_eq!(density.step, DensityStep::Scroll);
        let scroll = density.scroll.unwrap();
        assert_eq!(scroll.visible, scroll.content - 1.0);
    }
}

#[test]
fn no_width_scrolls_with_nothing_visible() {
    for available in [0.0, -1.0, -5000.0, f32::NEG_INFINITY, f32::NAN] {
        let density = density(available, 2, 3);
        assert_eq!(density.step, DensityStep::Scroll, "{available}");
        let scroll = density.scroll.unwrap();
        assert_eq!(scroll.visible, 0.0, "{available}");
        assert_eq!(scroll.content, 2.0 * 172.0 + 1.0);
    }
}

#[test]
fn counts_are_clamped_to_their_ranges() {
    assert_eq!(width_needed(DensityStep::Normal, 0, 0), 824.0);
    // 4 paths and the clock output: 140 + 188 + 899 + 112 + 160.
    assert_eq!(width_needed(DensityStep::Normal, 9, 1), 1499.0);
    assert_eq!(width_needed(DensityStep::Normal, 4, 12), 2143.0);
    assert_eq!(density(1040.0, 0, 0), density(1040.0, 1, 1));
    assert_eq!(density(1280.0, 100, 100), density(1280.0, 4, 5));
    assert_eq!(density(1280.0, 100, 100).scroll.unwrap().content, 691.0);
}
