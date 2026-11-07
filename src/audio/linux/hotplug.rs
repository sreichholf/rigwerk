//! Sound-card add and remove events from the kernel.
//!
//! A configuration probe cannot tell "unplugged" from "busy": opening an
//! ALSA PCM that the bridge already holds fails, and the old scan treated
//! that failure as the device going away. The kernel publishes the real
//! change on the uevent netlink. The UI thread only enumerates when one of
//! those messages names the sound subsystem, and still polls slowly so a
//! PipeWire node that never creates a sound card is not missed.

use std::sync::mpsc::{self, Receiver};
use std::thread;

/// `true` when a uevent is a sound device appearing, leaving, or changing.
///
/// Messages are NUL-separated `KEY=value` fields, sometimes with a first
/// line like `add@/devices/...`.
pub(crate) fn uevent_is_sound_change(message: &str) -> bool {
    let mut action = false;
    let mut sound = false;
    for field in message.split(['\0', '\n']) {
        let field = field.trim();
        if let Some(name) = field.strip_prefix("ACTION=") {
            action = matches!(name, "add" | "remove" | "change");
        } else if field == "SUBSYSTEM=sound" {
            sound = true;
        } else if let Some(rest) = field.strip_prefix("add@") {
            action = true;
            if rest.contains("/sound/") {
                sound = true;
            }
        } else if field.starts_with("remove@") || field.starts_with("change@") {
            action = true;
            if field.contains("/sound/") {
                sound = true;
            }
        }
    }
    action && sound
}

/// Blocks until a sound uevent arrives, then sends on the channel.
///
/// If the netlink socket cannot be opened, the thread exits and the caller
/// keeps its slow poll. One process-wide watch is enough.
pub(crate) fn spawn() -> Receiver<()> {
    let (sender, receiver) = mpsc::channel();
    let _ = thread::Builder::new()
        .name("rigwerk-hotplug".to_owned())
        .spawn(move || {
            let Some(fd) = open_uevent_socket() else {
                return;
            };
            let mut buf = [0u8; 8192];
            loop {
                let read = unsafe { libc::recv(fd, buf.as_mut_ptr().cast(), buf.len(), 0) };
                if read <= 0 {
                    break;
                }
                let text = String::from_utf8_lossy(&buf[..read as usize]);
                if uevent_is_sound_change(&text) && sender.send(()).is_err() {
                    break;
                }
            }
            unsafe { libc::close(fd) };
        });
    receiver
}

fn open_uevent_socket() -> Option<libc::c_int> {
    unsafe {
        let fd = libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC,
            libc::NETLINK_KOBJECT_UEVENT,
        );
        if fd < 0 {
            return None;
        }
        let mut addr: libc::sockaddr_nl = std::mem::zeroed();
        addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        // Group 1 is the kernel's kobject broadcast.
        addr.nl_groups = 1;
        let bound = libc::bind(
            fd,
            (&addr as *const libc::sockaddr_nl).cast(),
            std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        );
        if bound < 0 {
            libc::close(fd);
            return None;
        }
        Some(fd)
    }
}

#[cfg(test)]
mod tests;
