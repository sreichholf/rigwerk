//! The X11 window a CLAP editor embeds in, on its own XCB connection.
//!
//! The plugin reparents its view into [`X11Frame::parent`] over a connection
//! of its own, so this one only sees the frame's events: the close button
//! (`WM_DELETE_WINDOW`) and size changes (`ConfigureNotify`).  Size limits go
//! to the window manager as `WM_NORMAL_HINTS`.

use xcb::{x, Xid};

use super::{EditorFrame, FrameEvents};
use crate::audio::clap::{ParentWindow, ResizeHints};

/// `WM_SIZE_HINTS` flags (ICCCM 4.1.2.3).
const P_MIN_SIZE: u32 = 1 << 4;
const P_MAX_SIZE: u32 = 1 << 5;
const P_ASPECT: u32 = 1 << 7;

/// The largest size X11 can describe; a free axis is capped here.
const MAX_SIDE: u32 = u16::MAX as u32;

pub(super) struct X11Frame {
    connection: xcb::Connection,
    window: x::Window,
    wm_protocols: x::Atom,
    wm_delete_window: x::Atom,
}

fn intern(connection: &xcb::Connection, name: &[u8]) -> Result<x::Atom, String> {
    let cookie = connection.send_request(&x::InternAtom {
        only_if_exists: false,
        name,
    });
    connection
        .wait_for_reply(cookie)
        .map(|reply| reply.atom())
        .map_err(|error| format!("The X server did not answer: {error}"))
}

fn side(value: u32) -> u16 {
    value.clamp(1, MAX_SIDE) as u16
}

impl X11Frame {
    pub(super) fn create(title: &str, (width, height): (u32, u32)) -> Result<Self, String> {
        let (connection, screen_number) = xcb::Connection::connect(None)
            .map_err(|error| format!("Could not reach the X server: {error}"))?;
        let screen = connection
            .get_setup()
            .roots()
            .nth(screen_number.max(0) as usize)
            .ok_or_else(|| "The X server reported no screen.".to_owned())?;
        let (root, visual, black) = (screen.root(), screen.root_visual(), screen.black_pixel());
        let window: x::Window = connection.generate_id();
        connection
            .send_and_check_request(&x::CreateWindow {
                depth: x::COPY_FROM_PARENT as u8,
                wid: window,
                parent: root,
                x: 0,
                y: 0,
                width: side(width),
                height: side(height),
                border_width: 0,
                class: x::WindowClass::InputOutput,
                visual,
                value_list: &[
                    x::Cw::BackPixel(black),
                    x::Cw::EventMask(x::EventMask::STRUCTURE_NOTIFY),
                ],
            })
            .map_err(|error| format!("Could not create an X11 window: {error}"))?;
        let frame = Self {
            wm_protocols: intern(&connection, b"WM_PROTOCOLS")?,
            wm_delete_window: intern(&connection, b"WM_DELETE_WINDOW")?,
            connection,
            window,
        };
        let net_wm_name = intern(&frame.connection, b"_NET_WM_NAME")?;
        let utf8_string = intern(&frame.connection, b"UTF8_STRING")?;
        let title = format!("{title} - CLAP");
        frame.connection.send_request(&x::ChangeProperty {
            mode: x::PropMode::Replace,
            window,
            property: x::ATOM_WM_NAME,
            r#type: x::ATOM_STRING,
            data: title.as_bytes(),
        });
        frame.connection.send_request(&x::ChangeProperty {
            mode: x::PropMode::Replace,
            window,
            property: net_wm_name,
            r#type: utf8_string,
            data: title.as_bytes(),
        });
        frame.connection.send_request(&x::ChangeProperty {
            mode: x::PropMode::Replace,
            window,
            property: x::ATOM_WM_CLASS,
            r#type: x::ATOM_STRING,
            data: b"rigwerk\0RigWerk\0",
        });
        frame.connection.send_request(&x::ChangeProperty {
            mode: x::PropMode::Replace,
            window,
            property: frame.wm_protocols,
            r#type: x::ATOM_ATOM,
            data: &[frame.wm_delete_window],
        });
        let _ = frame.connection.flush();
        Ok(frame)
    }
}

