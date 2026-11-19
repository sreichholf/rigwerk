//! The UI-thread side of the built-in amp: the models folder, the scan,
//! and installing one capture on a path.
//!
//! A [`nam_rs::Model`] is built and dropped here, never inside `set_mix`
//! and never on the audio thread.  The same path at the same sample rate
//! on the same run is not rebuilt.  Neither is a width change that keeps
//! the same submodel; one that picks another queues a model built at it.
//! The folder window and the clipboard read run on the UI thread too.

use std::path::{Path, PathBuf};

use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawDisplayHandle,
    RawWindowHandle, WindowHandle,
};

use crate::app::{LogTone, RigWerkApp};
use crate::audio::amp::{
    prepare_amp_file, scan_amp_folder, slim_submodel, AmpCatalogEntry, AmpFile,
};

/// The viewport the folder window is parented to, so it opens in front.
/// Copied from the frame at the start of each pass.
#[derive(Clone, Copy)]
pub(crate) struct NativeWindow {
    window: RawWindowHandle,
    display: RawDisplayHandle,
}

impl HasWindowHandle for NativeWindow {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        Ok(unsafe {
            // The handle was copied from the live frame this pass.  The
            // dialog runs before that pass returns.
            WindowHandle::borrow_raw(self.window)
        })
    }
}

impl HasDisplayHandle for NativeWindow {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        Ok(unsafe {
            // Same lifetime as the window handle above.
            DisplayHandle::borrow_raw(self.display)
        })
    }
}

/// A typed or pasted folder, ready to scan.  Strips a copied path's
/// quotes and a `file://` URI, and a `.nam` file becomes its folder.
pub(crate) fn normalize_amp_folder(raw: &str) -> String {
    let line = raw
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .trim_start_matches('\u{feff}');
    let mut text = strip_wrapping_quotes(line).to_owned();
    if let Some(path) = file_uri_path(&text) {
        text = path;
    }
    let path = Path::new(&text);
    if path.is_file() {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            return parent.to_string_lossy().into_owned();
        }
    }
    text
}

/// One folder the sheet has listed.  `folder` is what was scanned, so a
/// change of preference is visible without rescanning every frame.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct AmpCatalog {
    pub folder: String,
    pub entries: Vec<AmpCatalogEntry>,
}

/// What the last install attempt was.  A match is left alone.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct AmpInstall {
    pub path: String,
    pub rate: u32,
    pub run: u64,
    pub generation: u64,
    /// The submodel of a slimmable capture.  0 otherwise.
    pub submodel: usize,
}

impl RigWerkApp {
    /// The rate a path is running at: the opened output, else the planned
    /// output, else the session rate.  A capture that names another rate
    /// is not installed.
    fn amp_rate(&self) -> u32 {
        let opened = self.engine.opened_output_rate();
        if opened > 0 {
            opened
        } else if self.output_rate > 0 {
            self.output_rate
        } else {
            self.sample_rate.max(1)
        }
    }

    /// Remembers the viewport so Browse can parent the folder window.
    pub(crate) fn remember_native_window(&mut self, frame: &eframe::Frame) {
        let Ok(window) = frame.window_handle() else {
            return;
        };
        let Ok(display) = frame.display_handle() else {
            return;
        };
        self.native_window = Some(NativeWindow {
            window: window.as_raw(),
            display: display.as_raw(),
        });
    }

    /// Lists `*.nam` files in the models folder.  UI thread only.
    pub(crate) fn rescan_amp_folder(&mut self) {
        let folder = normalize_amp_folder(&self.amp_folder);
        self.amp_folder = folder.clone();
        let entries = scan_amp_folder(&folder);
        self.amp_catalog = AmpCatalog { folder, entries };
        self.amp_rescan = false;
    }

    /// Stores the models folder.  The sheet rescans when it next draws.
    pub(crate) fn set_amp_folder(&mut self, folder: String) {
        let folder = normalize_amp_folder(&folder);
        if folder != self.amp_folder {
            self.amp_folder = folder;
            self.amp_rescan = true;
        }
    }

    /// A keystroke or a Ctrl+V into the folder field.  A multi-character
    /// change that is not itself a directory may be Explorer's copied
    /// folder, whose clipboard text is only the name.
    pub(crate) fn apply_typed_amp_folder(&mut self, previous: &str, edited: String) {
        let folder = if looks_pasted(previous, &edited) {
            pasted_folder(&edited).unwrap_or_else(|| normalize_amp_folder(&edited))
        } else {
            normalize_amp_folder(&edited)
        };
        self.set_amp_folder(folder);
        if self.amp_rescan {
            self.rescan_amp_folder();
        }
    }

