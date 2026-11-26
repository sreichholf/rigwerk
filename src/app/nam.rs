//! The UI-thread side of the NAM blocks: the models folder, the scan, the
//! loader thread, and the block edits the sheet and the console call.
//!
//! A [`nam_rs::Model`](nam_rs::Model) is built, warmed and measured on the
//! loader thread ([`NamLoader`]), never inside `set_mix` and never on the
//! audio thread.  The loader hands a finished model back; the UI queues it
//! for the block's id, and a stale answer (the block changed while the model
//! was being built) is dropped here, where a model is safe to drop.
//!
//! The same path at the same sample rate, run and dilation factor is not
//! rebuilt.  Neither is a width change that keeps the same submodel; one
//! that picks another queues a model built at it.  When a path's NAM series
//! crosses into oversampling (two or more eligible captures in a row), every
//! block of it is rebuilt at factor 2 and fades in through the same install;
//! back at one, it is rebuilt at factor 1.  The folder window and the
//! clipboard read run on the UI thread too.
//!
//! A capture is timed once per bridge run.  One never measured is measured
//! before its model goes live.  One measured in an earlier run goes live at
//! once with that figure and is measured again when the loader has no
//! install waiting, so a start never plays dry while a figure is refreshed.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawDisplayHandle,
    RawWindowHandle, WindowHandle,
};

use crate::app::chain::ChainSelection;
use crate::app::paths::NamBlock;
use crate::app::screen::Sheet;
use crate::app::{LogTone, RigWerkApp};
use crate::audio::nam::{
    prepare_nam_file, scan_nam_folder, slim_submodel, NamCatalogEntry, NamFile, NamLoad,
};
use crate::audio::{NamBlockControls, MAX_NAM_BLOCKS, NAM_SOFT_LIMIT};

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
pub(crate) fn normalize_models_folder(raw: &str) -> String {
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
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct NamCatalog {
    pub folder: String,
    pub entries: Vec<NamCatalogEntry>,
}

/// What the last install attempt for one block was.  A match is left alone.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct NamInstall {
    pub path: String,
    pub rate: u32,
    pub run: u64,
    pub generation: u64,
    /// The submodel of a slimmable capture.  0 otherwise.
    pub submodel: usize,
    /// 2 when the capture is built with every layer dilation doubled for an
    /// oversampled group, 1 otherwise.
    pub factor: u8,
}

/// One block's capture, as the loader thread builds it.
#[derive(Clone)]
struct NamJob {
    slot: usize,
    id: u32,
    stamp: NamInstall,
    path: String,
    rate: u32,
    slim: f32,
    /// The dilation scale to build at: 1, or 2 for an oversampled group.
    factor: u8,
    /// The block size to measure the capture at.
    frames: usize,
}

/// A finished build, on its way back to the UI thread.
struct NamDone {
    slot: usize,
    id: u32,
    stamp: NamInstall,
    answer: NamAnswer,
}

/// What the loader sends back for one job.
enum NamAnswer {
    /// The capture, built (or why it was not).  Boxed: a figure alone is
    /// small.
    Built(Box<NamFile>),
    /// A fresh figure for a capture already live, measured in this run.
    Remeasured(NamLoad),
}

/// What the loader does about the cost of a capture it builds, from what it
/// measured before (the figure and the run it was taken in).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum NamMeasure {
    /// Measured in this run: use it.
    Known(NamLoad),
    /// Measured in an earlier run: use it, and measure again once no
    /// install waits.
    Refresh(NamLoad),
    /// Never measured: measure before the model goes live.
    Now,
}

impl NamMeasure {
    pub(crate) fn plan(cached: Option<(NamLoad, u64)>, run: u64) -> Self {
        match cached {
            Some((load, measured_in)) if measured_in == run => Self::Known(load),
            Some((load, _)) => Self::Refresh(load),
            None => Self::Now,
        }
    }
}

/// The thread that parses, builds, warms and measures captures.  One per
/// app, started with the first capture and stopped when the app drops.
pub(crate) struct NamLoader {
    jobs: Sender<NamJob>,
    done: Receiver<NamDone>,
}

