use eframe::egui;
use egui::Color32;

use super::{
    font, hex, hover_cursor, palette, ts, type_size, Face, Palette, TypeRole, DARK, LIGHT,
    PATH_COLOUR_NAMES,
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
fn quiet_text_reads_on_a_panel_and_stays_quieter_than_body_text() {
    for (theme, p) in palettes() {
        let on_strip = contrast(p.text3, p.strip);
        assert!(
            on_strip >= 4.5,
            "{theme}: text3 on strip is {on_strip:.3}:1, below WCAG AA 4.5:1"
        );
        let on_slot = contrast(p.text3, p.slot);
        assert!(
            on_slot >= 4.5,
            "{theme}: text3 on slot is {on_slot:.3}:1, below WCAG AA 4.5:1"
        );
        assert!(
            on_strip < contrast(p.text2, p.strip),
            "{theme}: text3 stays quieter than text2"
        );
        assert!(
            contrast(p.text2, p.strip) < contrast(p.text, p.strip),
            "{theme}: text2 stays quieter than text"
        );
    }

    // The blue-grey the quiet token replaced, on the panel it was drawn on.
    let previous = Color32::from_rgb(89, 102, 122);
    assert!(contrast(previous, DARK.strip) < 4.5);
}

#[test]
fn every_token_is_the_value_the_spec_prints() {
    let tokens: [(&str, Color32, u32, Color32, u32); 21] = [
        ("bg", DARK.bg, 0x121211, LIGHT.bg, 0xe9e6df),
        ("bar", DARK.bar, 0x0d0d0c, LIGHT.bar, 0xf4f2ed),
        ("strip", DARK.strip, 0x1a1918, LIGHT.strip, 0xfbfaf7),
        ("slot", DARK.slot, 0x222120, LIGHT.slot, 0xefede8),
        ("well", DARK.well, 0x0f0f0e, LIGHT.well, 0xe4e1da),
        ("sheet", DARK.sheet, 0x1c1b19, LIGHT.sheet, 0xfdfcfa),
        ("line", DARK.line, 0x2a2825, LIGHT.line, 0xdcd8cf),
        ("line2", DARK.line2, 0x3a3733, LIGHT.line2, 0xc9c4b9),
        ("text", DARK.text, 0xebe7df, LIGHT.text, 0x1d1c1a),
        ("text2", DARK.text2, 0xb0aa9f, LIGHT.text2, 0x55514a),
        ("text3", DARK.text3, 0x948e83, LIGHT.text3, 0x6b665d),
        ("led_off", DARK.led_off, 0x7d776e, LIGHT.led_off, 0x857f73),
        ("accent", DARK.accent, 0xff8f40, LIGHT.accent, 0xc55a0c),
        (
            "accent_text",
            DARK.accent_text,
            0xffb27a,
            LIGHT.accent_text,
            0xa8480a,
        ),
        (
            "accent_bg",
            DARK.accent_bg,
            0x2a1a0e,
            LIGHT.accent_bg,
            0xfbeee2,
        ),
        ("warn", DARK.warn, 0xffcc4d, LIGHT.warn, 0x7a5700),
        ("error", DARK.error, 0xff5a4a, LIGHT.error, 0xd93a2b),
        (
            "error_text",
            DARK.error_text,
            0xff5a4a,
            LIGHT.error_text,
            0xb8291c,
        ),
        ("ok_text", DARK.ok_text, 0x9cc48a, LIGHT.ok_text, 0x446b33),
        ("meter", DARK.meter, 0xcfc8bb, LIGHT.meter, 0x4a463f),
        (
            "meter_hot",
            DARK.meter_hot,
            0xffcc4d,
            LIGHT.meter_hot,
            0x7a5700,
        ),
    ];
    for (name, dark, dark_hex, light, light_hex) in tokens {
        assert_eq!(dark, hex(dark_hex), "dark `{name}`");
        assert_eq!(light, hex(light_hex), "light `{name}`");
    }
    let paths = [
        ("Blue", 0x7fb2d6, 0x3f7fae),
        ("Violet", 0xc49bd8, 0x8d5bb0),
        ("Green", 0x9cc48a, 0x5f8f4a),
        ("Rose", 0xe39aa6, 0xb5566a),
    ];
    assert_eq!(paths.len(), DARK.path.len());
    for (index, (name, dark, light)) in paths.into_iter().enumerate() {
        assert_eq!(DARK.path[index], hex(dark), "dark path `{name}`");
        assert_eq!(LIGHT.path[index], hex(light), "light path `{name}`");
    }
    assert_eq!(PATH_COLOUR_NAMES, paths.map(|(name, _, _)| name));
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
            ("ok_text", p.ok_text),
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

/// The word a path colour reads as, from its own channels: the channel it
/// carries most names it, and a blue with more red than green is the violet.
fn path_colour_name(colour: Color32) -> &'static str {
    let (r, g, b) = (colour.r(), colour.g(), colour.b());
    if g > r && g > b {
        "Green"
    } else if r > b {
        "Rose"
    } else if r > g {
        "Violet"
    } else {
        "Blue"
    }
}

#[test]
fn every_path_colour_has_its_name_in_order() {
    for (theme, p) in palettes() {
        assert_eq!(PATH_COLOUR_NAMES.len(), p.path.len(), "{theme}");
        for (index, colour) in p.path.iter().enumerate() {
            assert_eq!(
                path_colour_name(*colour),
                PATH_COLOUR_NAMES[index],
                "{theme}: path colour {index} reads as the wrong name"
            );
        }
    }
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
        (TypeRole::TunerNote, 104.0),
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
