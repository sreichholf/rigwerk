//! Bundled IBM Plex subsets (`assets/fonts`, SIL OFL).  egui's fonts stay
//! as the fallback, and Hack is last on the sans side: of those, only Hack
//! draws ⇧, ▴ and ▾, which Plex lacks.

use std::sync::Arc;

use eframe::egui;
use egui::{FontData, FontDefinitions, FontFamily};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Face {
    Sans,
    SansMedium,
    SansSemibold,
    Mono,
    MonoMedium,
}

impl Face {
    pub(crate) const ALL: [Self; 5] = [
        Self::Sans,
        Self::SansMedium,
        Self::SansSemibold,
        Self::Mono,
        Self::MonoMedium,
    ];

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Sans => "sans",
            Self::SansMedium => "sans-medium",
            Self::SansSemibold => "sans-semibold",
            Self::Mono => "mono",
            Self::MonoMedium => "mono-medium",
        }
    }

    pub(crate) fn family(self) -> FontFamily {
        FontFamily::Name(self.name().into())
    }

    fn is_mono(self) -> bool {
        matches!(self, Self::Mono | Self::MonoMedium)
    }

    fn data_key(self) -> &'static str {
        match self {
            Self::Sans => "plex-sans-regular",
            Self::SansMedium => "plex-sans-medium",
            Self::SansSemibold => "plex-sans-semibold",
            Self::Mono => "plex-mono-regular",
            Self::MonoMedium => "plex-mono-medium",
        }
    }

    fn bytes(self) -> &'static [u8] {
        match self {
            Self::Sans => include_bytes!("../../assets/fonts/RigWerkSans-Regular.ttf"),
            Self::SansMedium => include_bytes!("../../assets/fonts/RigWerkSans-Medium.ttf"),
            Self::SansSemibold => include_bytes!("../../assets/fonts/RigWerkSans-SemiBold.ttf"),
            Self::Mono => include_bytes!("../../assets/fonts/RigWerkMono-Regular.ttf"),
            Self::MonoMedium => include_bytes!("../../assets/fonts/RigWerkMono-Medium.ttf"),
        }
    }
}

/// The key egui's default fonts use for Hack.
const HACK: &str = "Hack";

pub(crate) fn definitions() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    let proportional = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let monospace = fonts
        .families
        .get(&FontFamily::Monospace)
        .cloned()
        .unwrap_or_default();
    let mut sans_fallback = proportional;
    if fonts.font_data.contains_key(HACK) && !sans_fallback.iter().any(|name| name == HACK) {
        sans_fallback.push(HACK.to_owned());
    }
    for face in Face::ALL {
        fonts.font_data.insert(
            face.data_key().to_owned(),
            Arc::new(FontData::from_static(face.bytes())),
        );
        let fallback = if face.is_mono() {
            &monospace
        } else {
            &sans_fallback
        };
        let chain = std::iter::once(face.data_key().to_owned())
            .chain(fallback.iter().cloned())
            .collect();
        fonts.families.insert(face.family(), chain);
    }
    for (family, face) in [
        (FontFamily::Proportional, Face::Sans),
        (FontFamily::Monospace, Face::Mono),
    ] {
        let chain = fonts.families[&face.family()].clone();
        fonts.families.insert(family, chain);
    }
    fonts
}

pub(crate) fn install(ctx: &egui::Context) {
    ctx.set_fonts(definitions());
}

#[cfg(test)]
mod tests;