impl NamLoader {
    /// Starts the thread.  `None` when the thread cannot be started, in
    /// which case nothing is ever loaded and the sheet says so.
    fn spawn() -> Option<Self> {
        let (jobs, job_queue) = mpsc::channel::<NamJob>();
        let (results, done) = mpsc::channel::<NamDone>();
        std::thread::Builder::new()
            .name("rigwerk-nam".to_owned())
            .spawn(move || {
                // Measured costs, by (file, rate, block size, width,
                // factor), with the run each was taken in: a second block on
                // the same build does not measure again, and a figure from
                // an earlier run is refreshed (see the module docs).
                type Key = (String, u32, usize, u32, u8);
                let mut measured: HashMap<Key, (NamLoad, u64)> = HashMap::new();
                // Live captures whose figure is from an earlier run.  An
                // install always goes first.
                let mut refresh: VecDeque<NamJob> = VecDeque::new();
                loop {
                    let (job, install) = match job_queue.try_recv() {
                        Ok(job) => (job, true),
                        Err(TryRecvError::Disconnected) => break,
                        Err(TryRecvError::Empty) => match refresh.pop_front() {
                            Some(job) => (job, false),
                            None => match job_queue.recv() {
                                Ok(job) => (job, true),
                                Err(_) => break,
                            },
                        },
                    };
                    let key = (
                        job.path.clone(),
                        job.rate,
                        job.frames,
                        job.slim.to_bits(),
                        job.factor,
                    );
                    let run = job.stamp.run;
                    let plan = NamMeasure::plan(measured.get(&key).copied(), run);
                    let answer = if install {
                        let now = plan == NamMeasure::Now;
                        let mut file = prepare_nam_file(
                            &job.path,
                            job.rate,
                            job.slim,
                            now.then_some(job.frames),
                            job.factor,
                        );
                        if let NamFile::Ready(loaded) = &mut file {
                            match plan {
                                NamMeasure::Known(load) => loaded.load = Some(load),
                                NamMeasure::Refresh(load) => {
                                    loaded.load = Some(load);
                                    refresh.push_back(job.clone());
                                }
                                NamMeasure::Now => {
                                    if let Some(load) = loaded.load {
                                        measured.insert(key, (load, run));
                                    }
                                }
                            }
                        }
                        NamAnswer::Built(Box::new(file))
                    } else if let NamMeasure::Known(load) = plan {
                        // Another block on the same build was measured
                        // in this run already.
                        NamAnswer::Remeasured(load)
                    } else {
                        let file = prepare_nam_file(
                            &job.path,
                            job.rate,
                            job.slim,
                            Some(job.frames),
                            job.factor,
                        );
                        // Only the figure is wanted: the model built to time
                        // was never live and is dropped here.
                        let NamFile::Ready(loaded) = file else {
                            continue;
                        };
                        let Some(load) = loaded.load else {
                            continue;
                        };
                        measured.insert(key, (load, run));
                        NamAnswer::Remeasured(load)
                    };
                    let done = NamDone {
                        slot: job.slot,
                        id: job.id,
                        stamp: job.stamp,
                        answer,
                    };
                    if results.send(done).is_err() {
                        break;
                    }
                }
            })
            .ok()
            .map(|_| Self { jobs, done })
    }

    /// Asks for one capture.  `false` when the loader is gone.
    fn request(&self, job: NamJob) -> bool {
        self.jobs.send(job).is_ok()
    }
}

impl RigWerkApp {
    /// The rate a path is running at: the opened output, else the planned
    /// output, else the session rate.  A capture that names another rate
    /// is not installed.
    pub(crate) fn nam_rate(&self) -> u32 {
        let opened = self.engine.opened_output_rate();
        if opened > 0 {
            opened
        } else if self.output_rate > 0 {
            self.output_rate
        } else {
            self.sample_rate.max(1)
        }
    }

