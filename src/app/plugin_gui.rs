//! The plugin's own graphical editor, in a window of the plugin's making.
//!
//! VST3 editors are opened through `vst3-host`'s [`PluginWindow`].  On Linux
//! that window is an X11 client, so it needs a display (`DISPLAY`, including
//! XWayland).  The crate does not implement `IWaylandHost` / `IWaylandFrame`,
//! and a session with no X server cannot open the editor.
//!
//! A CLAP editor embeds in a native window created here (Win32, or X11 when
//! an X server is reachable), or floats in a toplevel of its own (pure
//! Wayland, or a plugin that only floats).  The plugin's GUI callbacks only
//! set flags on its host; [`ClapGuiWindow::service`] follows them once per
//! frame.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use vst3_host::PluginWindow;

use crate::app::paths::InsertKey;
use crate::app::RigWerkApp;
use crate::audio::clap::{ClapPlugin, GuiVisibility, HostState, ParentWindow, ResizeHints};
use crate::audio::{plugin_instance_id, EngineState, PluginHandle, PluginView};
use crate::ui::theme::{GREEN, RED};

/// How often a Linux VST3 editor's run loop is serviced while a bridge runs.
/// Every frame would hold the instance lock often enough for the audio
/// callback to miss many blocks; at about 30 Hz the editor still paints.
const VST3_RUN_LOOP_INTERVAL: Duration = Duration::from_millis(30);

/// Whether the editor's run loop is due.  With no audio running there is
/// nothing to starve, so every frame services it; while a bridge runs it is
/// limited to [`VST3_RUN_LOOP_INTERVAL`].
fn run_loop_due(active: bool, last: Instant, now: Instant) -> bool {
    !active || now.saturating_duration_since(last) >= VST3_RUN_LOOP_INTERVAL
}

/// The `last_run_loop` an editor starts with: one interval in the past, so
/// the first frame services the loop and paints, or `now` on a clock that
/// cannot go back that far.  Shared with the test, which pins the property.
fn run_loop_start(now: Instant) -> Instant {
    now.checked_sub(VST3_RUN_LOOP_INTERVAL).unwrap_or(now)
}

/// Whether the engine is running a callback that can process a plugin block.
/// Preview does not: it starts no callback, so a VST3 insert there never
/// sees a processed block and its reported changes stay queued.
fn audio_active(state: EngineState) -> bool {
    matches!(
        state,
        EngineState::Starting | EngineState::Running | EngineState::Stopping
    )
}

pub(crate) struct PluginGuiWindow {
    /// The insert this editor belongs to: its path and its position there.
    pub(crate) key: InsertKey,
    kind: GuiKind,
    last_sync: Instant,
}

enum GuiKind {
    Vst3 {
        plugin: Arc<Mutex<vst3_host::Plugin>>,
        window: PluginWindow,
        /// When the run loop was last serviced, for
        /// [`VST3_RUN_LOOP_INTERVAL`].  Kept here rather than on the shared
        /// window so the VST3 arm can update it under its own borrow.
        last_run_loop: Instant,
    },
    Clap {
        plugin: Arc<Mutex<ClapPlugin>>,
        window: ClapGuiWindow,
    },
}

impl PluginGuiWindow {
    /// The identity of the instance this editor belongs to, as the published
    /// insert views report it.
    fn instance_id(&self) -> usize {
        match &self.kind {
            GuiKind::Vst3 { plugin, .. } => plugin_instance_id(plugin),
            GuiKind::Clap { plugin, .. } => plugin_instance_id(plugin),
        }
    }

    /// Whether the insert at this editor's key still holds its instance.
    /// Read from the published views, so polling an open editor every frame
    /// never takes the chain lock the audio callback needs.
    fn matches(&self, view: Option<&PluginView>) -> bool {
        view.and_then(|view| view.instance) == Some(self.instance_id())
    }
}

