use super::*;

const FLAGS: usize = 0;
const MIN: std::ops::Range<usize> = 5..7;
const MAX: std::ops::Range<usize> = 7..9;
const ASPECT: std::ops::Range<usize> = 11..15;

#[test]
fn a_fixed_editor_pins_both_axes() {
    let hints = normal_hints(ResizeHints::FIXED, (400, 300));
    assert_eq!(hints[FLAGS], P_MIN_SIZE | P_MAX_SIZE);
    assert_eq!(hints[MIN], [400, 300]);
    assert_eq!(hints[MAX], [400, 300]);
    assert_eq!(hints[ASPECT], [0, 0, 0, 0]);
}

#[test]
fn a_free_axis_is_open_and_a_fixed_one_pinned() {
    let wide_only = ResizeHints {
        horizontal: true,
        vertical: false,
        aspect: None,
    };
    let hints = normal_hints(wide_only, (400, 300));
    assert_eq!(hints[MIN], [1, 300]);
    assert_eq!(hints[MAX], [MAX_SIDE, 300]);
}

#[test]
fn a_kept_aspect_ratio_goes_to_the_window_manager() {
    let square = ResizeHints {
        horizontal: true,
        vertical: true,
        aspect: Some((4, 3)),
    };
    let hints = normal_hints(square, (400, 300));
    assert_eq!(hints[FLAGS], P_MIN_SIZE | P_MAX_SIZE | P_ASPECT);
    assert_eq!(hints[ASPECT], [4, 3, 4, 3]);
}

#[test]
fn sizes_outside_what_x11_describes_are_clamped() {
    let hints = normal_hints(ResizeHints::FIXED, (0, 1 << 20));
    assert_eq!(hints[MIN], [1, MAX_SIDE]);
}
