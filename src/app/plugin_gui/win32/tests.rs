use super::*;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, PeekMessageW, PostMessageW, MSG, PM_REMOVE,
};

#[cfg(target_os = "windows")]
#[test]
fn win32_frame_opens_sizes_and_closes() {
    let mut frame = Win32Frame::create("RigWerk test", (320, 240)).expect("create a Win32 frame");
    let resizable = WS_THICKFRAME | WS_MAXIMIZEBOX;
    assert_eq!(frame.style() & resizable, resizable);
    let dpi = unsafe { GetDpiForWindow(frame.hwnd) };
    assert_eq!(frame.own_scale(), Some(f64::from(dpi) / 96.0));

    frame.resize(400, 300);
    frame.set_resize_hints(ResizeHints::FIXED, (400, 300));
    assert_eq!(frame.style() & resizable, WINDOW_STYLE(0));
    frame.set_resize_hints(
        ResizeHints {
            horizontal: true,
            vertical: true,
            aspect: None,
        },
        (400, 300),
    );
    assert_eq!(frame.style() & resizable, resizable);

    frame.show();
    frame.hide();
    assert!(!frame.poll().closed);

    unsafe {
        PostMessageW(Some(frame.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)).expect("post WM_CLOSE");
        let mut message = MSG::default();
        while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
            DispatchMessageW(&message);
        }
    }
    assert!(frame.poll().closed);
    drop(frame);
}
