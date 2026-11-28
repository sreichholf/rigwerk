//! The RigWerk mark, from the `.ico` files in `assets`.  The compact `RW`
//! is for marks at 64 px and below, where the wordmark does not read.

use eframe::egui;
use egui::{ColorImage, TextureHandle, TextureOptions};

const FULL: &[u8] = include_bytes!("../../assets/rigwerk.ico");
const COMPACT: &[u8] = include_bytes!("../../assets/rigwerk-rw.ico");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mark {
    Full,
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

pub(crate) fn texture(ctx: &egui::Context, mark: Mark) -> TextureHandle {
    let id = egui::Id::unique(("rigwerk-icon", mark.name()));
    if let Some(texture) = ctx.data(|data| data.get_temp::<TextureHandle>(id)) {
        return texture;
    }
    let texture = ctx.load_texture(mark.name(), color_image(mark), TextureOptions::LINEAR);
    ctx.data_mut(|data| data.insert_temp(id, texture.clone()));
    texture
}

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

/// An `.ico` load returns the largest frame.
fn decode(bytes: &[u8]) -> image::RgbaImage {
    image::load_from_memory_with_format(bytes, image::ImageFormat::Ico)
        .expect("the embedded RigWerk icon decodes")
        .to_rgba8()
}

#[cfg(test)]
mod tests;
