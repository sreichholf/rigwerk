use super::{color_image, window_icon, Mark};

#[test]
fn both_marks_decode_to_the_largest_frame() {
    for mark in [Mark::Full, Mark::Compact] {
        let image = color_image(mark);
        assert_eq!(image.size, [256, 256], "{mark:?}");
        assert!(image.pixels.iter().any(|pixel| pixel.a() > 0), "{mark:?}");
    }
}

#[test]
fn the_window_icon_is_rgba_and_square() {
    let icon = window_icon();
    assert_eq!((icon.width, icon.height), (256, 256));
    assert_eq!(icon.rgba.len(), 256 * 256 * 4);
    assert!(icon.rgba.chunks_exact(4).any(|pixel| pixel[3] > 0));
}
