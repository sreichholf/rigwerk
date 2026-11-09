//! The plugin side of a CLAP editor: which window API it opens with, and the
//! `clap_plugin_gui` calls that create, attach, size, scale and close it.
//!
//! Win32, X11 and Wayland all use physical pixels.  CLAP has no embedding
//! for Wayland, so there the plugin opens a floating toplevel on its own
//! connection and the host only creates, shows and destroys it.  Every call
//! here is a main-thread call, made under the instance lock.

use std::ffi::{c_char, c_void, CStr, CString};

use clap_sys::ext::gui::{
    clap_gui_resize_hints, clap_plugin_gui, clap_window, clap_window_handle,
    CLAP_WINDOW_API_WAYLAND, CLAP_WINDOW_API_WIN32, CLAP_WINDOW_API_X11,
};
use clap_sys::plugin::clap_plugin;

use super::plugin::ClapPlugin;

/// The size an editor falls back to when the plugin does not report one.
const FALLBACK_SIZE: (u32, u32) = (640, 480);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WindowApi {
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    Win32,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    X11,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Wayland,
}

impl WindowApi {
    pub(super) fn id(self) -> &'static CStr {
        match self {
            Self::Win32 => CLAP_WINDOW_API_WIN32,
            Self::X11 => CLAP_WINDOW_API_X11,
            Self::Wayland => CLAP_WINDOW_API_WAYLAND,
        }
    }
}

/// A window API and mode the plugin agreed to through `is_api_supported`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EditorApi {
    pub(crate) api: WindowApi,
    /// The plugin opens its own toplevel instead of embedding in ours.
    pub(crate) floating: bool,
}

impl EditorApi {
    const fn embedded(api: WindowApi) -> Self {
        Self {
            api,
            floating: false,
        }
    }

    const fn floating(api: WindowApi) -> Self {
        Self {
            api,
            floating: true,
        }
    }
}

/// The native window an embedded editor is attached to.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ParentWindow {
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    Win32(*mut c_void),
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    X11(u64),
}

/// How the user may resize an embedded editor, from `can_resize` and
/// `get_resize_hints`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ResizeHints {
    pub(crate) horizontal: bool,
    pub(crate) vertical: bool,
    /// Width to height, when the plugin keeps its aspect ratio.
    pub(crate) aspect: Option<(u32, u32)>,
}

impl ResizeHints {
    pub(crate) const FIXED: Self = Self {
        horizontal: false,
        vertical: false,
        aspect: None,
    };

    pub(crate) fn resizable(&self) -> bool {
        self.horizontal || self.vertical
    }
}

/// The window APIs a Linux session can serve, best first.  With an X server
/// (`DISPLAY`, XWayland included) the editor embeds in an X11 window, or
/// floats on X11.  A pure Wayland session can only float.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn linux_candidates(x11: bool, wayland: bool) -> Vec<EditorApi> {
    let mut candidates = Vec::new();
    if x11 {
        candidates.push(EditorApi::embedded(WindowApi::X11));
        candidates.push(EditorApi::floating(WindowApi::X11));
    }
    if wayland {
        candidates.push(EditorApi::floating(WindowApi::Wayland));
    }
    candidates
}

/// The window APIs this session can serve, best first.
pub(crate) fn session_candidates() -> Vec<EditorApi> {
    #[cfg(target_os = "windows")]
    {
        vec![
            EditorApi::embedded(WindowApi::Win32),
            EditorApi::floating(WindowApi::Win32),
        ]
    }
    #[cfg(target_os = "linux")]
    {
        let set = |name: &str| std::env::var_os(name).is_some_and(|value| !value.is_empty());
        linux_candidates(set("DISPLAY"), set("WAYLAND_DISPLAY"))
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        Vec::new()
    }
}

