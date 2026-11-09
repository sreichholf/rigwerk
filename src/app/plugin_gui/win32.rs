//! The Win32 window a CLAP editor embeds in.
//!
//! The window procedure only stores what happened (close, client size, new
//! DPI) in atomics on a boxed state the window points at; the UI thread
//! reads them in [`Win32Frame::poll`].  A fixed-size editor's window loses
//! its sizing border, so the user cannot drag it to a size the view ignores.

use std::mem;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use winapi::shared::minwindef::{LPARAM, LRESULT, UINT, WPARAM};
use winapi::shared::windef::{HWND, RECT};
use winapi::um::libloaderapi::GetModuleHandleW;
use winapi::um::winuser::{
    AdjustWindowRectEx, CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect,
    GetDpiForWindow, GetWindowLongPtrW, LoadCursorW, RegisterClassExW, SetWindowLongPtrW,
    SetWindowPos, ShowWindow, UpdateWindow, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, GWLP_USERDATA,
    GWL_STYLE, IDC_ARROW, SIZE_MINIMIZED, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_NOZORDER, SW_HIDE, SW_SHOW, WM_CLOSE, WM_DPICHANGED, WM_SIZE, WNDCLASSEXW, WS_MAXIMIZEBOX,
    WS_OVERLAPPEDWINDOW, WS_THICKFRAME,
};

use super::{EditorFrame, FrameEvents};
use crate::audio::clap::{ParentWindow, ResizeHints};

struct WindowState {
    closed: AtomicBool,
    client_size: AtomicU64,
    /// The DPI from the last `WM_DPICHANGED`; 0 when none is pending.
    dpi: AtomicU32,
}

pub(super) struct Win32Frame {
    hwnd: HWND,
    /// Pointed at by the window's `GWLP_USERDATA`; lives until the window.
    state: Box<WindowState>,
}

fn dpi_scale(dpi: u32) -> f64 {
    if dpi == 0 {
        1.0
    } else {
        f64::from(dpi) / 96.0
    }
}

impl Win32Frame {
    pub(super) fn create(title: &str, (width, height): (u32, u32)) -> Result<Self, String> {
        let state = Box::new(WindowState {
            closed: AtomicBool::new(false),
            client_size: AtomicU64::new(0),
            dpi: AtomicU32::new(0),
        });
        let hwnd = unsafe { create_window(title, width, height, &state)? };
        Ok(Self { hwnd, state })
    }

    fn style(&self) -> u32 {
        unsafe { GetWindowLongPtrW(self.hwnd, GWL_STYLE) as u32 }
    }
}

impl EditorFrame for Win32Frame {
    fn parent(&self) -> ParentWindow {
        ParentWindow::Win32(self.hwnd as *mut std::ffi::c_void)
    }

    fn own_scale(&self) -> Option<f64> {
        Some(dpi_scale(unsafe { GetDpiForWindow(self.hwnd) }))
    }

    fn poll(&mut self) -> FrameEvents {
        let closed = self.state.closed.load(Ordering::Relaxed);
        let packed = self.state.client_size.swap(0, Ordering::Relaxed);
        let size = (packed != 0).then_some(((packed >> 32) as u32, packed as u32));
        let dpi = self.state.dpi.swap(0, Ordering::Relaxed);
        FrameEvents {
            closed,
            size,
            scale: (dpi != 0).then(|| dpi_scale(dpi)),
        }
    }

    fn resize(&mut self, width: u32, height: u32) {
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        unsafe {
            AdjustWindowRectEx(&mut rect, self.style(), 0, 0);
            SetWindowPos(
                self.hwnd,
                ptr::null_mut(),
                0,
                0,
                rect.right - rect.left,
                rect.bottom - rect.top,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }

    fn set_resize_hints(&mut self, hints: ResizeHints, _size: (u32, u32)) {
        let sizing = WS_THICKFRAME | WS_MAXIMIZEBOX;
        let style = if hints.resizable() {
            self.style() | sizing
        } else {
            self.style() & !sizing
        };
        if style == self.style() {
            return;
        }
        unsafe {
            SetWindowLongPtrW(self.hwnd, GWL_STYLE, style as isize);
            SetWindowPos(
                self.hwnd,
                ptr::null_mut(),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
        }
    }

    fn show(&mut self) {
        unsafe {
            ShowWindow(self.hwnd, SW_SHOW);
            UpdateWindow(self.hwnd);
        }
    }

    fn hide(&mut self) {
        unsafe {
            ShowWindow(self.hwnd, SW_HIDE);
        }
    }
}

impl Drop for Win32Frame {
    fn drop(&mut self) {
        unsafe {
            SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0);
            DestroyWindow(self.hwnd);
        }
        let _ = &self.state;
    }
}

unsafe fn window_state(hwnd: HWND) -> Option<&'static WindowState> {
    let pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const WindowState;
    unsafe { pointer.as_ref() }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: UINT,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_CLOSE => {
            if let Some(state) = unsafe { window_state(hwnd) } {
                state.closed.store(true, Ordering::Relaxed);
            }
            return 0;
        }
        WM_SIZE if wparam != SIZE_MINIMIZED => {
            if let Some(state) = unsafe { window_state(hwnd) } {
                let mut rect: RECT = unsafe { mem::zeroed() };
                if unsafe { GetClientRect(hwnd, &mut rect) } != 0 {
                    let width = (rect.right - rect.left).max(0) as u64;
                    let height = (rect.bottom - rect.top).max(0) as u64;
                    state
                        .client_size
                        .store((width << 32) | height, Ordering::Relaxed);
                }
            }
            return 0;
        }
        WM_DPICHANGED => {
            // The new DPI is in the low word.  The editor's size at that
            // scale decides the window size, not the suggested rectangle.
            if let Some(state) = unsafe { window_state(hwnd) } {
                state.dpi.store((wparam & 0xffff) as u32, Ordering::Relaxed);
            }
            return 0;
        }
        _ => {}
    }
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

unsafe fn create_window(
    title: &str,
    width: u32,
    height: u32,
    state: &WindowState,
) -> Result<HWND, String> {
    static REGISTER: std::sync::Once = std::sync::Once::new();
    let class_name = "IndependentAudioBridgeClapWindow\0"
        .encode_utf16()
        .collect::<Vec<u16>>();
    unsafe {
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
