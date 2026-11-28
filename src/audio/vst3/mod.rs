//! VST3 host.  Linux and Windows; elsewhere `module::acquire` refuses.
//!
//! Everything but `processor`, `params`, and the scan thread is UI-thread
//! work.  The bindings are `Send + Sync`, so the audio side holds only a
//! [`Vst3Processor`].  A callback from another thread (`notify`,
//! `performEdit`, `restartComponent`, run-loop registration) `try_lock`s,
//! stores atomics, or refuses, and never waits.  `scan` does not load a
//! binary.

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

/// Once per UI frame.  Only Linux has a host run loop.
#[cfg(target_os = "linux")]
pub(crate) fn service_run_loop() {
    run_loop::service_run_loop();
}

/// Once per UI frame.  Only Linux has a host run loop.
#[cfg(not(target_os = "linux"))]
pub(crate) fn service_run_loop() {}

/// NUL-terminated UTF-16 (`String128`).
fn utf16_text(field: &[u16]) -> String {
    let end = field
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(field.len());
    String::from_utf16_lossy(&field[..end])
}

#[cfg(all(test, target_os = "linux"))]
pub(crate) fn run_loop_counts() -> (usize, usize) {
    run_loop::counts()
}

/// NUL-terminated `char8`.
fn c_text(field: &[std::ffi::c_char]) -> String {
    let end = field.iter().position(|&c| c == 0).unwrap_or(field.len());
    // `c_char` is signed on some targets: reinterpret each unit as a byte.
    let bytes: Vec<u8> = field[..end].iter().map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}