    /// The block size a capture is measured at: the step the running output
    /// opened, which is the block the mixer runs the NAM blocks in, else the
    /// requested output buffer.  A requested size is not a measurement, so
    /// the opened one wins whenever there is one.
    pub(crate) fn nam_frames(&self) -> usize {
        self.output_step()
            .map_or(self.output_buffer, |(frames, _)| frames as usize)
            .max(1)
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
    pub(crate) fn rescan_models_folder(&mut self) {
        let folder = normalize_models_folder(&self.nam_folder);
        self.nam_folder = folder.clone();
        let entries = scan_nam_folder(&folder);
        self.nam_catalog = NamCatalog { folder, entries };
        self.nam_rescan = false;
    }

    /// Stores the models folder.  The sheet rescans when it next draws.
    pub(crate) fn set_models_folder(&mut self, folder: String) {
        let folder = normalize_models_folder(&folder);
        if folder != self.nam_folder {
            self.nam_folder = folder;
            self.nam_rescan = true;
        }
    }

    /// A keystroke or a Ctrl+V into the folder field.  A multi-character
    /// change that is not itself a directory may be Explorer's copied
    /// folder, whose clipboard text is only the name.
    pub(crate) fn apply_typed_models_folder(&mut self, previous: &str, edited: String) {
        let folder = if looks_pasted(previous, &edited) {
            pasted_folder(&edited).unwrap_or_else(|| normalize_models_folder(&edited))
        } else {
            normalize_models_folder(&edited)
        };
        self.set_models_folder(folder);
        if self.nam_rescan {
            self.rescan_models_folder();
        }
    }

    /// Opens the native folder window, parented to this viewport.
    pub(crate) fn browse_models_folder(&mut self) {
        let parent = self.native_window;
        let start = browse_start(&self.nam_folder);
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
        self.set_models_folder(path.to_string_lossy().into_owned());
        self.rescan_models_folder();
    }

    /// Puts the clipboard's path, or a copied folder, into the models folder.
    /// False when the clipboard has neither.
    pub(crate) fn paste_models_folder(&mut self) -> bool {
        let Some(folder) = clipboard_models_folder() else {
            self.push_log("The clipboard has no folder path.", LogTone::Amber);
            return false;
        };
        self.set_models_folder(folder);
        self.rescan_models_folder();
        true
    }

    /// Adds a block to path `slot`: an empty one, enabled, before the
    /// effects, with the next free id.  `None` at
    /// [`MAX_NAM_BLOCKS`](crate::audio::MAX_NAM_BLOCKS), with a log line.
    pub(crate) fn add_nam_block(&mut self, slot: usize) -> Option<usize> {
        let path = self.paths.get_mut(slot).and_then(Option::as_mut)?;
        if path.nam.len() >= MAX_NAM_BLOCKS {
            self.push_log(
                format!("{MAX_NAM_BLOCKS} NAM blocks is the most one path runs."),
                LogTone::Amber,
            );
            return None;
        }
        let id = path.next_nam_id;
        path.next_nam_id = path.next_nam_id.wrapping_add(1);
        let mut stack = path.nam_stack();
        if !stack.push(NamBlockControls {
            id,
            ..NamBlockControls::default()
        }) {
            return None;
        }
        path.nam.push(NamBlock::new(id));
        let index = stack.index_of(id)?;
        path.order_nam_like(&stack);
        self.push_paths_to_engine();
        Some(index)
    }

    /// Adds a block, asking once more from the fifth on: a path that heavy
    /// is a DSP decision, not an accident.  `None` when the stack is full,
    /// or when the click only armed the confirmation
    /// ([`Self::nam_confirm_pending`]).
    pub(crate) fn request_nam_block(&mut self, slot: usize) -> Option<usize> {
        let path = self.paths.get(slot).and_then(Option::as_ref)?;
        if path.nam.len() >= MAX_NAM_BLOCKS {
            self.push_log(
                format!("{MAX_NAM_BLOCKS} NAM blocks is the most one path runs."),
                LogTone::Amber,
            );
            return None;
        }
        // The block about to be the fifth one is what asks.
        if path.nam.len() + 1 >= NAM_SOFT_LIMIT && self.nam_confirm != Some(slot) {
            self.nam_confirm = Some(slot);
            return None;
        }
        self.nam_confirm = None;
        self.add_nam_block(slot)
    }

    /// Whether the sheet is waiting for a second click before it adds
    /// another block to `slot`.
    pub(crate) fn nam_confirm_pending(&self, slot: usize) -> bool {
        self.nam_confirm == Some(slot)
    }

    pub(crate) fn cancel_nam_block(&mut self) {
        self.nam_confirm = None;
    }

    /// Removes the block at `index`.  The audio side parks the model when it
    /// sees the block gone.  An open sheet, or a selection, on this block
    /// moves to the one before it.
    pub(crate) fn remove_nam_block(&mut self, slot: usize, index: usize) -> bool {
        let Some(path) = self.paths.get_mut(slot).and_then(Option::as_mut) else {
            return false;
        };
        let before = path.nam_ids();
        let mut stack = path.nam_stack();
        if stack.remove(index).is_none() {
            return false;
        }
        let name = path.nam.get(index).map_or_else(
            || "NAM block".to_owned(),
            |block| {
                if block.name.is_empty() {
                    "NAM block".to_owned()
                } else {
                    block.name.clone()
                }
            },
        );
        path.order_nam_like(&stack);
        self.push_log(format!("Removed NAM block {name}."), LogTone::Muted);
        self.follow_nam_blocks(slot, &before);
        self.push_paths_to_engine();
        true
    }

    /// Moves the block at `index` `by` places up (`by < 0`) or down inside
    /// its group, stopping at the group's edge.  The block's new index;
    /// `None` when it did not move.
    pub(crate) fn move_nam_block(&mut self, slot: usize, index: usize, by: isize) -> Option<usize> {
        let path = self.paths.get_mut(slot).and_then(Option::as_mut)?;
        let mut stack = path.nam_stack();
        let id = stack.get(index)?.id;
        let mut at = index;
        for _ in 0..by.unsigned_abs() {
            if !stack.move_block(at, by.signum()) {
                break;
            }
            at = stack.index_of(id)?;
        }
        if at == index {
            return None;
        }
        let before = path.nam_ids();
        path.order_nam_like(&stack);
        self.follow_nam_blocks(slot, &before);
        self.push_paths_to_engine();
        Some(at)
    }

    /// Applies edited controls to the block at `index`.  Does not load a
    /// capture; a width change that picks another submodel does.  The
    /// block's index afterwards: a block that changed sides moved.
    pub(crate) fn set_nam_controls(
        &mut self,
        slot: usize,
        index: usize,
        controls: NamBlockControls,
    ) -> Option<usize> {
        let path = self.paths.get_mut(slot).and_then(Option::as_mut)?;
        let block = path.nam.get(index)?;
        let controls = controls.sanitized();
        let normalize_changed = block.controls.normalize != controls.normalize;
        let id = block.controls.id;
        let before = path.nam_ids();
        if block.controls.position != controls.position {
            // A block that changed sides moves to the end of its new group,
            // so the stack and the list stay one order.  The stack is built
            // from the old positions: `index` names this block only there.
            let mut stack = path.nam_stack();
            stack.set_position(index, controls.position);
            path.order_nam_like(&stack);
        }
        let moved = path.nam.iter().position(|block| block.controls.id == id)?;
        path.nam[moved].controls = controls;
        self.follow_nam_blocks(slot, &before);
        self.push_paths_to_engine();
        if normalize_changed
            && self
                .paths
                .get(slot)
                .and_then(Option::as_ref)
                .is_some_and(|path| path.mix.match_db.abs() >= 0.05)
        {
            self.push_log(
                "NAM normalization changed; match the paths again because normalization happens before the match.",
                LogTone::Amber,
            );
        }
        self.sync_nam_slot(slot);
        Some(moved)
    }

    /// Keeps the open NAM sheet and the chain selection of `slot` on the
    /// block they showed before an edit reordered or removed blocks.
    /// `before` is the ids in their old order.  One on a removed block
    /// moves to the block before it.
    fn follow_nam_blocks(&mut self, slot: usize, before: &[u32]) {
        let Some(path) = self.paths.get(slot).and_then(Option::as_ref) else {
            return;
        };
        let follow = |index: usize| -> Option<usize> {
            let id = *before.get(index)?;
            Some(
                path.nam
                    .iter()
                    .position(|block| block.controls.id == id)
                    .unwrap_or(index.saturating_sub(1)),
            )
        };
        if let Some(Sheet::Nam { path: open, index }) = self.chrome.sheet {
            if let Some(index) = follow(index).filter(|_| open == slot) {
                self.chrome.sheet = Some(Sheet::Nam { path: slot, index });
            }
        }
        if let ChainSelection::Nam(index) = self.chain_selected {
            if let Some(index) = follow(index).filter(|_| self.selected_path == slot) {
                self.chain_selected = ChainSelection::Nam(index).clamp_nam(path.nam.len());
            }
        }
    }

    /// Points the block at `index` at a capture and installs it.  A failure
    /// keeps the path string and records a short error; nothing is
    /// installed.
    pub(crate) fn set_nam_model(&mut self, slot: usize, index: usize, model_path: String) {
        let Some(path) = self.paths.get_mut(slot).and_then(Option::as_mut) else {
            return;
        };
        let Some(block) = path.nam.get_mut(index) else {
            return;
        };
        block.model = model_path.trim().to_owned();
        block.install = NamInstall::default();
        block.oversample = false;
        self.sync_nam_slot(slot);
    }

    /// Installs or clears each live path's captures.  Same path, rate, run,
    /// generation and submodel does nothing.
    pub(crate) fn sync_nam_models(&mut self) {
        let slots: Vec<usize> = self
            .paths
            .iter()
            .enumerate()
            .filter_map(|(slot, path)| path.as_ref().map(|_| slot))
            .collect();
        for slot in slots {
            self.sync_nam_slot(slot);
        }
    }

    fn sync_nam_slot(&mut self, slot: usize) {
        let Some(path) = self.paths.get(slot).and_then(Option::as_ref) else {
            return;
        };
        let rate = self.nam_rate();
        let run = self.engine.run_id();
        let generation = self.engine.path_generation(slot);
        let frames = self.nam_frames();
        // An empty effect chain joins the Pre and Post groups into one
        // series; a plugin between them splits it.
        let effects_empty = self.engine.plugin_views(slot).is_empty();
        let factors = path.nam_factors(effects_empty);
        // What to ask for, computed before anything changes.
        let mut wanted = Vec::new();
        for (block, factor) in path.nam.iter().zip(factors) {
            let stamp = NamInstall {
                path: block.model.clone(),
                rate,
                run,
                generation,
                submodel: slim_submodel(&block.slim_steps, block.controls.slim),
                factor,
            };
            if stamp != block.install {
                wanted.push((block.controls.id, block.controls.slim, stamp));
            }
        }
        if wanted.is_empty() {
            return;
        }
        if self.nam_loader.is_none() {
            self.nam_loader = NamLoader::spawn();
            if self.nam_loader.is_none() {
                self.push_log("Could not start the capture loader.", LogTone::Amber);
                return;
            }
        }
        for (id, slim, stamp) in wanted {
            if stamp.path.trim().is_empty() {
                self.engine.clear_nam(slot, id);
                let mut changed = false;
                if let Some(path) = self.paths.get_mut(slot).and_then(Option::as_mut) {
                    if let Some(block) = path.nam.iter_mut().find(|block| block.controls.id == id) {
                        let controls = block.controls;
                        block.cleared();
                        block.install = stamp;
                        changed = block.controls != controls;
                    }
                }
                // The cleared levels are part of the published stack.
                if changed {
                    self.push_paths_to_engine();
                }
                continue;
            }
            let job = NamJob {
                slot,
                id,
                stamp: stamp.clone(),
                path: stamp.path.clone(),
                rate,
                slim,
                factor: stamp.factor,
                frames,
            };
            let asked = self
                .nam_loader
                .as_ref()
                .is_some_and(|loader| loader.request(job));
            // The stamp is written even when the request failed: the block
            // keeps what it shows, and a new one is only asked for when the
            // capture, the rate or the run changes.
            if let Some(path) = self.paths.get_mut(slot).and_then(Option::as_mut) {
                if let Some(block) = path.nam.iter_mut().find(|block| block.controls.id == id) {
                    block.install = stamp;
                }
            }
            if !asked {
                self.nam_loader = None;
            }
        }
    }

    /// Takes the captures the loader finished.  A stale answer (its block
    /// changed or left) is dropped here, on the UI thread.
    pub(crate) fn poll_nam_loader(&mut self) {
        let mut answers = Vec::new();
        if let Some(loader) = &self.nam_loader {
            while let Ok(done) = loader.done.try_recv() {
                answers.push(done);
            }
        }
        for done in answers {
            self.apply_nam_result(done);
        }
    }

    fn apply_nam_result(&mut self, done: NamDone) {
        let NamDone {
            slot,
            id,
            stamp,
            answer,
        } = done;
        let mut queue = None;
        let Some(path) = self.paths.get_mut(slot).and_then(Option::as_mut) else {
            // The path left: dropping the model here is what this thread is
            // for.
            return;
        };
        let published = path.nam_stack();
        let Some(block) = path.nam.iter_mut().find(|block| block.controls.id == id) else {
            return;
        };
        if block.install != stamp {
            return;
        }
        let file = match answer {
            NamAnswer::Built(file) => *file,
            NamAnswer::Remeasured(load) => {
                block.load = Some(load);
                if self.nam_warning.is_some() {
                    self.check_nam_load();
                }
                return;
            }
        };
        let clear = match file {
            NamFile::Ready(loaded) => {
                block.name = loaded.name;
                block.error = None;
                block.gear = loaded.gear_type;
                block.slim_steps = loaded.slim_steps;
                block.mismatch = false;
                block.file_rate = loaded.file_rate;
                block.arch = loaded.arch;
                block.oversample = loaded.oversample;
                block.load = loaded.load;
                block.controls.level_in_dbu = loaded.level_in_dbu;
                block.controls.level_out_dbu = loaded.level_out_dbu;
                block.controls.loudness_db = loaded.loudness_db;
                // The stamp names the submodel this build really is.
                block.install.submodel = slim_submodel(&block.slim_steps, block.controls.slim);
                queue = Some(loaded.prepared);
                false
            }
            NamFile::RateMismatch { file_rate, name } => {
                block.name = name;
                block.error = None;
                block.gear = None;
                block.slim_steps.clear();
                block.mismatch = true;
                block.oversample = false;
                block.file_rate = file_rate;
                block.arch.clear();
                block.load = None;
                true
            }
            NamFile::Failed(error) => {
                block.name = stem_or_dash(&stamp.path);
                block.error = Some(error);
                block.gear = None;
                block.slim_steps.clear();
                block.mismatch = false;
                block.oversample = false;
                block.file_rate = 0;
                block.arch.clear();
                block.load = None;
                true
            }
        };
        // The file's calibration levels travel in the stack, so a capture
        // whose levels differ from the last one is published before its
        // model is queued: it fades in level-matched.
        let changed = path.nam_stack() != published;
        if changed {
            self.push_paths_to_engine();
        }
        if clear {
            self.engine.clear_nam(slot, id);
        }
        if let Some(prepared) = queue {
            self.engine.queue_nam(slot, id, prepared);
        }
        // A heavy preset's banner refreshes as its captures come in: the
        // estimates it opened with are replaced by what each one measured.
        if self.nam_warning.is_some() {
            self.check_nam_load();
        }
    }

    /// The share of one block every live path's NAM blocks take together.
    /// The DSP budget is one budget, so this is what a preset is checked
    /// against.
    pub(crate) fn nam_total_load(&self) -> f32 {
        self.path_slots()
            .into_iter()
            .map(|slot| self.nam_load(slot))
            .sum()
    }

    /// Every live path's running blocks, heaviest first.
    pub(crate) fn nam_heaviest_all(&self) -> Vec<NamHeavy> {
        let rate = self.nam_rate();
        let mut blocks: Vec<NamHeavy> = self
            .path_slots()
            .into_iter()
            .filter_map(|slot| Some((slot, self.paths.get(slot)?.as_ref()?)))
            .flat_map(|(slot, path)| {
                path.nam
                    .iter()
                    .filter(|block| block.active())
                    .map(move |block| NamHeavy {
                        slot,
                        id: block.controls.id,
                        path: path.name.clone(),
                        name: if block.name.is_empty() {
                            "NAM block".to_owned()
                        } else {
                            block.name.clone()
                        },
                        share: block.load_share(rate),
                    })
            })
            .collect();
        blocks.sort_by(|a, b| b.share.total_cmp(&a.share));
        blocks
    }

    /// The share of one block path `slot`'s NAM blocks take together: the
    /// measured cost where a capture has one, the estimate otherwise.
    pub(crate) fn nam_load(&self, slot: usize) -> f32 {
        let rate = self.nam_rate();
        self.paths
            .get(slot)
            .and_then(Option::as_ref)
            .map_or(0.0, |path| {
                path.nam
                    .iter()
                    .filter(|block| block.active())
                    .map(|block| block.load_share(rate))
                    .sum()
            })
    }

    /// The blocks worth bypassing when the DSP does not fit, heaviest first.
    /// A capture that has not been measured yet counts with its estimate.
    pub(crate) fn nam_heaviest(&self, slot: usize) -> Vec<(usize, String, f32)> {
        let Some(path) = self.paths.get(slot).and_then(Option::as_ref) else {
            return Vec::new();
        };
        let rate = self.nam_rate();
        let mut blocks: Vec<(usize, String, f32)> = path
            .nam
            .iter()
            .enumerate()
            .filter(|(_, block)| block.active())
            .map(|(index, block)| {
                let name = if block.name.is_empty() {
                    "NAM block".to_owned()
                } else {
                    block.name.clone()
                };
                (index, name, block.load_share(rate))
            })
            .collect();
        blocks.sort_by(|a, b| b.2.total_cmp(&a.2));
        blocks
    }

    /// Bypasses the `count` heaviest NAM blocks of path `slot`.  A normal
    /// edit: it is saved only if the person saves.
    pub(crate) fn bypass_heaviest_nam(&mut self, slot: usize, count: usize) {
        let heaviest = self.nam_heaviest(slot);
        let Some(path) = self.paths.get_mut(slot).and_then(Option::as_mut) else {
            return;
        };
        let mut bypassed = 0;
        for (index, _, _) in heaviest.into_iter().take(count) {
            if let Some(block) = path.nam.get_mut(index) {
                if block.controls.enabled {
                    block.controls.enabled = false;
                    bypassed += 1;
                }
            }
        }
        if bypassed > 0 {
            self.push_paths_to_engine();
            self.push_log(format!("Bypassed {bypassed} NAM block(s)."), LogTone::Amber);
        }
    }

    /// Sets the warning a freshly loaded preset gets when the NAM blocks of
    /// all live paths together are projected past the DSP warning share.
    /// Never refuses the load and never touches a block on its own: the
    /// banner offers, the person decides.
    pub(crate) fn check_nam_load(&mut self) {
        let load = self.nam_total_load();
        let blocks = self.nam_heaviest_all();
        if load < crate::app::issues::DSP_WARN_SHARE || blocks.is_empty() {
            self.nam_warning = None;
            return;
        }
        let suggested = suggested_bypass(blocks.iter().map(|block| block.share), load);
        self.nam_warning = Some(NamLoadWarning {
            blocks,
            load,
            suggested,
        });
    }

    /// Bypasses the blocks the warning suggests, then checks again: the
    /// banner stays only while the paths are still over the share.  A
    /// normal edit, saved only if the person saves.
    pub(crate) fn bypass_suggested_nam(&mut self) {
        let Some(warning) = self.nam_warning.take() else {
            return;
        };
        let mut bypassed = 0;
        for heavy in warning.blocks.iter().take(warning.suggested) {
            let Some(path) = self.paths.get_mut(heavy.slot).and_then(Option::as_mut) else {
                continue;
            };
            if let Some(block) = path
                .nam
                .iter_mut()
                .find(|block| block.controls.id == heavy.id)
            {
                if block.controls.enabled {
                    block.controls.enabled = false;
                    bypassed += 1;
                }
            }
        }
        if bypassed > 0 {
            self.push_paths_to_engine();
            self.push_log(format!("Bypassed {bypassed} NAM block(s)."), LogTone::Amber);
        }
        self.check_nam_load();
    }
}

/// How many of the heaviest blocks (`shares`, heaviest first) have to be
/// bypassed, in order, for `load` to drop under the DSP warning share.
/// Every block when even that is not enough.
pub(crate) fn suggested_bypass(shares: impl IntoIterator<Item = f32>, load: f32) -> usize {
    let mut left = load;
    let mut count = 0;
    for share in shares {
        if left < crate::app::issues::DSP_WARN_SHARE {
            break;
        }
        left -= share;
        count += 1;
    }
    count
}

/// One running block, as the heavy-preset warning lists it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct NamHeavy {
    pub slot: usize,
    pub id: u32,
    /// The path's name and the capture's.
    pub path: String,
    pub name: String,
    pub share: f32,
}

