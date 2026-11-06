//! The plugin's own graphical editor, in a window of the plugin's making.
//!
//! VST3 editors are opened through `vst3-host`'s [`PluginWindow`].  CLAP
//! editors are embedded in a native Win32 window created here, because CLAP's
//! GUI extension expects the host to provide the parent `HWND`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use vst3_host::PluginWindow;

use crate::app::RigWerkApp;
use crate::audio::clap::ClapPlugin;
use crate::audio::{plugin_instance_id, PluginHandle, PluginView};
use crate::ui::theme::{GREEN, RED};

pub(crate) struct PluginGuiWindow {
    /// Position of the insert in the master chain.
    pub(crate) index: usize,
    kind: GuiKind,
    last_sync: Instant,
}

enum GuiKind {
    Vst3 {
        plugin: Arc<Mutex<vst3_host::Plugin>>,
        window: PluginWindow,
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

    /// Whether the insert at this editor's index still holds its instance.
    /// Read from the published views, so polling an open editor every frame
    /// never takes the chain lock the audio callback needs.
    fn matches(&self, view: Option<&PluginView>) -> bool {
        view.and_then(|view| view.instance) == Some(self.instance_id())
    }
}

impl RigWerkApp {
    /// Opens the plugin's own interface for one insert, replacing any editor
    /// that is already open.  Returns whether a window could be opened.
    pub(crate) fn open_plugin_gui(&mut self, index: usize) -> bool {
        self.engine.ensure_plugin_loaded(index);
        // Dropping the old window detaches its editor before the new one opens.
        self.finish_plugin_gui();
        let Some(handle) = self.engine.plugin_handle(index) else {
            self.plugin_browser.notice = Some((
                "The effect is not loaded, so its interface cannot open.".to_owned(),
                Instant::now(),
            ));
            return false;
        };
        let name = self
            .engine
            .plugin_views()
            .get(index)
            .map(|view| view.name.clone())
            .unwrap_or_else(|| "Plugin".to_owned());
        let opened = match &handle {
            PluginHandle::Vst3(plugin) => {
                let mut window = PluginWindow::new(plugin.clone());
                match window.open() {
                    Ok(()) => {
                        self.plugin_gui = Some(PluginGuiWindow {
                            index,
                            kind: GuiKind::Vst3 {
                                plugin: plugin.clone(),
                                window,
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
            PluginHandle::Clap(plugin) => match ClapGuiWindow::open(plugin.clone(), &name) {
                Ok(window) => {
                    self.plugin_gui = Some(PluginGuiWindow {
                        index,
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
            },
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

    /// Drops the editor after a last sync, so whatever it changed since the
    /// previous 250 ms sync reaches the saved insert and its state blob.
    pub(crate) fn finish_plugin_gui(&mut self) {
        if let Some(index) = self.plugin_gui.as_ref().map(|gui| gui.index) {
            self.engine.finish_plugin_editor(index);
        }
        self.plugin_gui = None;
    }

    /// Servicing and keeping the open editor in step, once per frame.
    pub(crate) fn poll_plugin_gui(&mut self) {
        let Some(index) = self.plugin_gui.as_ref().map(|gui| gui.index) else {
            return;
        };
        let views = self.engine.plugin_views();
        let (dismissed, same_plugin, due) = {
            let Some(gui) = self.plugin_gui.as_mut() else {
                return;
            };
            let dismissed = match &mut gui.kind {
                GuiKind::Vst3 { window, .. } => {
                    let _ = window.service_platform_events();
                    window.closed_by_user() || !window.is_open()
                }
                GuiKind::Clap { window, .. } => window.service(),
            };
            let same_plugin = gui.matches(views.get(index));
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
            // A plugin's editor can change anything at any time; copying the
            // values back keeps the saved insert (and the panel) truthful.
            let _ = self.engine.sync_plugin_parameters(index);
        }
    }
}

#[cfg(target_os = "windows")]
struct ClapGuiWindow {
    plugin: Arc<Mutex<ClapPlugin>>,
    host: Arc<crate::audio::clap::HostState>,
    hwnd: winapi::shared::windef::HWND,
    state: Box<ClapWindowState>,
    last_client: Option<(u32, u32)>,
}

#[cfg(target_os = "windows")]
struct ClapWindowState {
    closed: std::sync::atomic::AtomicBool,
    client_size: std::sync::atomic::AtomicU64,
}

#[cfg(target_os = "windows")]
impl ClapGuiWindow {
    fn open(plugin: Arc<Mutex<ClapPlugin>>, title: &str) -> Result<Self, String> {
        let (width, height, host) = {
            let mut plugin = plugin.lock().unwrap_or_else(|poison| poison.into_inner());
            let (width, height) = plugin.prepare_editor()?;
            (width, height, plugin.host())
        };
        let state = Box::new(ClapWindowState {
            closed: std::sync::atomic::AtomicBool::new(false),
            client_size: std::sync::atomic::AtomicU64::new(0),
        });
        let hwnd = create_window(title, width, height, &state)?;

        let scale = unsafe {
            let dpi = winapi::um::winuser::GetDpiForWindow(hwnd);
            if dpi == 0 {
                1.0
            } else {
                dpi as f64 / 96.0
            }
        };
        let attached = {
            let mut locked = plugin.lock().unwrap_or_else(|poison| poison.into_inner());
            locked.attach_editor(hwnd as *mut std::ffi::c_void, scale)
        };
        if let Err(error) = attached {
            let mut locked = plugin.lock().unwrap_or_else(|poison| poison.into_inner());
            locked.close_editor();
            unsafe {
                winapi::um::winuser::DestroyWindow(hwnd);
            }
            return Err(error);
        }
        unsafe {
            winapi::um::winuser::ShowWindow(hwnd, winapi::um::winuser::SW_SHOW);
            winapi::um::winuser::UpdateWindow(hwnd);
        }
        Ok(Self {
            plugin,
            host,
            hwnd,
            state,
            last_client: None,
        })
    }

    /// Runs the plugin's main-thread work and follows its resize requests.
    /// Returns true once the window should be closed.
    fn service(&mut self) -> bool {
        use std::sync::atomic::Ordering;
        if self.state.closed.load(Ordering::Relaxed) {
            return true;
        }
        // Timers and `on_main_thread` (where most editors repaint) run
        // without the instance lock, so the audio callback keeps processing
        // this insert while the editor is busy.  A latency change it picks
        // up stays flagged on the host, so the path's `service_plugins`
        // still republishes the insert views; the result is not needed here.
        let _ = crate::audio::service_clap(&self.host, &self.plugin);
        if self.host.take_gui_closed() {
            return true;
        }
        if let Some((width, height)) = self.host.take_gui_resize() {
            unsafe {
                resize_window(self.hwnd, width, height);
            }
            self.last_client = Some((width, height));
            return false;
        }
        let packed = self.state.client_size.swap(0, Ordering::Relaxed);
        if packed != 0 {
            let width = (packed >> 32) as u32;
            let height = packed as u32;
            if self.last_client != Some((width, height)) {
                self.last_client = Some((width, height));
                // A resize is a user action and rare; it is the one editor
                // call that still takes the instance lock.
                self.plugin
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .resize_editor(width, height);
            }
        }
        false
    }
}

#[cfg(target_os = "windows")]
impl Drop for ClapGuiWindow {
    fn drop(&mut self) {
        // Detach the editor before destroying the parent window.
        let mut plugin = self
            .plugin
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        plugin.close_editor();
        drop(plugin);
        unsafe {
            winapi::um::winuser::DestroyWindow(self.hwnd);
        }
        let _ = &self.state;
    }
}

#[cfg(target_os = "windows")]
fn create_window(
    title: &str,
    width: u32,
    height: u32,
    state: &ClapWindowState,
) -> Result<winapi::shared::windef::HWND, String> {
    use std::mem;
    use std::ptr;
    use winapi::shared::minwindef::UINT;
    use winapi::shared::windef::HWND;
    use winapi::shared::windef::RECT;
    use winapi::um::libloaderapi::GetModuleHandleW;
    use winapi::um::winuser::{
        AdjustWindowRectEx, CreateWindowExW, DefWindowProcW, GetClientRect, GetWindowLongPtrW,
        LoadCursorW, RegisterClassExW, SetWindowLongPtrW, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT,
        GWLP_USERDATA, IDC_ARROW, SIZE_MINIMIZED, WM_CLOSE, WM_SIZE, WNDCLASSEXW,
        WS_OVERLAPPEDWINDOW,
    };

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        message: UINT,
        wparam: winapi::shared::minwindef::WPARAM,
        lparam: winapi::shared::minwindef::LPARAM,
    ) -> winapi::shared::minwindef::LRESULT {
        if message == WM_CLOSE {
            if let Some(state) = window_state(hwnd) {
                state
                    .closed
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
            return 0;
        }
        if message == WM_SIZE && wparam != SIZE_MINIMIZED {
            if let Some(state) = window_state(hwnd) {
                let mut rect: RECT = mem::zeroed();
                if GetClientRect(hwnd, &mut rect) != 0 {
                    let width = (rect.right - rect.left).max(0) as u64;
                    let height = (rect.bottom - rect.top).max(0) as u64;
                    state
                        .client_size
                        .store((width << 32) | height, std::sync::atomic::Ordering::Relaxed);
                }
            }
            return 0;
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }

    unsafe fn window_state(hwnd: HWND) -> Option<&'static ClapWindowState> {
        let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ClapWindowState;
        if pointer.is_null() {
            None
        } else {
            Some(&*pointer)
        }
    }

    unsafe {
        static REGISTER: std::sync::Once = std::sync::Once::new();
        let class_name = "IndependentAudioBridgeClapWindow\0"
            .encode_utf16()
            .collect::<Vec<u16>>();
        REGISTER.call_once(|| {
            let mut class: WNDCLASSEXW = mem::zeroed();
            class.cbSize = mem::size_of::<WNDCLASSEXW>() as UINT;
            class.style = CS_HREDRAW | CS_VREDRAW;
            class.lpfnWndProc = Some(window_proc);
            class.hInstance = GetModuleHandleW(ptr::null());
            class.hCursor = LoadCursorW(ptr::null_mut(), IDC_ARROW);
            class.lpszClassName = class_name.as_ptr();
            RegisterClassExW(&class);
        });

        let mut rect = RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        AdjustWindowRectEx(&mut rect, WS_OVERLAPPEDWINDOW, 0, 0);
        let window_title = format!("{title} - CLAP\0")
            .encode_utf16()
            .collect::<Vec<u16>>();
        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            window_title.as_ptr(),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            rect.right - rect.left,
            rect.bottom - rect.top,
            ptr::null_mut(),
            ptr::null_mut(),
            GetModuleHandleW(ptr::null()),
            ptr::null_mut(),
        );
        if hwnd.is_null() {
            return Err("Could not create a window for the CLAP interface.".to_owned());
        }
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as *const _ as isize);
        Ok(hwnd)
    }
}

#[cfg(target_os = "windows")]
unsafe fn resize_window(hwnd: winapi::shared::windef::HWND, width: u32, height: u32) {
    use winapi::shared::windef::RECT;
    use winapi::um::winuser::{
        AdjustWindowRectEx, SetWindowPos, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER,
        WS_OVERLAPPEDWINDOW,
    };
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: width as i32,
        bottom: height as i32,
    };
    unsafe {
        AdjustWindowRectEx(&mut rect, WS_OVERLAPPEDWINDOW, 0, 0);
        SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            0,
            0,
            rect.right - rect.left,
            rect.bottom - rect.top,
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

// The other platforms do not have a native window implementation for CLAP
// editors yet.  Plugins still process audio and expose their controls in the
// the RigWerk panel; only the "open the plugin's own view" path is unavailable.
#[cfg(not(target_os = "windows"))]
struct ClapGuiWindow;

#[cfg(not(target_os = "windows"))]
impl ClapGuiWindow {
    fn open(_plugin: Arc<Mutex<ClapPlugin>>, _title: &str) -> Result<Self, String> {
        Err("CLAP interfaces can only be embedded on Windows in this build.".to_owned())
    }

    fn service(&mut self) -> bool {
        true
    }
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn opens_and_closes_a_clap_window() {
        let Ok(path) = std::env::var("RIGWERK_CLAP_FIXTURE") else {
            return;
        };
        let path = PathBuf::from(path);
        let candidates = crate::audio::clap::scan_bundle(&path).expect("scan the fixture");
        let Some(candidate) = candidates.first() else {
            return;
        };
        let plugin = Arc::new(Mutex::new(
            ClapPlugin::load_fresh(&path, &candidate.plugin_id, 48_000, 512)
                .expect("load the fixture"),
        ));
        if !plugin
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .has_editor()
        {
            return;
        }
        let mut window =
            ClapGuiWindow::open(plugin.clone(), &candidate.name).expect("open the fixture window");
        assert!(!window.service());
        drop(window);
    }
}