/// The first candidate the plugin supports, or `None` when it has no
/// editor this session can open.
pub(super) fn choose_editor_api(
    plugin: *const clap_plugin,
    gui: *const clap_plugin_gui,
    candidates: &[EditorApi],
) -> Option<EditorApi> {
    let gui = unsafe { gui.as_ref() }?;
    let supported = gui.is_api_supported?;
    gui.create?;
    candidates.iter().copied().find(|candidate| unsafe {
        supported(plugin, candidate.api.id().as_ptr(), candidate.floating)
    })
}

impl ClapPlugin {
    fn gui(&self) -> Result<&clap_plugin_gui, String> {
        unsafe { self.gui_ext.as_ref() }
            .ok_or_else(|| "This plugin has no GUI extension.".to_owned())
    }

    pub(crate) fn has_editor(&self) -> bool {
        self.editor_api.is_some()
    }

    /// The window API and mode the editor opens with.
    pub(crate) fn editor_api(&self) -> Option<EditorApi> {
        self.editor_api
    }

    /// Creates the plugin's GUI resources and asks how large its view is.
    pub(crate) fn prepare_editor(&mut self) -> Result<(u32, u32), String> {
        if self.editor_open {
            return Ok(self.editor_size().unwrap_or(FALLBACK_SIZE));
        }
        let api = self
            .editor_api
            .ok_or_else(|| "This plugin has no interface that RigWerk can open here.".to_owned())?;
        let create = self
            .gui()?
            .create
            .ok_or_else(|| "The plugin cannot create its interface.".to_owned())?;
        if !unsafe { create(self.plugin, api.api.id().as_ptr(), api.floating) } {
            return Err("The plugin refused to create its interface.".to_owned());
        }
        self.editor_open = true;
        Ok(self.editor_size().unwrap_or(FALLBACK_SIZE))
    }

    pub(crate) fn editor_size(&self) -> Option<(u32, u32)> {
        let get_size = self.gui().ok()?.get_size?;
        let (mut width, mut height) = FALLBACK_SIZE;
        if unsafe { get_size(self.plugin, &mut width, &mut height) } {
            Some((width.max(80), height.max(80)))
        } else {
            None
        }
    }

    /// Tells the editor the display scale (2.0 is 200 %).  False when the
    /// plugin ignored it, which CLAP allows for one that reads the OS.
    pub(crate) fn set_editor_scale(&mut self, scale: f64) -> bool {
        if !self.editor_open || !scale.is_finite() || scale <= 0.0 {
            return false;
        }
        match self.gui().ok().and_then(|gui| gui.set_scale) {
            Some(set_scale) => unsafe { set_scale(self.plugin, scale) },
            None => false,
        }
    }

    /// Attaches the prepared view to a native window and shows it.
    pub(crate) fn attach_editor(&mut self, parent: ParentWindow, scale: f64) -> Result<(), String> {
        let (api, specific) = match parent {
            ParentWindow::Win32(hwnd) => (WindowApi::Win32, clap_window_handle { win32: hwnd }),
            ParentWindow::X11(window) => (
                WindowApi::X11,
                clap_window_handle {
                    x11: window as clap_sys::ext::gui::clap_xwnd,
                },
            ),
        };
        if self.editor_api != Some(EditorApi::embedded(api)) {
            return Err("The plugin's interface was not created for this window.".to_owned());
        }
        self.set_editor_scale(scale);
        let gui = self.gui()?;
        let window = clap_window {
            api: api.id().as_ptr(),
            specific,
        };
        if let Some(set_parent) = gui.set_parent {
            if !unsafe { set_parent(self.plugin, &window) } {
                return Err("The plugin could not attach its interface.".to_owned());
            }
        }
        self.show_editor();
        Ok(())
    }