/// A preset that loaded heavier than the DSP budget: the blocks of every
/// path, heaviest first, what they come to together, and how many of them
/// the banner suggests bypassing.
pub(crate) struct NamLoadWarning {
    pub blocks: Vec<NamHeavy>,
    /// The NAM load of all live paths, as a share of the block.
    pub load: f32,
    /// The first this many of `blocks` bring `load` under the warning share.
    pub suggested: usize,
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
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            GetAsyncKeyState, VK_CONTROL, VK_MENU, VK_SHIFT, VK_V,
        };
        // The high bit is set while the key is down.  Shift and Alt stay out
        // so this is Ctrl+V only.
        unsafe {
            GetAsyncKeyState(VK_CONTROL.0 as i32) < 0
                && GetAsyncKeyState(VK_V.0 as i32) < 0
                && GetAsyncKeyState(VK_SHIFT.0 as i32) >= 0
                && GetAsyncKeyState(VK_MENU.0 as i32) >= 0
        }
    }
    #[cfg(not(windows))]
    {
        false
    }
}

pub(crate) fn looks_pasted(previous: &str, edited: &str) -> bool {
    edited.chars().count() > previous.chars().count().saturating_add(1)
}

/// The edited text is a directory, or it matches the clipboard and the
/// clipboard also holds a copied folder.
pub(crate) fn pasted_folder(edited: &str) -> Option<String> {
    let normalized = normalize_models_folder(edited);
    if Path::new(&normalized).is_dir() {
        return Some(normalized);
    }
    let mut board = arboard::Clipboard::new().ok()?;
    let text = board.get_text().ok()?;
    let same = normalize_models_folder(&text) == normalized || text.trim() == edited.trim();
    if !same {
        return None;
    }
    file_list_folder(&mut board)
}

pub(crate) fn clipboard_models_folder() -> Option<String> {
    let mut board = arboard::Clipboard::new().ok()?;
    let text = board.get_text().ok();
    if let Some(folder) = text.as_deref().map(normalize_models_folder) {
        if Path::new(&folder).is_dir() {
            return Some(folder);
        }
    }
    if let Some(folder) = file_list_folder(&mut board) {
        return Some(folder);
    }
    let folder = normalize_models_folder(text.as_deref()?);
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
    let folder = normalize_models_folder(&folder.to_string_lossy());
    if folder.is_empty() {
        None
    } else {
        Some(folder)
    }
}

pub(crate) fn browse_start(folder: &str) -> Option<PathBuf> {
    let path = PathBuf::from(folder);
    if path.is_dir() {
        Some(path)
    } else {
        path.parent()
            .filter(|parent| parent.is_dir())
            .map(Path::to_path_buf)
    }
}

fn stem_or_dash(path: &str) -> String {
    std::path::Path::new(path)
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| "NAM".to_owned())
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
