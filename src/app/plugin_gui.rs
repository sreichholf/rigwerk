//! The plugin's own graphical editor, in a window of the plugin's making.
//!
//! A VST3 editor embeds in a native window created here: Win32, or X11 on
//! Linux, which needs an X server (`DISPLAY`, XWayland included).  The host
//! does not implement `IWaylandHost` / `IWaylandFrame`, so a session with
//! no X server cannot open it.  Its timers and fds run from the VST3 run
//! loop, which the engine services every frame.
//!
//! A CLAP editor embeds in a native window created here (Win32, or X11 when
//! an X server is reachable), or floats in a toplevel of its own (pure
//! Wayland, or a plugin that only floats).  The plugin's GUI callbacks only
//! set flags on its host; [`ClapGuiWindow::service`] follows them once per
//! frame.
//!
//! Both reach the plugin through its [`PluginHandle`] (the UI-only lock the
//! audio thread never takes), so an editor at work never makes the audio
//! callback skip the insert.

use crate::app::LogTone;
use std::sync::Arc;
use std::time::Instant;

use crate::app::paths::InsertKey;
use crate::app::RigWerkApp;
use crate::audio::clap::{
    ClapInstance, GuiVisibility, HostState, ParentWindow, ResizeHints, WindowApi,
};
use crate::audio::vst3::Vst3Editor;
use crate::audio::{PluginHandle, PluginView};

pub(crate) struct PluginGuiWindow {
    /// The insert this editor belongs to: its path and its position there.
    pub(crate) key: InsertKey,
    /// The plugin's path and class, so a rebuilt instance of the same
    /// plugin at this key (a layout reload) gets its editor back.
    plugin: (String, Option<String>),
    kind: GuiKind,
    /// The plugin this editor shows.  Declared after `kind`, so the editor
    /// is detached before this reference goes.
    handle: PluginHandle,
}

enum GuiKind {
    Vst3 { window: Vst3GuiWindow },
    Clap { window: ClapGuiWindow },
}

impl PluginGuiWindow {
    /// The identity of the instance this editor belongs to, as the published
    /// insert views report it.
    fn instance_id(&self) -> usize {
        self.handle.instance_id()
    }

    /// Whether the insert at this editor's key still holds its instance.
    /// Read from the published views, so polling an open editor every frame
    /// never takes the chain lock the audio callback needs.
    fn matches(&self, view: Option<&PluginView>) -> bool {
        view.and_then(|view| view.instance) == Some(self.instance_id())
    }

    /// Whether `view` is a new instance of this editor's plugin, with an
    /// editor of its own.
    fn rebuilt(&self, view: Option<&PluginView>) -> bool {
        view.is_some_and(|view| {
            view.instance.is_some()
                && view.has_editor
                && (view.path.as_str(), view.class_id.as_deref())
                    == (self.plugin.0.as_str(), self.plugin.1.as_deref())
        })
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
        let views = self.engine.plugin_views(path);
        let view = views.get(index);
        let name = view
            .map(|view| view.name.clone())
            .unwrap_or_else(|| "Plugin".to_owned());
        let plugin = view
            .map(|view| (view.path.clone(), view.class_id.clone()))
            .unwrap_or_default();
        // With several paths the window title says whose insert it is.
        let name = match self.paths.get(path).and_then(Option::as_ref) {
            Some(owner) if self.path_count() > 1 => format!("{name} · {}", owner.name),
            _ => name,
        };
        let opened = if handle.is_vst3() {
            match Vst3GuiWindow::open(&handle, &name, f64::from(self.ui_scale)) {
                Ok(window) => {
                    self.plugin_gui = Some(PluginGuiWindow {
                        key,
                        plugin,
                        kind: GuiKind::Vst3 { window },
                        handle,
                    });
                    true
                }
                Err(error) => {
                    self.gui_error(error);
                    false
                }
            }
        } else if handle.is_clap() {
            match ClapGuiWindow::open(handle.clone(), &name, f64::from(self.ui_scale)) {
                Ok(window) => {
                    self.plugin_gui = Some(PluginGuiWindow {
                        key,
                        plugin,
                        kind: GuiKind::Clap { window },
                        handle,
                    });
                    true
                }
                Err(error) => {
                    self.gui_error(error);
                    false
                }
            }
        } else {
            false
        };
        if opened {
            self.push_log("Opened the plugin's own interface.", LogTone::Green);
        }
        opened
    }

    fn gui_error(&mut self, error: String) {
        let message = format!("This effect has no interface it can open here: {error}");
        self.push_log(message.clone(), LogTone::Red);
        self.plugin_browser.notice = Some((message, Instant::now()));
    }

    /// Closes the plugin's window, if one is open.  Dropping it detaches the
    /// editor from its native window first, so nothing outlives the window.
    pub(crate) fn close_plugin_gui(&mut self) {
        self.finish_plugin_gui();
    }