    /// Opens the native folder window, parented to this viewport.
    pub(crate) fn browse_amp_folder(&mut self) {
        let parent = self.native_window;
        let start = browse_start(&self.amp_folder);
        let mut dialog = rfd::FileDialog::new().set_title("Models folder");
        if let Some(start) = start {
            dialog = dialog.set_directory(start);
        }
        if let Some(parent) = parent.as_ref() {
            dialog = dialog.set_parent(parent);
        }
        let Some(path) = dialog.pick_folder() else {
            return;
        };
        self.set_amp_folder(path.to_string_lossy().into_owned());
        self.rescan_amp_folder();
    }

    /// Puts the clipboard's path, or a copied folder, into the models folder.
    /// False when the clipboard has neither.
    pub(crate) fn paste_amp_folder(&mut self) -> bool {
        let Some(folder) = clipboard_models_folder() else {
            self.push_log("The clipboard has no folder path.", LogTone::Amber);
            return false;
        };
        self.set_amp_folder(folder);
        self.rescan_amp_folder();
        true
    }

    /// Points path `slot` at a capture and installs it.  A failure keeps
    /// the path string and records a short error; nothing is installed.
    pub(crate) fn set_path_amp_model(&mut self, slot: usize, model_path: String) {
        let Some(path) = self.paths.get_mut(slot).and_then(Option::as_mut) else {
            return;
        };
        path.amp_model = model_path.trim().to_owned();
        path.amp_install = AmpInstall::default();
        self.sync_amp_slot(slot);
    }

    /// Installs or clears each live path's capture.  Same path, rate, run,
    /// slot generation and submodel does nothing.
    pub(crate) fn sync_amp_models(&mut self) {
        let slots: Vec<usize> = self
            .paths
            .iter()
            .enumerate()
            .filter_map(|(slot, path)| path.as_ref().map(|_| slot))
            .collect();
        for slot in slots {
            self.sync_amp_slot(slot);
        }
    }

    fn sync_amp_slot(&mut self, slot: usize) {
        let Some(path) = self.paths.get(slot).and_then(Option::as_ref) else {
            return;
        };
        let rate = self.amp_rate();
        let run = self.engine.run_id();
        let generation = self.engine.path_generation(slot);
        let slim = path.mix.amp.slim;
        let mut wanted = AmpInstall {
            path: path.amp_model.clone(),
            rate,
            run,
            generation,
            submodel: slim_submodel(&path.amp_slim_steps, slim),
        };
        if path.amp_install == wanted {
            return;
        }
        let model_path = path.amp_model.clone();
        if model_path.is_empty() {
            self.engine.clear_amp(slot);
            if let Some(path) = self.paths.get_mut(slot).and_then(Option::as_mut) {
                path.amp_name.clear();
                path.amp_error = None;
                path.amp_slim_steps.clear();
                path.amp_mismatch = false;
                path.amp_file_rate = 0;
                path.amp_install = AmpInstall {
                    submodel: 0,
                    ..wanted
                };
            }
            return;
        }
        match prepare_amp_file(&model_path, rate, slim) {
            AmpFile::Ready(loaded) => {
                self.engine.queue_amp(slot, loaded.prepared);
                // The stamp names the submodel this model was built at.
                wanted.submodel = slim_submodel(&loaded.slim_steps, slim);
                if let Some(path) = self.paths.get_mut(slot).and_then(Option::as_mut) {
                    path.amp_name = loaded.name;
                    path.amp_error = None;
                    path.amp_slim_steps = loaded.slim_steps;
                    path.amp_mismatch = false;
                    path.amp_file_rate = loaded.file_rate;
                    path.amp_install = wanted;
                }
            }
            AmpFile::RateMismatch { file_rate, name } => {
                self.engine.clear_amp(slot);
                if let Some(path) = self.paths.get_mut(slot).and_then(Option::as_mut) {
                    path.amp_name = name;
                    path.amp_error = None;
                    path.amp_slim_steps.clear();
                    path.amp_mismatch = true;
                    path.amp_file_rate = file_rate;
                    path.amp_install = AmpInstall {
                        submodel: 0,
                        ..wanted
                    };
                }
            }
            AmpFile::Failed(error) => {
                self.engine.clear_amp(slot);
                let stem = std::path::Path::new(&model_path)
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
                    .filter(|stem| !stem.is_empty())
                    .unwrap_or_else(|| "Amp".to_owned());
                if let Some(path) = self.paths.get_mut(slot).and_then(Option::as_mut) {
                    path.amp_name = stem;
                    path.amp_error = Some(error);
                    path.amp_slim_steps.clear();
                    path.amp_mismatch = false;
                    path.amp_file_rate = 0;
                    path.amp_install = AmpInstall {
                        submodel: 0,
                        ..wanted
                    };
                }
            }
        }
    }
}

