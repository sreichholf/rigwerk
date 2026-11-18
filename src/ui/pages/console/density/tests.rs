use super::fit;

/// One window from the §5.2 table.
struct At {
    width: f32,
    step: u8,
    /// Paths-group width when `step` is 5.
    scroll_width: Option<f32>,
    /// `scroll_width / 172`, in tenths, with the remainder dropped: the
    /// table's "3.8 paths" is 38.
    shown_tenths: Option<f32>,
}

fn at(width: f32, step: u8, scroll_width: Option<f32>, shown_tenths: Option<f32>) -> At {
    At {
        width,
        step,
        scroll_width,
        shown_tenths,
    }
}

/// §5.2, every setup at 1440, 1280 and 1040.
///
/// Step 5's scroll width is what remains after the fixed row:
/// 24 + 36 + 80 + input 132 + master 88 + the outputs at 64, with a 1 pt
/// line between outputs.  Four outputs leave 619, so 1280 and 1040 give
/// 661 and 421 (3.8 and 2.4 paths of 172).  Five outputs leave 684, so
/// those widths give 596 and 356 (3.4 and 2.0 paths).
#[test]
fn every_row_at_1440_1280_and_1040() {
    let rows = [
        (
            1,
            1,
            [
                at(1440.0, 1, None, None),
                at(1280.0, 1, None, None),
                at(1040.0, 1, None, None),
            ],
        ),
        (
            2,
            3,
            [
                at(1440.0, 1, None, None),
                at(1280.0, 2, None, None),
                at(1040.0, 3, None, None),
            ],
        ),
        (
            4,
            4,
            [
                at(1440.0, 3, None, None),
                at(1280.0, 5, Some(661.0), Some(38.0)),
                at(1040.0, 5, Some(421.0), Some(24.0)),
            ],
        ),
        (
            4,
            5,
            [
                at(1440.0, 4, None, None),
                at(1280.0, 5, Some(596.0), Some(34.0)),
                at(1040.0, 5, Some(356.0), Some(20.0)),
            ],
        ),
    ];
    for (paths, outputs, windows) in rows {
        for window in windows {
            let density = fit(window.width, paths, outputs);
            let label = format!("{paths} paths, {outputs} outputs, at {}", window.width);
            assert_eq!(density.step, window.step, "{label}");
            assert_eq!(density.scroll_width, window.scroll_width, "{label}");
            assert_eq!(density.step == 5, density.scroll_width.is_some(), "{label}");
            if let Some(tenths) = window.shown_tenths {
                let scroll = density.scroll_width.expect("step 5");
                let shown = (scroll / 172.0 * 10.0).floor();
                assert_eq!(shown, tenths, "{label}");
            }
        }
    }
}

/// The width columns of §5.2 are where the step changes: that width still
/// fits, and one point less takes the next step.
#[test]
fn published_widths_are_the_step_boundaries() {
    let rows = [
        (1, 1, [824.0, 760.0, 628.0, 596.0]),
        (2, 3, [1371.0, 1179.0, 995.0, 899.0]),
        (4, 4, [1982.0, 1726.0, 1438.0, 1310.0]),
        (4, 5, [2143.0, 1823.0, 1535.0, 1375.0]),
    ];
    for (paths, outputs, widths) in rows {
        for (index, width) in widths.iter().copied().enumerate() {
            let step = (index + 1) as u8;
            let fitted = fit(width, paths, outputs);
            let label = format!("{paths} paths, {outputs} outputs, at {width}");
            assert_eq!(fitted.step, step, "{label}");
            assert!(fitted.scroll_width.is_none(), "{label}");
            let narrower = fit(width - 1.0, paths, outputs);
            assert_eq!(
                narrower.step,
                step + 1,
                "{paths} paths, {outputs} outputs, at {}",
                width - 1.0
            );
            assert_eq!(narrower.step == 5, narrower.scroll_width.is_some());
        }
    }
}

#[test]
fn counts_outside_the_board_are_clamped() {
    // One point under the 4-path, 5-output step 4 width, so the paths scroll
    // and the scroll width shows which counts were used.
    let width = 1374.0;
    assert_eq!(fit(width, 0, 0).scroll_width, fit(width, 1, 1).scroll_width);
    assert_eq!(fit(width, 8, 8).scroll_width, fit(width, 4, 5).scroll_width);
}
