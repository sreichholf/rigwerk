//! The real plugins the fixture tests load: Steinberg's VST3 SDK samples,
//! built by `scripts/vst3-fixtures.sh`, and the LV2 examples built by
//! `scripts/lv2-fixtures.sh`.
//!
//! `RIGWERK_VST3_FIXTURES` and `RIGWERK_LV2_FIXTURES` name the folders that
//! hold the bundles.  Unset, the tests return early and pass, like the
//! `RIGWERK_CLAP_FIXTURE` ones.  Set, every bundle is expected: a missing one
//! fails the test instead of skipping it.
//!
//! The app loads plugins on its UI thread, which winit has joined to a COM
//! apartment on Windows.  A test thread is not.  VSTGUI creates its WIC
//! factory when the module loads, gets none outside an apartment, and the
//! editor then crashes loading its first bitmap.  So the VST3 helper joins
//! the calling thread the way the UI thread is before a test loads anything.

use std::path::PathBuf;

/// The bundle `<name>.vst3` in `RIGWERK_VST3_FIXTURES`, or `None` when the
/// variable is unset or empty.  Panics when the variable is set and the
/// bundle is not there.  Call it on the thread that loads the plugin.
pub(crate) fn vst3_fixture(name: &str) -> Option<PathBuf> {
    let dir = std::env::var_os("RIGWERK_VST3_FIXTURES").filter(|dir| !dir.is_empty())?;
    let path = PathBuf::from(dir).join(format!("{name}.vst3"));
    assert!(
        path.is_dir(),
        "RIGWERK_VST3_FIXTURES is set but {} is missing; run scripts/vst3-fixtures.sh",
        path.display()
    );
    join_com_apartment();
    Some(path)
}

/// The bundle `<name>.lv2` in `RIGWERK_LV2_FIXTURES`, or `None` when the
/// variable is unset or empty.  Panics when the variable is set and the
/// bundle is not there.
pub(crate) fn lv2_fixture(name: &str) -> Option<PathBuf> {
    let dir = std::env::var_os("RIGWERK_LV2_FIXTURES").filter(|dir| !dir.is_empty())?;
    let path = PathBuf::from(dir).join(format!("{name}.lv2"));
    assert!(
        path.is_dir(),
        "RIGWERK_LV2_FIXTURES is set but {} is missing; run scripts/lv2-fixtures.sh",
        path.display()
    );
    Some(path)
}

/// Joins the calling thread to a single-threaded COM apartment, as the UI
/// thread is.  A thread that already joined one keeps it.
#[cfg(target_os = "windows")]
fn join_com_apartment() {
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
    // S_FALSE (already joined) is fine; the thread is never uninitialized.
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
}

#[cfg(not(target_os = "windows"))]
fn join_com_apartment() {}