impl EditorFrame for X11Frame {
    fn parent(&self) -> ParentWindow {
        ParentWindow::X11(u64::from(self.window.resource_id()))
    }

    fn own_scale(&self) -> Option<f64> {
        // The core protocol has no per-window DPI; the app's scale is used.
        None
    }

    fn poll(&mut self) -> FrameEvents {
        let mut events = FrameEvents::default();
        loop {
            match self.connection.poll_for_event() {
                Ok(Some(xcb::Event::X(x::Event::ClientMessage(message)))) => {
                    if message.r#type() == self.wm_protocols {
                        if let x::ClientMessageData::Data32(data) = message.data() {
                            if data[0] == self.wm_delete_window.resource_id() {
                                events.closed = true;
                            }
                        }
                    }
                }
                Ok(Some(xcb::Event::X(x::Event::ConfigureNotify(configure)))) => {
                    if configure.window() == self.window {
                        events.size =
                            Some((u32::from(configure.width()), u32::from(configure.height())));
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                // A failed request is reported and skipped; a lost
                // connection means the window is gone.
                Err(xcb::Error::Protocol(_)) => {}
                Err(xcb::Error::Connection(_)) => {
                    events.closed = true;
                    break;
                }
            }
        }
        events
    }

    fn resize(&mut self, width: u32, height: u32) {
        self.connection.send_request(&x::ConfigureWindow {
            window: self.window,
            value_list: &[
                x::ConfigWindow::Width(u32::from(side(width))),
                x::ConfigWindow::Height(u32::from(side(height))),
            ],
        });
        let _ = self.connection.flush();
    }

    fn set_resize_hints(&mut self, hints: ResizeHints, size: (u32, u32)) {
        let data = normal_hints(hints, size);
        self.connection.send_request(&x::ChangeProperty {
            mode: x::PropMode::Replace,
            window: self.window,
            property: x::ATOM_WM_NORMAL_HINTS,
            r#type: x::ATOM_WM_SIZE_HINTS,
            data: &data,
        });
        let _ = self.connection.flush();
    }

    fn show(&mut self) {
        self.connection.send_request(&x::MapWindow {
            window: self.window,
        });
        let _ = self.connection.flush();
    }

    fn hide(&mut self) {
        self.connection.send_request(&x::UnmapWindow {
            window: self.window,
        });
        let _ = self.connection.flush();
    }
}

impl Drop for X11Frame {
    fn drop(&mut self) {
        self.connection.send_request(&x::DestroyWindow {
            window: self.window,
        });
        let _ = self.connection.flush();
    }
}

/// The 18 words of `WM_NORMAL_HINTS` for these hints at this size.  An axis
/// the editor cannot resize is pinned to its current length.
pub(super) fn normal_hints(hints: ResizeHints, (width, height): (u32, u32)) -> [u32; 18] {
    let (width, height) = (u32::from(side(width)), u32::from(side(height)));
    let (min_width, max_width) = if hints.horizontal {
        (1, MAX_SIDE)
    } else {
        (width, width)
    };
    let (min_height, max_height) = if hints.vertical {
        (1, MAX_SIDE)
    } else {
        (height, height)
    };
    let mut flags = P_MIN_SIZE | P_MAX_SIZE;
    let (aspect_x, aspect_y) = match hints.aspect {
        Some(ratio) if hints.resizable() => {
            flags |= P_ASPECT;
            ratio
        }
        _ => (0, 0),
    };
    [
        flags, 0, 0, 0, 0, min_width, min_height, max_width, max_height, 0, 0, aspect_x, aspect_y,
        aspect_x, aspect_y, 0, 0, 0,
    ]
}

#[cfg(test)]
mod tests;