    /// Shows a floating editor the plugin owns, titled `title`.
    pub(crate) fn show_floating_editor(&mut self, title: &str, scale: f64) -> Result<(), String> {
        if !self.editor_api.is_some_and(|api| api.floating) {
            return Err("The plugin's interface was not created as its own window.".to_owned());
        }
        self.set_editor_scale(scale);
        let gui = self.gui()?;
        if let Some(suggest_title) = gui.suggest_title {
            let title = CString::new(title.replace('\0', "")).unwrap_or_default();
            unsafe { suggest_title(self.plugin, title.as_ptr() as *const c_char) };
        }
        let show = gui
            .show
            .ok_or_else(|| "The plugin cannot show its interface.".to_owned())?;
        if !unsafe { show(self.plugin) } {
            return Err("The plugin could not show its interface.".to_owned());
        }
        Ok(())
    }

    pub(crate) fn show_editor(&mut self) {
        if let Some(show) = self.gui().ok().and_then(|gui| gui.show) {
            let _ = unsafe { show(self.plugin) };
        }
    }

    pub(crate) fn hide_editor(&mut self) {
        if let Some(hide) = self.gui().ok().and_then(|gui| gui.hide) {
            let _ = unsafe { hide(self.plugin) };
        }
    }

    /// How the user may resize the embedded editor.  A plugin without
    /// `can_resize`, or one that says no, is fixed.
    pub(crate) fn editor_resize_hints(&self) -> ResizeHints {
        let Ok(gui) = self.gui() else {
            return ResizeHints::FIXED;
        };
        let resizable = gui
            .can_resize
            .is_some_and(|can_resize| unsafe { can_resize(self.plugin) });
        if !resizable {
            return ResizeHints::FIXED;
        }
        let mut hints = clap_gui_resize_hints {
            can_resize_horizontally: true,
            can_resize_vertically: true,
            preserve_aspect_ratio: false,
            aspect_ratio_width: 0,
            aspect_ratio_height: 0,
        };
        let read = gui
            .get_resize_hints
            .is_some_and(|get| unsafe { get(self.plugin, &mut hints) });
        if !read {
            return ResizeHints {
                horizontal: true,
                vertical: true,
                aspect: None,
            };
        }
        let aspect = (hints.preserve_aspect_ratio
            && hints.aspect_ratio_width > 0
            && hints.aspect_ratio_height > 0)
            .then_some((hints.aspect_ratio_width, hints.aspect_ratio_height));
        ResizeHints {
            horizontal: hints.can_resize_horizontally,
            vertical: hints.can_resize_vertically,
            aspect,
        }
    }

    /// Follows a resize of the parent window.  The plugin first rounds the
    /// size through `adjust_size`, then takes it through `set_size`.
    /// Returns the size the window must have: the adjusted size, or the
    /// editor's own size when it cannot or will not resize.
    pub(crate) fn resize_editor(&mut self, width: u32, height: u32) -> (u32, u32) {
        let current = self.editor_size().unwrap_or((width, height));
        let Ok(gui) = self.gui() else {
            return current;
        };
        if !self.editor_resize_hints().resizable() {
            return current;
        }
        let (mut adjusted_width, mut adjusted_height) = (width, height);
        if let Some(adjust) = gui.adjust_size {
            if !unsafe { adjust(self.plugin, &mut adjusted_width, &mut adjusted_height) } {
                (adjusted_width, adjusted_height) = (width, height);
            }
        }
        let accepted = gui.set_size.is_some_and(|set_size| unsafe {
            set_size(self.plugin, adjusted_width, adjusted_height)
        });
        if accepted {
            (adjusted_width, adjusted_height)
        } else {
            self.editor_size().unwrap_or(current)
        }
    }

    pub(crate) fn close_editor(&mut self) {
        if !self.editor_open {
            return;
        }
        if let Ok(gui) = self.gui() {
            if let Some(hide) = gui.hide {
                let _ = unsafe { hide(self.plugin) };
            }
            if let Some(destroy) = gui.destroy {
                unsafe { destroy(self.plugin) };
            }
        }
        self.editor_open = false;
    }
}
