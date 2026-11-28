//! A kernel is built and dropped here, never inside `set_mix` and never
//! on the audio thread.  A cut or a polarity change rebakes the cache
//! and does not re-read the file.  A rate or path change does.

use crate::app::nam::{
    browse_start, clipboard_models_folder, looks_pasted, normalize_models_folder, pasted_folder,
};
use crate::app::{LogTone, RigWerkApp};
use crate::audio::cab::{
    load_cab_channels, prepare_cab_samples, scan_cab_folder, CabCatalogEntry, CabFile,
};

/// One folder the sheet has listed.  `folder` is what was scanned, so a
/// change of preference is visible without rescanning every frame.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CabCatalog {
    pub folder: String,
    pub entries: Vec<CabCatalogEntry>,
}

/// What the last install attempt was.  A match is left alone.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CabInstall {
    pub path: String,
    pub rate: u32,
    /// [`f32::to_bits`] of the low cut baked into the kernel.
    pub low_cut: u32,
    /// [`f32::to_bits`] of the high cut baked into the kernel.
    pub high_cut: u32,
    pub polarity: bool,
    pub run: u64,
    pub generation: u64,
}

impl RigWerkApp {
    /// Lists `*.wav` files in the impulse folder.  UI thread only.
    pub(crate) fn rescan_cab_folder(&mut self) {
        let folder = normalize_models_folder(&self.cab_folder);
        self.cab_folder = folder.clone();
        let entries = scan_cab_folder(&folder);
        self.cab_catalog = CabCatalog { folder, entries };
        self.cab_rescan = false;
    }

    /// Stores the impulse folder.  The sheet rescans when it next draws.
    pub(crate) fn set_cab_folder(&mut self, folder: String) {
        let folder = normalize_models_folder(&folder);
        if folder != self.cab_folder {
            self.cab_folder = folder;
            self.cab_rescan = true;
        }
    }

    /// A keystroke or a Ctrl+V into the folder field.  A typed path is kept
    /// raw until `commit` (focus left the field): trimming each keystroke
    /// deletes the space the user just typed.  A paste is a whole path and
    /// is normalized immediately.
    pub(crate) fn apply_typed_cab_folder(&mut self, previous: &str, edited: String, commit: bool) {
        if looks_pasted(previous, &edited) {
            let folder = pasted_folder(&edited).unwrap_or_else(|| normalize_models_folder(&edited));
            self.set_cab_folder(folder);
        } else if commit {
            self.cab_folder = normalize_models_folder(&edited);
            self.cab_rescan = true;
        } else if edited != self.cab_folder {
            self.cab_folder = edited;
        }
        if self.cab_rescan {
            self.rescan_cab_folder();
        }
    }

    /// Opens the native folder window, parented to this viewport.
    pub(crate) fn browse_cab_folder(&mut self) {
        let parent = self.native_window;
        let start = browse_start(&self.cab_folder);
        let mut dialog = rfd::FileDialog::new().set_title("Impulse responses");
        if let Some(start) = start {
            dialog = dialog.set_directory(start);
        }
        if let Some(parent) = parent.as_ref() {
            dialog = dialog.set_parent(parent);
        }
        let Some(path) = dialog.pick_folder() else {
            return;
        };
        self.set_cab_folder(path.to_string_lossy().into_owned());
        self.rescan_cab_folder();
    }

    /// Puts the clipboard's path, or a copied folder, into the impulse folder.
    /// False when the clipboard has neither.
    pub(crate) fn paste_cab_folder(&mut self) -> bool {
        let Some(folder) = clipboard_models_folder() else {
            self.push_log("The clipboard has no folder path.", LogTone::Amber);
            return false;
        };
        self.set_cab_folder(folder);
        self.rescan_cab_folder();
        true
    }