impl RigWerkApp {
    /// Opens the plugin's own interface for one insert, replacing any editor
    /// that is already open.  Returns whether a window could be opened.
    pub(crate) fn open_plugin_gui(&mut self, key: InsertKey) -> bool {
        let InsertKey {
            path,
            insert: index,
        } = key;
        self.engine.ensure_plugin_loaded(path, index);
        // Dropping the old window detaches its editor before the new one opens.
        self.finish_plugin_gui();
        let Some(handle) = self.engine.plugin_handle(path, index) else {
            self.plugin_browser.notice = Some((
                "The effect is not loaded, so its interface cannot open.".to_owned(),
                Instant::now(),
            ));
            return false;
        };
        let name = self
            .engine
            .plugin_views(path)
            .get(index)
            .map(|view| view.name.clone())
            .unwrap_or_else(|| "Plugin".to_owned());
        // With several paths the window title says whose insert it is.
        let name = match self.paths.get(path).and_then(Option::as_ref) {
            Some(owner) if self.path_count() > 1 => format!("{name} · {}", owner.name),
            _ => name,
        };
        let opened = match &handle {
            PluginHandle::Vst3(plugin) => {
                let mut window = PluginWindow::new(plugin.clone());
                match window.open() {
                    Ok(()) => {
                        self.plugin_gui = Some(PluginGuiWindow {
                            key,
                            kind: GuiKind::Vst3 {
                                plugin: plugin.clone(),
                                window,
                                // Due at once, so opening the editor paints
                                // on the first frame rather than after one
                                // interval of a blank window.
                                last_run_loop: run_loop_start(Instant::now()),
                            },
                            last_sync: Instant::now(),
                        });
                        true
                    }
                    Err(error) => {
                        self.gui_error(error.to_string());
                        false
                    }
                }
            }
            PluginHandle::Clap(plugin) => {
                match ClapGuiWindow::open(plugin.clone(), &name, f64::from(self.ui_scale)) {
                    Ok(window) => {
                        self.plugin_gui = Some(PluginGuiWindow {
                            key,
                            kind: GuiKind::Clap {
                                plugin: plugin.clone(),
                                window,
                            },
                            last_sync: Instant::now(),
                        });
                        true
                    }
                    Err(error) => {
                        self.gui_error(error);
                        false
                    }
                }
            }
        };
        if opened {
            self.push_log("Opened the plugin's own interface.", GREEN);
        }
        opened
    }

    fn gui_error(&mut self, error: String) {
        let message = format!("This effect has no interface it can open here: {error}");
        self.push_log(message.clone(), RED);
        self.plugin_browser.notice = Some((message, Instant::now()));
    }

    /// Closes the plugin's window, if one is open.  Dropping it detaches the
    /// editor from its native window first, so nothing outlives the window.
    pub(crate) fn close_plugin_gui(&mut self) {
        self.finish_plugin_gui();
    }

    /// Drops the editor after a last full sync, so whatever it changed since
    /// the previous poll reaches the saved insert and its state blob.
    ///
    /// The poll itself only asks a VST3 insert that processed a block for the
    /// changes it reported; a CLAP editor and an insert that has not
    /// processed a block are read in full instead (see
    /// [`Self::poll_plugin_gui`]).  So this close-time read is not the only
    /// full one, but it is the last chance to see changes a plugin made
    /// without reporting them.
    pub(crate) fn finish_plugin_gui(&mut self) {
        if let Some(key) = self.plugin_gui.as_ref().map(|gui| gui.key) {
            self.engine.finish_plugin_editor(key.path, key.insert);
        }
        self.plugin_gui = None;
    }

    /// Copies the open editor's current controls into the engine with a full
    /// read, without closing the editor.  Used before a save and before a
    /// bridge starts: an edit may be younger than the last 250 ms poll, and
    /// the save or the new session is built from the insert's saved values
    /// and state blob, not from the open editor.  The full sync marks the
    /// insert's saved state stale, so a later save captures it again.
    pub(crate) fn sync_plugin_editor(&self) {
        if let Some(key) = self.plugin_gui.as_ref().map(|gui| gui.key) {
            let _ = self
                .engine
                .sync_plugin_parameters(key.path, key.insert, true);
        }
    }

