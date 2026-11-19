use eframe::egui;
use egui::Color32;

use super::{
    font, hover_cursor, palette, ts, type_size, Face, Palette, TypeRole, AMBER, CARD, DARK, DIM,
    LIGHT, MUTED, TEAL, TEXT,
};

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
    assert_eq!((CARD.r(), CARD.g(), CARD.b()), (0x1a, 0x19, 0x18));
    assert_eq!((DIM.r(), DIM.g(), DIM.b()), (0x94, 0x8e, 0x83));
    assert_ne!(DIM, MUTED);
    assert_ne!(DIM, TEXT);
    assert!(DIM.r() < MUTED.r() && DIM.g() < MUTED.g() && DIM.b() < MUTED.b());
    assert!(TEXT.r() > MUTED.r() && TEXT.g() > MUTED.g() && TEXT.b() > MUTED.b());

    let on_card = contrast(DIM, CARD);
    assert!(
        on_card >= 4.5,
        "DIM on CARD is {on_card:.3}:1, below WCAG AA 4.5:1"
    );
    let on_raised = contrast(DIM, DARK.slot);
    assert!(
        on_raised >= 4.5,
        "DIM on a slot is {on_raised:.3}:1, below WCAG AA 4.5:1"
    );
    assert!(on_card < contrast(MUTED, CARD));
    assert!(contrast(MUTED, CARD) < contrast(TEXT, CARD));

    let previous = Color32::from_rgb(89, 102, 122);
    assert!(contrast(previous, CARD) < 4.5);
}

fn palettes() -> [(&'static str, &'static Palette); 2] {
    [("dark", &DARK), ("light", &LIGHT)]
}

/// Every token against every surface must reach `minimum`.  Failures are
/// collected, so one run lists them all.
fn check_pairs(
    theme: &str,
    tokens: &[(&str, Color32)],
    surfaces: &[(&str, Color32)],
    minimum: f64,
    failures: &mut Vec<String>,
) {
    for (token, colour) in tokens {
        for (surface, background) in surfaces {
            let ratio = contrast(*colour, *background);
            if ratio < minimum {
                failures.push(format!(
                    "{theme}: {token} on {surface} is {ratio:.2}:1, below {minimum}:1"
                ));
            }
        }
    }
}

#[test]
fn text_tokens_read_on_every_text_surface() {
    let mut failures = Vec::new();
    for (theme, p) in palettes() {
        let text = [
            ("text", p.text),
            ("text2", p.text2),
            ("text3", p.text3),
            ("accent_text", p.accent_text),
            ("warn", p.warn),
            ("error_text", p.error_text),
        ];
        let surfaces = [
            ("bg", p.bg),
            ("bar", p.bar),
            ("strip", p.strip),
            ("sheet", p.sheet),
            ("slot", p.slot),
        ];
        check_pairs(theme, &text, &surfaces, 4.5, &mut failures);
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn state_colours_stand_out_on_strips_and_slots() {
    let mut failures = Vec::new();
    for (theme, p) in palettes() {
        let mut states = vec![
            ("accent", p.accent),
            ("led_off", p.led_off),
            ("meter", p.meter),
            ("meter_hot", p.meter_hot),
            ("warn", p.warn),
        ];
        states.extend(
            ["path 1", "path 2", "path 3", "path 4"]
                .into_iter()
                .zip(p.path),
        );
        let surfaces = [("strip", p.strip), ("slot", p.slot)];
        check_pairs(theme, &states, &surfaces, 3.0, &mut failures);
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn meter_hot_is_warn_and_no_two_paths_share_a_colour() {
    for (theme, p) in palettes() {
        assert_eq!(p.meter_hot, p.warn, "{theme}");
        for (i, a) in p.path.iter().enumerate() {
            for b in &p.path[i + 1..] {
                assert_ne!(a, b, "{theme}: two paths share a colour");
            }
        }
    }
}

#[test]
fn the_old_constants_are_the_dark_tokens() {
    assert_eq!(TEXT, DARK.text);
    assert_eq!(MUTED, DARK.text2);
    assert_eq!(DIM, DARK.text3);
    assert_eq!(CARD, DARK.strip);
    assert_eq!(TEAL, DARK.accent);
    assert_eq!(AMBER, DARK.warn);
}

#[test]
fn the_type_scale_is_the_spec_and_ts_is_unchanged() {
    let scale = [
        (TypeRole::GridHeader, 11.0),
        (TypeRole::Secondary, 12.0),
        (TypeRole::Body, 13.0),
        (TypeRole::Control, 14.0),
        (TypeRole::Title, 15.0),
        (TypeRole::Heading, 18.0),
        (TypeRole::TunerString, 22.0),
        (TypeRole::TunerCents, 44.0),
    ];
    for (role, size) in scale {
        assert_eq!(type_size(role), size, "{role:?}");
    }
    assert_eq!(ts(10.0), 12.0, "ts() still applies the reading scale");
}

#[test]
fn egui_text_styles_follow_the_type_scale() {
    use super::configure_style;
    let ctx = egui::Context::default();
    configure_style(&ctx);
    for theme in [egui::Theme::Dark, egui::Theme::Light] {
        let style = ctx.style_of(theme);
        assert_eq!(
            style.text_styles[&egui::TextStyle::Button].size,
            type_size(TypeRole::Control),
            "{theme:?}: bare buttons and combos must match segmented / outline controls"
        );
        assert_eq!(
            style.text_styles[&egui::TextStyle::Body].size,
            type_size(TypeRole::Body),
            "{theme:?}"
        );
        assert_eq!(
            style.text_styles[&egui::TextStyle::Small].size,
            type_size(TypeRole::Secondary),
            "{theme:?}"
        );
        assert_eq!(
            style.text_styles[&egui::TextStyle::Heading].size,
            type_size(TypeRole::Heading),
            "{theme:?}"
        );
        assert_eq!(
            style.text_styles[&egui::TextStyle::Monospace].size,
            type_size(TypeRole::Secondary),
            "{theme:?}"
        );
    }
}

#[test]
fn font_names_the_bundled_family() {
    let id = font(Face::MonoMedium, TypeRole::Secondary);
    assert_eq!(id.size, 12.0);
    assert_eq!(id.family, egui::FontFamily::Name("mono-medium".into()));
    assert_eq!(
        font(Face::SansSemibold, TypeRole::Heading).family,
        egui::FontFamily::Name("sans-semibold".into())
    );
}

#[test]
fn a_click_is_a_hand_and_a_plain_drag_is_not() {
    assert_eq!(
        hover_cursor(egui::Sense::click(), true),
        Some(egui::CursorIcon::PointingHand)
    );
    assert_eq!(
        hover_cursor(egui::Sense::click_and_drag(), true),
        Some(egui::CursorIcon::PointingHand)
    );
    assert_eq!(hover_cursor(egui::Sense::drag(), true), None);
    assert_eq!(hover_cursor(egui::Sense::hover(), true), None);
    assert_eq!(hover_cursor(egui::Sense::click(), false), None);
}

#[test]
fn palette_follows_the_theme() {
    let ctx = egui::Context::default();
    ctx.set_theme(egui::Theme::Light);
    assert_eq!(palette(&ctx), &LIGHT);
    ctx.set_theme(egui::Theme::Dark);
    assert_eq!(palette(&ctx), &DARK);
}
