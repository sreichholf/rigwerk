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
//! - `editor`: the `IPlugFrame` and the editor view the window attaches.
//! - `handler`: the `IComponentHandler` edits and restart requests reach.
//! - `params`: the preallocated parameter change lists `process` uses.
//! - `controller`: the main-thread half of a loaded plugin and its load
//!   sequence.
//! - `processor`: the audio-thread half.
//! - `scan`: finding bundles and reading `moduleinfo.json`, without
//!   loading a binary.
//!
//! Everything but `processor`, `params` and `scan` (the library scan's
//! own thread) is UI-thread work.  The bindings mark every interface
//! `Send + Sync`, so the compiler does not keep a pointer off the audio
//! thread: the audio side only ever holds a [`Vst3Processor`], and the
//! callbacks a plugin may make from another thread
//! (`IConnectionPoint::notify`, `performEdit`, `restartComponent`, run-loop
//! registration) use `try_lock`, atomics, or refuse, and never wait for the
//! UI.

mod connection;
mod controller;
mod editor;
mod factory;
mod handler;
mod host;
mod message;
mod module;
mod params;
mod processor;
#[cfg(target_os = "linux")]
mod run_loop;
mod scan;
mod stream;

pub(crate) use controller::{Vst3Controller, Vst3Loaded};
pub(crate) use editor::Vst3Editor;
#[cfg(test)]
pub(crate) use processor::BusLayout;
pub(crate) use processor::Vst3Processor;
#[cfg(test)]
pub(crate) use scan::tests::TestBundles;
pub(crate) use scan::{find_bundles, has_loadable_binary, read_module_info, search_roots};

use ::vst3::Steinberg::Vst::IProcessContextRequirements_::Flags_ as Needs;
use ::vst3::Steinberg::Vst::ProcessContext_::StatesAndFlags_ as States;
use ::vst3::Steinberg::Vst::{
    BusDirections_, BusTypes_, MediaTypes_, ProcessModes_, RestartFlags_, SymbolicSampleSizes_,
};

/// The `restartComponent` flags as the `u32` bits `PluginShared` keeps.
pub(crate) const LATENCY_CHANGED: u32 = RestartFlags_::kLatencyChanged as u32;
pub(crate) const PARAM_VALUES_CHANGED: u32 = RestartFlags_::kParamValuesChanged as u32;
pub(crate) const PARAM_TITLES_CHANGED: u32 = RestartFlags_::kParamTitlesChanged as u32;
pub(crate) const IO_CHANGED: u32 = RestartFlags_::kIoChanged as u32;
pub(crate) const RELOAD_COMPONENT: u32 = RestartFlags_::kReloadComponent as u32;

// The bindings' enums are `c_int` on Windows and `c_uint` elsewhere; the
// struct fields and arguments they go into are fixed-size integers.
#[allow(clippy::unnecessary_cast)]
const K_REALTIME: i32 = ProcessModes_::kRealtime as i32;
#[allow(clippy::unnecessary_cast)]
const K_SAMPLE32: i32 = SymbolicSampleSizes_::kSample32 as i32;
#[allow(clippy::unnecessary_cast)]
const K_AUDIO: i32 = MediaTypes_::kAudio as i32;
#[allow(clippy::unnecessary_cast)]
const K_EVENT: i32 = MediaTypes_::kEvent as i32;
#[allow(clippy::unnecessary_cast)]
const K_INPUT: i32 = BusDirections_::kInput as i32;
#[allow(clippy::unnecessary_cast)]
const K_OUTPUT: i32 = BusDirections_::kOutput as i32;
#[allow(clippy::unnecessary_cast)]
const K_MAIN: i32 = BusTypes_::kMain as i32;
#[allow(clippy::unnecessary_cast)]
const CONT_TIME_VALID: u32 = States::kContTimeValid as u32;
#[allow(clippy::unnecessary_cast)]
const TEMPO_VALID: u32 = States::kTempoValid as u32;
#[allow(clippy::unnecessary_cast)]
const TIME_SIG_VALID: u32 = States::kTimeSigValid as u32;
#[allow(clippy::unnecessary_cast)]
const NEED_TEMPO: u32 = Needs::kNeedTempo as u32;
#[allow(clippy::unnecessary_cast)]
const NEED_TIME_SIGNATURE: u32 = Needs::kNeedTimeSignature as u32;

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

/// The text in a fixed-size, NUL-terminated UTF-16 field (`String128`).
fn utf16_text(field: &[u16]) -> String {
    let end = field
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(field.len());
    String::from_utf16_lossy(&field[..end])
}

/// How many run-loop timers and fd handlers are registered, for the
/// editor tests.
#[cfg(all(test, target_os = "linux"))]
pub(crate) fn run_loop_counts() -> (usize, usize) {
    run_loop::counts()
}

/// The text in a fixed-size, NUL-terminated `char8` field of a VST3 struct.
fn c_text(field: &[std::ffi::c_char]) -> String {
    let end = field.iter().position(|&c| c == 0).unwrap_or(field.len());
    // `c_char` is signed on some targets: reinterpret each unit as a byte.
    let bytes: Vec<u8> = field[..end].iter().map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}