    /// Servicing and keeping the open editor in step, once per UI frame.
    ///
    /// On Linux a VST3 editor's `IRunLoop` carries its input, timers and
    /// repaints.  This drives that loop while the window is open, about 30 Hz
    /// while a bridge runs and at the full frame rate when it is stopped:
    /// each service try-locks the instance, and at 60 Hz the lock would be
    /// busy often enough for the audio callback to miss many chunks.  The UI
    /// never waits on the callback.  While the UI holds the lock to service
    /// the loop, the callback skips the insert for that chunk, declicked and
    /// with its latency kept.  So the 30 Hz gate delays the editor's input
    /// and timer dispatch as well as its paint.  The cheap platform-event
    /// service (window close, resize and DPI changes) still runs every frame
    /// and try-locks once for them, so the interval limits only the run loop
    /// itself.
    pub(crate) fn poll_plugin_gui(&mut self) {
        let Some(key) = self.plugin_gui.as_ref().map(|gui| gui.key) else {
            return;
        };
        let views = self.engine.plugin_views(key.path);
        let audio_active = audio_active(self.engine.state);
        let now = Instant::now();
        let (dismissed, same_plugin, due) = {
            let Some(gui) = self.plugin_gui.as_mut() else {
                return;
            };
            let dismissed = match &mut gui.kind {
                GuiKind::Vst3 {
                    plugin,
                    window,
                    last_run_loop,
                } => {
                    let _ = window.service_platform_events();
                    let dismissed = window.closed_by_user() || !window.is_open();
                    // A title-bar close is already recorded.  Painting now would
                    // draw into a window the host is about to destroy.
                    if !dismissed
                        && run_loop_due(audio_active, *last_run_loop, now)
                        && service_vst3_run_loop(plugin)
                    {
                        *last_run_loop = now;
                    }
                    dismissed
                }
                GuiKind::Clap { window, .. } => window.service(f64::from(self.ui_scale)),
            };
            let same_plugin = gui.matches(views.get(key.insert));
            let due = gui.last_sync.elapsed() >= Duration::from_millis(250);
            if due {
                gui.last_sync = Instant::now();
            }
            (dismissed, same_plugin, due)
        };
        if dismissed && same_plugin {
            self.finish_plugin_gui();
            return;
        }
        if !same_plugin {
            self.plugin_gui = None;
            return;
        }
        if due {
            // A running VST3 insert reports its own changes, so the sync reads
            // only the controls it names once it has processed a block.  A
            // bridge that is not running processes none, so no VST3 edit could
            // become visible and every poll reads in full; the sync also goes
            // full for a CLAP editor and an insert that has not processed a
            // block (see `needs_full_sync`).
            let full = self.engine.state != EngineState::Running;
            let _ = self
                .engine
                .sync_plugin_parameters(key.path, key.insert, full);
        }
    }
}

/// Drives a Linux VST3 editor's `IRunLoop` without blocking the audio callback.
///
/// VSTGUI registers its X11 connection and timers there and only paints when
/// the host calls [`vst3_host::Plugin::service_run_loop`].  That needs the
/// instance lock.  The audio callback try-locks the same mutex and skips the
/// insert when it loses, so this does the same: a busy callback leaves the
/// editor for the next frame, and a poisoned lock is recovered rather than
/// silencing the editor for the rest of the session.  Other platforms have no
/// host run loop to service.  Taking the lock there would only make the
/// callback skip blocks.
///
/// Returns whether the loop ran, so a collision retries on the next frame
/// instead of waiting out [`VST3_RUN_LOOP_INTERVAL`].  Non-Linux platforms
/// have nothing to run and report `true`.
fn service_vst3_run_loop(plugin: &Mutex<vst3_host::Plugin>) -> bool {
    #[cfg(target_os = "linux")]
    {
        try_with_instance(plugin, vst3_host::Plugin::service_run_loop)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = plugin;
        true
    }
}