    /// Drops the editor after a last full sync, so whatever it changed
    /// without reporting it reaches the saved insert and its state blob.
    /// The service frames already follow its edits and reported values.
    pub(crate) fn finish_plugin_gui(&mut self) {
        if let Some(key) = self.plugin_gui.as_ref().map(|gui| gui.key) {
            self.engine.finish_plugin_editor(key.path, key.insert);
        }
        self.plugin_gui = None;
    }

    /// Copies the open editor's current controls into the engine with a full
    /// read, without closing the editor.  Used before a save and before a
    /// bridge starts: the save or the new session is built from the
    /// insert's saved values and state blob, not from the open editor.  The
    /// full sync marks the insert's saved state stale, so a later save
    /// captures it again.
    pub(crate) fn sync_plugin_editor(&self) {
        if let Some(key) = self.plugin_gui.as_ref().map(|gui| gui.key) {
            self.engine.sync_plugin_parameters(key.path, key.insert);
        }
    }

    /// Follows the open editor's window once per UI frame: a close, a
    /// resize, a scale change, and the editor's own size requests.  The
    /// plugin's main-thread work (edits, restarts, a VST3 editor's run
    /// loop, CLAP callbacks) is the engine's `service_plugins`, which the
    /// frame runs too.  An editor whose insert went away is dropped; one
    /// whose plugin was rebuilt in place (a layout reload) opens again on
    /// the new instance.
    pub(crate) fn poll_plugin_gui(&mut self) {
        let Some(key) = self.plugin_gui.as_ref().map(|gui| gui.key) else {
            return;
        };
        let views = self.engine.plugin_views(key.path);
        let app_scale = f64::from(self.ui_scale);
        let (dismissed, same_plugin, rebuilt) = {
            let Some(gui) = self.plugin_gui.as_mut() else {
                return;
            };
            let dismissed = match &mut gui.kind {
                GuiKind::Vst3 { window } => window.service(app_scale),
                GuiKind::Clap { window } => window.service(app_scale),
            };
            let view = views.get(key.insert);
            (dismissed, gui.matches(view), gui.rebuilt(view))
        };
        if dismissed && same_plugin {
            self.finish_plugin_gui();
            return;
        }
        if !same_plugin {
            // A rebuilt plugin's editor opens before the old one closes.
            // VSTGUI on X11 keeps one X connection per module and closes it
            // with its last editor, without finishing the cairo device it
            // drew with; cairo keys that device by the connection's address,
            // so a new connection allocated at the same address would draw
            // through the stale device and abort.  Overlapping the two keeps
            // the connection open.  The old view is still detached before
            // the old instance deactivates (`kind` drops before `handle`).
            let old = self.plugin_gui.take();
            if rebuilt && !dismissed {
                self.open_plugin_gui(key);
            }
            drop(old);
        }
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

/// The native window an embedded editor lives in.  Sizes are physical
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

/// A VST3 editor view and the window it is attached to.
struct Vst3GuiWindow {
    /// Declared first: the view is detached before its window goes.
    editor: Vst3Editor,
    frame: Box<dyn EditorFrame>,
    hints: ResizeHints,
    /// The client size the view was last given or asked for, so the
    /// window's echo of a resize is not passed back to the plugin.
    last_client: Option<(u32, u32)>,
    scale: f64,
}

impl Vst3GuiWindow {
    /// The window API VST3 views attach to here.
    #[cfg(target_os = "windows")]
    const API: WindowApi = WindowApi::Win32;
    #[cfg(not(target_os = "windows"))]
    const API: WindowApi = WindowApi::X11;

    fn open(handle: &PluginHandle, title: &str, app_scale: f64) -> Result<Self, String> {
        let mut editor = handle.open_vst3_editor()?;
        let size = editor.size().unwrap_or((640, 480));
        let frame = create_frame(Self::API, title, size)?;
        let scale = frame.own_scale().unwrap_or(app_scale);
        editor.set_scale(scale);
        editor.attach(frame.parent())?;
        let hints = if editor.can_resize() {
            ResizeHints {
                horizontal: true,
                vertical: true,
                aspect: None,
            }
        } else {
            ResizeHints::FIXED
        };
        let mut window = Self {
            editor,
            frame,
            hints,
            last_client: Some(size),
            scale,
        };
        // Attaching may have settled the view on another size.
        let size = window.editor.size().unwrap_or(size);
        window.resize_frame(size);
        window.frame.show();
        Ok(window)
    }

    /// Follows the window and the view's size requests.  Returns true once
    /// the window should be closed.
    fn service(&mut self, app_scale: f64) -> bool {
        let events = self.frame.poll();
        if events.closed {
            return true;
        }
        let scale = match self.frame.own_scale() {
            Some(_) => events.scale,
            None => ((app_scale - self.scale).abs() > 1e-3).then_some(app_scale),
        };
        if let Some(scale) = scale {
            self.scale = scale;
            self.editor.set_scale(scale);
        }
        if let Some(size) = self.editor.take_resize_request() {
            // The view already has this size (`resizeView` calls `onSize`).
            self.resize_frame(size);
            return false;
        }
        if let Some(size) = events.size {
            if self.last_client != Some(size) {
                // The user resized the window: the view rounds the size and
                // the window follows whatever it settled on.
                let fitted = self.editor.fit(size.0, size.1);
                self.editor.resize(fitted.0, fitted.1);
                self.last_client = Some(fitted);
                if fitted != size {
                    self.resize_frame(fitted);
                }
            }
        }
        false
    }

    fn resize_frame(&mut self, size: (u32, u32)) {
        self.frame.resize(size.0, size.1);
        self.frame.set_resize_hints(self.hints, size);
        self.last_client = Some(size);
    }
}

/// A CLAP editor and the window it is in.  `frame` is `None` for a floating
/// editor, whose toplevel the plugin owns.
struct ClapGuiWindow {
    host: Arc<HostState>,
    frame: Option<Box<dyn EditorFrame>>,
    hints: ResizeHints,
    /// The client size the editor was last given or asked for, so the
    /// window's echo of a resize is not passed back to the plugin.
    last_client: Option<(u32, u32)>,
    scale: f64,
    /// Last: the editor is closed (in `drop`) before this reference goes.
    handle: PluginHandle,
}

impl ClapGuiWindow {
    /// Runs `body` on the plugin's main-thread instance.
    fn plugin<R>(&self, body: impl FnOnce(&mut ClapInstance) -> R) -> Result<R, String> {
        self.handle
            .with_clap(body)
            .ok_or_else(|| "This is not a CLAP plugin.".to_owned())
    }

    fn open(handle: PluginHandle, title: &str, app_scale: f64) -> Result<Self, String> {
        let (api, size, host) = handle
            .with_clap(|plugin| {
                let api = plugin.editor_api().ok_or_else(|| {
                    "This plugin has no interface RigWerk can open here.".to_owned()
                })?;
                let size = plugin.prepare_editor()?;
                Ok::<_, String>((api, size, plugin.host()))
            })
            .ok_or_else(|| "This is not a CLAP plugin.".to_owned())??;
        let mut window = Self {
            host,
            frame: None,
            hints: ResizeHints::FIXED,
            last_client: Some(size),
            scale: app_scale,
            handle,
        };
        if api.floating {
            // Dropping `window` on failure destroys the created editor.
            let title = format!("{title} - CLAP");
            window.plugin(|plugin| plugin.show_floating_editor(&title, app_scale))??;
            return Ok(window);
        }
        let frame = create_frame(api.api, title, size)?;
        window.scale = frame.own_scale().unwrap_or(app_scale);
        let parent = frame.parent();
        // Owned by `window` from here, so a failed attach still detaches the
        // editor before the native window is destroyed.
        window.frame = Some(frame);
        let scale = window.scale;
        let hints = window.plugin(|plugin| {
            plugin.attach_editor(parent, scale)?;
            Ok::<_, String>(plugin.editor_resize_hints())
        })??;
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
        // Timers, fds and `on_main_thread` (where most editors repaint).  The
        // audio callback never waits for them.  Restarts and latency are
        // left to the path's `service_plugins`, which republishes the views.
        let _ = self.plugin(ClapInstance::service_callbacks);
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
                let _ = self.plugin(|plugin| plugin.set_editor_scale(scale));
            }
            return false;
        };
        match self.host.take_gui_visibility() {
            Some(GuiVisibility::Show) => {
                frame.show();
                let _ = self.handle.with_clap(ClapInstance::show_editor);
            }
            Some(GuiVisibility::Hide) => {
                let _ = self.handle.with_clap(ClapInstance::hide_editor);
                frame.hide();
            }
            None => {}
        }
        if let Some(scale) = scale {
            self.scale = scale;
            let size = self
                .plugin(|plugin| {
                    plugin.set_editor_scale(scale);
                    plugin.editor_size()
                })
                .ok()
                .flatten();
            if let Some(size) = size {
                self.resize_frame(size);
            }
        }
        if self.host.take_gui_hints_changed() {
            let hints = self
                .plugin(|plugin| plugin.editor_resize_hints())
                .unwrap_or(ResizeHints::FIXED);
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
                let fitted = self
                    .plugin(|plugin| plugin.resize_editor(size.0, size.1))
                    .unwrap_or(size);
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
        let _ = self.handle.with_clap(ClapInstance::close_editor);
        drop(self.frame.take());
    }
}

#[cfg_attr(
    not(any(target_os = "windows", target_os = "linux")),
    allow(unused_variables)
)]
fn create_frame(
    api: WindowApi,
    title: &str,
    size: (u32, u32),
) -> Result<Box<dyn EditorFrame>, String> {
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
