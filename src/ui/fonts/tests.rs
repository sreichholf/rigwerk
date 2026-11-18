use eframe::egui;
use egui::epaint::text::{FontDefinitions, Fonts, TextOptions};
use egui::{FontFamily, FontId};

use super::{definitions, Face};

/// Characters the UI writes that every bundled face must draw itself.
const IN_PLEX: &str = "AZaz09 −·→←↑↓±≈Ø∞…‹›×°µ–—“”";
/// Characters Plex lacks; the fallback draws them.
const FROM_FALLBACK: &str = "⇧▴▾";

fn fonts(definitions: FontDefinitions) -> Fonts {
    Fonts::new(TextOptions::default(), definitions)
}

#[test]
fn every_face_is_a_named_family_led_by_its_own_data() {
    let defs = definitions();
    for face in Face::ALL {
        let chain = &defs.families[&face.family()];
        assert_eq!(chain[0], face.data_key(), "{}", face.name());
        assert!(defs.font_data.contains_key(face.data_key()));
        assert!(chain.len() > 1, "{} has no fallback", face.name());
    }
    assert_eq!(
        defs.families[&FontFamily::Proportional],
        defs.families[&Face::Sans.family()]
    );
    assert_eq!(
        defs.families[&FontFamily::Monospace],
        defs.families[&Face::Mono.family()]
    );
}

#[test]
fn egui_default_fonts_stay_as_fallbacks() {
    let defaults = FontDefinitions::default();
    let defs = definitions();
    for (family, face) in [
        (FontFamily::Proportional, Face::Sans),
        (FontFamily::Monospace, Face::Mono),
    ] {
        let chain = &defs.families[&face.family()];
        for name in &defaults.families[&family] {
            assert!(chain.contains(name), "{} lost {name}", face.name());
        }
    }
}

#[test]
fn each_face_draws_the_ui_characters_itself() {
    for face in Face::ALL {
        let mut only = FontDefinitions::empty();
        only.font_data.insert(
            face.data_key().to_owned(),
            definitions().font_data[face.data_key()].clone(),
        );
        only.families
            .insert(FontFamily::Proportional, vec![face.data_key().to_owned()]);
        let mut fonts = fonts(only);
        let id = FontId::proportional(13.0);
        for c in IN_PLEX.chars() {
            assert!(fonts.has_glyph(&id, c), "{} has no {c:?}", face.name());
        }
    }
}

#[test]
fn the_fallback_draws_what_plex_lacks() {
    let mut fonts = fonts(definitions());
    for face in Face::ALL {
        let id = FontId::new(13.0, face.family());
        assert!(
            fonts.has_glyphs(&id, FROM_FALLBACK),
            "{} cannot draw {FROM_FALLBACK}",
            face.name()
        );
    }
}