/// Runs `body` when the instance lock is free, including after a panic
/// poisoned it.  Returns whether `body` ran.  A lock the audio callback still
/// holds is left alone.
#[cfg(any(test, target_os = "linux"))]
fn try_with_instance<T>(instance: &Mutex<T>, body: impl FnOnce(&mut T)) -> bool {
    match instance.try_lock() {
        Ok(mut guard) => {
            body(&mut guard);
            true
        }
        Err(std::sync::TryLockError::Poisoned(poisoned)) => {
            body(&mut poisoned.into_inner());
            true
        }
        Err(std::sync::TryLockError::WouldBlock) => false,
    }
}

/// What a native editor window saw since the last poll.
#[derive(Default)]
struct FrameEvents {
    /// The user asked to close it.
    closed: bool,
    /// Its client area now has this size.
    size: Option<(u32, u32)>,
    /// It moved to a display with this scale.
    scale: Option<f64>,
}

/// The native window an embedded CLAP editor lives in.  Sizes are physical
/// pixels of the client area.  Dropping it destroys the window, so the
/// editor must be detached first.
trait EditorFrame {
    fn parent(&self) -> ParentWindow;
    /// The display scale of the window itself, for a frame that tracks it.
    /// `None` follows the app's scale instead.
    fn own_scale(&self) -> Option<f64>;
    fn poll(&mut self) -> FrameEvents;
    fn resize(&mut self, width: u32, height: u32);
    /// Lets the user resize the window only the way the editor allows.
    fn set_resize_hints(&mut self, hints: ResizeHints, size: (u32, u32));
    fn show(&mut self);
    fn hide(&mut self);
}

#[cfg(target_os = "windows")]
mod win32;
#[cfg(target_os = "linux")]
mod x11;

/// A CLAP editor and the window it is in.  `frame` is `None` for a floating
/// editor, whose toplevel the plugin owns.
struct ClapGuiWindow {
    plugin: Arc<Mutex<ClapPlugin>>,
    host: Arc<HostState>,
    frame: Option<Box<dyn EditorFrame>>,
    hints: ResizeHints,
    /// The client size the editor was last given or asked for, so the
    /// window's echo of a resize is not passed back to the plugin.
    last_client: Option<(u32, u32)>,
    scale: f64,
}

fn lock(plugin: &Mutex<ClapPlugin>) -> std::sync::MutexGuard<'_, ClapPlugin> {
    plugin.lock().unwrap_or_else(|poison| poison.into_inner())
}

impl ClapGuiWindow {
    fn open(plugin: Arc<Mutex<ClapPlugin>>, title: &str, app_scale: f64) -> Result<Self, String> {
        let (api, size, host) = {
            let mut locked = lock(&plugin);
            let api = locked
                .editor_api()
                .ok_or_else(|| "This plugin has no interface RigWerk can open here.".to_owned())?;
            let size = locked.prepare_editor()?;
            (api, size, locked.host())
        };
        let mut window = Self {
            plugin,
            host,
            frame: None,
            hints: ResizeHints::FIXED,
            last_client: Some(size),
            scale: app_scale,
        };
        if api.floating {
            // Dropping `window` on failure destroys the created editor.
            lock(&window.plugin).show_floating_editor(&format!("{title} - CLAP"), app_scale)?;
            return Ok(window);
        }
        let frame = create_frame(api.api, title, size)?;
        window.scale = frame.own_scale().unwrap_or(app_scale);
        let parent = frame.parent();
        // Owned by `window` from here, so a failed attach still detaches the
        // editor before the native window is destroyed.
        window.frame = Some(frame);
        let hints = {
            let mut locked = lock(&window.plugin);
            locked.attach_editor(parent, window.scale)?;
            locked.editor_resize_hints()
        };
        window.apply_hints(hints, size);
        if let Some(frame) = window.frame.as_mut() {
            frame.show();
        }
        Ok(window)
    }

