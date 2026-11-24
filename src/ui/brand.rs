//! The RigWerk mark, embedded from the `.ico` files in `assets`.
//!
//! The full icon carries the wordmark and is the native window icon and the
//! mark in About.  The compact `RW` icon is only for the title-bar mark,
//! where the wordmark would be too small to read.

use eframe::egui;
use egui::{ColorImage, TextureHandle, TextureOptions};

/// The app icon with the wordmark on the pedal.
const FULL: &[u8] = include_bytes!("../../assets/rigwerk.ico");
/// The `RW` variant, for marks at 64 px and below.
const COMPACT: &[u8] = include_bytes!("../../assets/rigwerk-rw.ico");

/// Which embedded icon a caller wants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mark {
    /// The full wordmark.
    Full,
    /// The `RW` variant.
    Compact,
}

impl Mark {
    fn bytes(self) -> &'static [u8] {
        match self {
            Self::Full => FULL,
            Self::Compact => COMPACT,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Full => "rigwerk-mark",
            Self::Compact => "rigwerk-mark-rw",
        }
    }
}

/// The mark as a texture, uploaded on first use and kept for later frames.
pub(crate) fn texture(ctx: &egui::Context, mark: Mark) -> TextureHandle {
    let id = egui::Id::unique(("rigwerk-icon", mark.name()));
    if let Some(texture) = ctx.data(|data| data.get_temp::<TextureHandle>(id)) {
        return texture;
    }
    let texture = ctx.load_texture(mark.name(), color_image(mark), TextureOptions::LINEAR);
    ctx.data_mut(|data| data.insert_temp(id, texture.clone()));
    texture
}

/// The RGBA image for the native window icon.
pub(crate) fn window_icon() -> egui::IconData {
    let rgba = decode(FULL);
    egui::IconData {
        width: rgba.width(),
        height: rgba.height(),
        rgba: rgba.into_raw(),
    }
}

fn color_image(mark: Mark) -> ColorImage {
    let rgba = decode(mark.bytes());
    ColorImage::from_rgba_unmultiplied(
        [rgba.width() as usize, rgba.height() as usize],
        rgba.as_raw(),
    )
}

/// The largest frame of an embedded `.ico`.
fn decode(bytes: &[u8]) -> image::RgbaImage {
    image::load_from_memory_with_format(bytes, image::ImageFormat::Ico)
        .expect("the embedded RigWerk icon decodes")
        .to_rgba8()
}

#[cfg(test)]
mod tests;