/// Rising edge of Ctrl+V.  `down` is the chord this frame and `held` is
/// last frame, tracked whether or not the field is active, so focusing it
/// while the keys are already down does not paste.
pub(crate) fn paste_chord_edge(active: bool, down: bool, held: bool) -> (bool, bool) {
    (active && down && !held, down)
}

/// Ctrl and V are down.  egui eats that chord when the clipboard has no
/// text, and a folder copied in Explorer is only a file list.
pub(crate) fn ctrl_v_is_down() -> bool {
    #[cfg(test)]
    if CTRL_V_TEST.with(|down| down.get()) {
        return true;
    }
    platform_ctrl_v_down()
}

#[cfg(test)]
thread_local! {
    static CTRL_V_TEST: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Test stand-in for the key, so a frame can paste with no egui event.
#[cfg(test)]
pub(crate) fn set_ctrl_v_for_test(down: bool) {
    CTRL_V_TEST.with(|cell| cell.set(down));
}

fn platform_ctrl_v_down() -> bool {
    #[cfg(windows)]
    {
        use winapi::um::winuser::{GetAsyncKeyState, VK_CONTROL, VK_MENU, VK_SHIFT};
        // The high bit is set while the key is down.  No pointer is passed.
        // 0x56 is V.  Shift and Alt stay out so this is Ctrl+V only.
        unsafe {
            GetAsyncKeyState(VK_CONTROL) < 0
                && GetAsyncKeyState(0x56) < 0
                && GetAsyncKeyState(VK_SHIFT) >= 0
                && GetAsyncKeyState(VK_MENU) >= 0
        }
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn looks_pasted(previous: &str, edited: &str) -> bool {
    edited.chars().count() > previous.chars().count().saturating_add(1)
}

/// The edited text is a directory, or it matches the clipboard and the
/// clipboard also holds a copied folder.
fn pasted_folder(edited: &str) -> Option<String> {
    let normalized = normalize_amp_folder(edited);
    if Path::new(&normalized).is_dir() {
        return Some(normalized);
    }
    let mut board = arboard::Clipboard::new().ok()?;
    let text = board.get_text().ok()?;
    let same = normalize_amp_folder(&text) == normalized || text.trim() == edited.trim();
    if !same {
        return None;
    }
    file_list_folder(&mut board)
}

fn clipboard_models_folder() -> Option<String> {
    let mut board = arboard::Clipboard::new().ok()?;
    let text = board.get_text().ok();
    if let Some(folder) = text.as_deref().map(normalize_amp_folder) {
        if Path::new(&folder).is_dir() {
            return Some(folder);
        }
    }
    if let Some(folder) = file_list_folder(&mut board) {
        return Some(folder);
    }
    let folder = normalize_amp_folder(text.as_deref()?);
    if folder.is_empty() {
        None
    } else {
        Some(folder)
    }
}

fn file_list_folder(board: &mut arboard::Clipboard) -> Option<String> {
    let path = board.get().file_list().ok()?.into_iter().next()?;
    let folder = if path.is_dir() {
        path
    } else {
        path.parent()?.to_path_buf()
    };
    let folder = normalize_amp_folder(&folder.to_string_lossy());
    if folder.is_empty() {
        None
    } else {
        Some(folder)
    }
}

fn browse_start(folder: &str) -> Option<PathBuf> {
    let path = PathBuf::from(folder);
    if path.is_dir() {
        Some(path)
    } else {
        path.parent()
            .filter(|parent| parent.is_dir())
            .map(Path::to_path_buf)
    }
}

fn strip_wrapping_quotes(text: &str) -> &str {
    let bytes = text.as_bytes();
    if bytes.len() >= 2 {
        let quote = bytes[0];
        if (quote == b'"' || quote == b'\'') && bytes[bytes.len() - 1] == quote {
            return text[1..text.len() - 1].trim();
        }
    }
    text
}

fn file_uri_path(text: &str) -> Option<String> {
    let (scheme, rest) = text.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("file") {
        return None;
    }
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    Some(drive_or_absolute(&percent_decode(rest)))
}

fn drive_or_absolute(path: &str) -> String {
    let bytes = path.as_bytes();
    if bytes.len() >= 4
        && bytes[0] == b'/'
        && bytes[1].is_ascii_alphabetic()
        && bytes[2] == b':'
        && (bytes[3] == b'/' || bytes[3] == b'\\')
    {
        return path[1..].to_owned();
    }
    if path.starts_with('/') || path.is_empty() {
        path.to_owned()
    } else {
        format!("/{path}")
    }
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) =
                (hex_digit(bytes[index + 1]), hex_digit(bytes[index + 2]))
            {
                out.push((high << 4) | low);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