    fn apply_hints(&mut self, hints: ResizeHints, size: (u32, u32)) {
        self.hints = hints;
        if let Some(frame) = self.frame.as_mut() {
            frame.set_resize_hints(hints, size);
        }
    }

    /// Runs the plugin's main-thread work and follows its window requests.
    /// Returns true once the window should be closed.
    fn service(&mut self, app_scale: f64) -> bool {
        let events = self
            .frame
            .as_mut()
            .map(|frame| frame.poll())
            .unwrap_or_default();
        if events.closed {
            return true;
        }
        // Timers, fds and `on_main_thread` (where most editors repaint) run
        // without the instance lock, so the audio callback keeps processing
        // this insert while the editor is busy.  A latency change it picks
        // up stays flagged on the host, so the path's `service_plugins`
        // still republishes the insert views; the result is not needed here.
        let _ = crate::audio::service_clap(&self.host, &self.plugin);
        if self.host.take_gui_closed() {
            return true;
        }
        let scale = match self.frame.as_ref().and_then(|frame| frame.own_scale()) {
            Some(_) => events.scale,
            None => ((app_scale - self.scale).abs() > 1e-3).then_some(app_scale),
        };
        let Some(frame) = self.frame.as_mut() else {
            // A floating editor sizes and shows its own toplevel.
            if let Some(scale) = scale {
                self.scale = scale;
                lock(&self.plugin).set_editor_scale(scale);
            }
            return false;
        };
        match self.host.take_gui_visibility() {
            Some(GuiVisibility::Show) => {
                frame.show();
                lock(&self.plugin).show_editor();
            }
            Some(GuiVisibility::Hide) => {
                lock(&self.plugin).hide_editor();
                frame.hide();
            }
            None => {}
        }
        if let Some(scale) = scale {
            self.scale = scale;
            let size = {
                let mut locked = lock(&self.plugin);
                locked.set_editor_scale(scale);
                locked.editor_size()
            };
            if let Some(size) = size {
                self.resize_frame(size);
            }
        }
        if self.host.take_gui_hints_changed() {
            let hints = lock(&self.plugin).editor_resize_hints();
            let size = self.last_client.unwrap_or((640, 480));
            self.apply_hints(hints, size);
        }
        if let Some(size) = self.host.take_gui_resize() {
            self.resize_frame(size);
            return false;
        }
        if let Some(size) = events.size {
            if self.last_client != Some(size) {
                // The user resized the window: the plugin rounds the size
                // and the window follows whatever it settled on.
                let fitted = lock(&self.plugin).resize_editor(size.0, size.1);
                self.last_client = Some(fitted);
                if fitted != size {
                    self.resize_frame(fitted);
                }
            }
        }
        false
    }

    /// Gives the window a new client size the editor chose, and pins the
    /// axes it cannot resize to that size.
    fn resize_frame(&mut self, size: (u32, u32)) {
        if let Some(frame) = self.frame.as_mut() {
            frame.resize(size.0, size.1);
            frame.set_resize_hints(self.hints, size);
        }
        self.last_client = Some(size);
    }
}

impl Drop for ClapGuiWindow {
    fn drop(&mut self) {
        // Detach the editor before the window it is embedded in goes.
        lock(&self.plugin).close_editor();
        drop(self.frame.take());
    }
}

#[cfg_attr(
    not(any(target_os = "windows", target_os = "linux")),
    allow(unused_variables)
)]
fn create_frame(
    api: crate::audio::clap::WindowApi,
    title: &str,
    size: (u32, u32),
) -> Result<Box<dyn EditorFrame>, String> {
    use crate::audio::clap::WindowApi;
    match api {
        #[cfg(target_os = "windows")]
        WindowApi::Win32 => Ok(Box::new(win32::Win32Frame::create(title, size)?)),
        #[cfg(target_os = "linux")]
        WindowApi::X11 => Ok(Box::new(x11::X11Frame::create(title, size)?)),
        _ => Err("This window system cannot hold the plugin's interface.".to_owned()),
    }
}

#[cfg(test)]
mod tests;