    /// Points path `slot` at an impulse and installs it.  A failure keeps
    /// the path string and records a short error; nothing is installed.
    pub(crate) fn set_path_cab_file(&mut self, slot: usize, file: String) {
        let Some(path) = self.paths.get_mut(slot).and_then(Option::as_mut) else {
            return;
        };
        path.cab_file = file.trim().to_owned();
        path.cab_install = CabInstall::default();
        path.cab_raw = None;
        self.sync_cab_slot(slot);
    }

    /// Installs or clears each live path's impulse.  Same path, rate, cuts,
    /// polarity, run and slot generation does nothing.
    pub(crate) fn sync_cab_files(&mut self) {
        let slots: Vec<usize> = self
            .paths
            .iter()
            .enumerate()
            .filter_map(|(slot, path)| path.as_ref().map(|_| slot))
            .collect();
        for slot in slots {
            self.sync_cab_slot(slot);
        }
    }

    fn sync_cab_slot(&mut self, slot: usize) {
        let Some(path) = self.paths.get(slot).and_then(Option::as_ref) else {
            return;
        };
        let rate = self.nam_rate();
        let run = self.engine.run_id();
        let generation = self.engine.path_generation(slot);
        let controls = path.mix.cab.sanitized();
        let wanted = CabInstall {
            path: path.cab_file.clone(),
            rate,
            low_cut: controls.low_cut_hz.to_bits(),
            high_cut: controls.high_cut_hz.to_bits(),
            polarity: controls.polarity,
            run,
            generation,
        };
        if path.cab_install == wanted {
            return;
        }
        let file = path.cab_file.clone();
        let cached = path.cab_raw.as_ref().is_some_and(|(cached_rate, _, _)| {
            *cached_rate == rate && path.cab_install.path == file
        });
        if file.is_empty() {
            self.engine.clear_cab(slot);
            if let Some(path) = self.paths.get_mut(slot).and_then(Option::as_mut) {
                path.cab_name.clear();
                path.cab_error = None;
                path.cab_raw = None;
                path.cab_install = wanted;
            }
            return;
        }
        let samples = if cached {
            self.paths[slot]
                .as_mut()
                .and_then(|path| path.cab_raw.take())
        } else {
            match load_cab_channels(&file, rate) {
                Ok((left, right)) => Some((rate, left, right)),
                Err(error) => {
                    self.engine.clear_cab(slot);
                    if let Some(path) = self.paths.get_mut(slot).and_then(Option::as_mut) {
                        path.cab_name = file_stem(&file);
                        path.cab_error = Some(error);
                        path.cab_raw = None;
                        path.cab_install = wanted;
                    }
                    return;
                }
            }
        };
        let Some((raw_rate, left, right)) = samples else {
            return;
        };
        match prepare_cab_samples(
            &left,
            &right,
            raw_rate,
            f32::from_bits(wanted.low_cut),
            f32::from_bits(wanted.high_cut),
            wanted.polarity,
        ) {
            CabFile::Ready(kernel) => {
                debug_assert!(kernel.left.len() <= crate::audio::CAB_MAX_TAPS);
                debug_assert!(kernel.right.len() <= crate::audio::CAB_MAX_TAPS);
                debug_assert!(
                    kernel.left.len() as f32
                        <= (rate as f32) * crate::audio::CAB_MAX_MS / 1000.0 + 1.0
                );
                self.engine.queue_cab(slot, kernel);
                if let Some(path) = self.paths.get_mut(slot).and_then(Option::as_mut) {
                    path.cab_name = file_stem(&file);
                    path.cab_error = None;
                    path.cab_raw = Some((raw_rate, left, right));
                    path.cab_install = wanted;
                }
            }
            CabFile::Failed(error) => {
                self.engine.clear_cab(slot);
                if let Some(path) = self.paths.get_mut(slot).and_then(Option::as_mut) {
                    path.cab_name = file_stem(&file);
                    path.cab_error = Some(error);
                    path.cab_raw = Some((raw_rate, left, right));
                    path.cab_install = wanted;
                }
            }
        }
    }
}

fn file_stem(path: &str) -> String {
    std::path::Path::new(path)
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| "Cabinet".to_owned())
}

#[cfg(test)]
mod tests;
