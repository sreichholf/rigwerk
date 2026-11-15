//! RigWerk's own VST3 host on the `vst3` bindings: the pieces every loaded
//! plugin shares.  Linux and Windows; elsewhere `module::acquire` refuses.
//!
//! - `module`: one loaded bundle per process (`ModuleEntry`/`ModuleExit`
//!   on Linux, `InitDll`/`ExitDll` on Windows) and the registry that keeps
//!   it loaded once.
//! - `factory`: class enumeration, class-id text, `createInstance`.
//! - `host`: the one process-wide host context (`IHostApplication`,
//!   `IPlugInterfaceSupport`, and on Linux `IRunLoop`).
//! - `message`: `IMessage` and `IAttributeList` for `allocateMessage`.
//! - `connection`: the component/controller proxies.
//! - `stream`: the in-memory `IBStream` and the saved-state envelope.
//! - `run_loop`: the Linux `IRunLoop` registry, serviced on the UI thread.
//! - `editor`: the `IPlugFrame` an editor view is attached with.
//!
//! Everything here is UI-thread work.  The bindings mark every interface
//! `Send + Sync`, so the compiler does not keep a pointer off the audio
//! thread: the callbacks a plugin may make from another thread
//! (`IConnectionPoint::notify`, run-loop registration) use `try_lock` or
//! refuse, and never wait for the UI.

// No callers yet: the processor/controller split moves VST3 hosting here (#28).
#![cfg_attr(not(test), allow(dead_code))]

mod connection;
mod editor;
mod factory;
mod host;
mod message;
mod module;
#[cfg(target_os = "linux")]
mod run_loop;
mod stream;

#[cfg(test)]
mod tests;

/// Services the plugins' run-loop timers and file descriptors.  Call it
/// once per UI frame.  Only Linux has a host run loop.
#[cfg(target_os = "linux")]
pub(crate) fn service_run_loop() {
    run_loop::service_run_loop();
}

/// Services the plugins' run-loop timers and file descriptors.  Call it
/// once per UI frame.  Only Linux has a host run loop.
#[cfg(not(target_os = "linux"))]
pub(crate) fn service_run_loop() {}

/// The text in a fixed-size, NUL-terminated `char8` field of a VST3 struct.
fn c_text(field: &[std::ffi::c_char]) -> String {
    let end = field.iter().position(|&c| c == 0).unwrap_or(field.len());
    // `c_char` is signed on some targets: reinterpret each unit as a byte.
    let bytes: Vec<u8> = field[..end].iter().map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}
