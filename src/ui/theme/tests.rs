use eframe::egui::Color32;

use super::{CARD, CARD_RAISED, DIM, MUTED, TEXT};

fn channel(c: u8) -> f64 {
    let c = f64::from(c) / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn luminance(color: Color32) -> f64 {
    0.2126 * channel(color.r()) + 0.7152 * channel(color.g()) + 0.0722 * channel(color.b())
}

/// WCAG 2 contrast ratio.
fn contrast(foreground: Color32, background: Color32) -> f64 {
    let lighter = luminance(foreground).max(luminance(background));
    let darker = luminance(foreground).min(luminance(background));
    (lighter + 0.05) / (darker + 0.05)
}

#[test]
fn dim_clears_wcag_aa_on_card_and_stays_quieter_than_muted() {
    assert_eq!((CARD.r(), CARD.g(), CARD.b()), (16, 21, 31));
    assert_eq!((DIM.r(), DIM.g(), DIM.b()), (116, 133, 159));
    assert_ne!(DIM, MUTED);
    assert_ne!(DIM, TEXT);
    assert!(DIM.r() < MUTED.r() && DIM.g() < MUTED.g() && DIM.b() < MUTED.b());
    assert!(TEXT.r() > MUTED.r() && TEXT.g() > MUTED.g() && TEXT.b() > MUTED.b());

    let on_card = contrast(DIM, CARD);
    assert!(
        on_card >= 4.5,
        "DIM on CARD is {on_card:.3}:1, below WCAG AA 4.5:1"
    );
    let on_raised = contrast(DIM, CARD_RAISED);
    assert!(
        on_raised >= 4.5,
        "DIM on CARD_RAISED is {on_raised:.3}:1, below WCAG AA 4.5:1"
    );
    assert!(on_card < contrast(MUTED, CARD));
    assert!(contrast(MUTED, CARD) < contrast(TEXT, CARD));

    let previous = Color32::from_rgb(89, 102, 122);
    assert!(contrast(previous, CARD) < 4.5);
}
