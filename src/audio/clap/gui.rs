//! CLAP editor calls, all main-thread under the plugin's UI-only lock.
//! Win32, X11, and Wayland use physical pixels.  CLAP has no Wayland
//! embedding, so there the plugin opens a floating toplevel and the host
//! only creates, shows, and destroys it.

#[cfg(target_os = "windows")]
use std::ffi::c_void;
use std::ffi::{c_char, CStr, CString};

#[cfg(target_os = "windows")]
use clap_sys::ext::gui::CLAP_WINDOW_API_WIN32;
use clap_sys::ext::gui::{clap_gui_resize_hints, clap_plugin_gui, clap_window, clap_window_handle};
#[cfg(any(target_os = "linux", test))]
use clap_sys::ext::gui::{CLAP_WINDOW_API_WAYLAND, CLAP_WINDOW_API_X11};
use clap_sys::plugin::clap_plugin;

use super::instance::ClapInstance;

const FALLBACK_SIZE: (u32, u32) = (640, 480);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WindowApi {
    #[cfg(target_os = "windows")]
    Win32,
    #[cfg(any(target_os = "linux", test))]
    X11,
    #[cfg(any(target_os = "linux", test))]
    Wayland,
}

impl WindowApi {
    pub(super) fn id(self) -> &'static CStr {
        // A Windows build has one variant.  The match covers the builds that
        // have more.
        #[cfg(all(target_os = "windows", not(test)))]
        {
            let Self::Win32 = self;
            CLAP_WINDOW_API_WIN32
        }
        #[cfg(any(not(target_os = "windows"), test))]
        match self {
            #[cfg(target_os = "windows")]
            Self::Win32 => CLAP_WINDOW_API_WIN32,
            #[cfg(any(target_os = "linux", test))]
            Self::X11 => CLAP_WINDOW_API_X11,
            #[cfg(any(target_os = "linux", test))]
            Self::Wayland => CLAP_WINDOW_API_WAYLAND,
        }
    }
}

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

#[derive(Clone, Copy, Debug)]
pub(crate) enum ParentWindow {
    #[cfg(target_os = "windows")]
    Win32(*mut c_void),
    #[cfg(any(target_os = "linux", test))]
    X11(u64),
}

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

/// Best first.  `DISPLAY` (XWayland included) embeds in X11, or floats
/// on X11.  A pure Wayland session can only float.
#[cfg(any(target_os = "linux", test))]
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

/// Best first.
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

impl ClapInstance {
    fn gui(&self) -> Result<&clap_plugin_gui, String> {
        unsafe { self.gui_ext.as_ref() }
            .ok_or_else(|| "This plugin has no GUI extension.".to_owned())
    }

    pub(crate) fn has_editor(&self) -> bool {
        self.editor_api.is_some()
    }

    pub(crate) fn editor_api(&self) -> Option<EditorApi> {
        self.editor_api
    }

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

    /// False when the plugin ignores it.  CLAP allows that for a plugin
    /// that reads the scale from the OS.
    pub(crate) fn set_editor_scale(&mut self, scale: f64) -> bool {
        if !self.editor_open || !scale.is_finite() || scale <= 0.0 {
            return false;
        }
        match self.gui().ok().and_then(|gui| gui.set_scale) {
            Some(set_scale) => unsafe { set_scale(self.plugin, scale) },
            None => false,
        }
    }

    pub(crate) fn attach_editor(&mut self, parent: ParentWindow, scale: f64) -> Result<(), String> {
        let (api, specific) = parent_handle(parent);
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

    /// No `can_resize`, or a no, is fixed.
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

    /// The size the window must take: the adjusted size, or the editor's
    /// own when it will not resize.
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

/// Windows tests still construct the X11 parent.  Every other build has
/// one variant, and a one-arm match is rejected.
fn parent_handle(parent: ParentWindow) -> (WindowApi, clap_window_handle) {
    #[cfg(not(all(target_os = "windows", test)))]
    {
        #[cfg(target_os = "windows")]
        {
            let ParentWindow::Win32(hwnd) = parent;
            (WindowApi::Win32, clap_window_handle { win32: hwnd })
        }
        #[cfg(not(target_os = "windows"))]
        {
            let ParentWindow::X11(window) = parent;
            (
                WindowApi::X11,
                clap_window_handle {
                    x11: window as clap_sys::ext::gui::clap_xwnd,
                },
            )
        }
    }
    #[cfg(all(target_os = "windows", test))]
    {
        match parent {
            ParentWindow::Win32(hwnd) => (WindowApi::Win32, clap_window_handle { win32: hwnd }),
            ParentWindow::X11(window) => (
                WindowApi::X11,
                clap_window_handle {
                    x11: window as clap_sys::ext::gui::clap_xwnd,
                },
            ),
        }
    }
}
