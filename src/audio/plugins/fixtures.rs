//! VST3 and LV2 fixture bundles (`scripts/vst3-fixtures.sh`,
//! `scripts/lv2-fixtures.sh`).  An unset `RIGWERK_*_FIXTURES` skips the
//! test; a set one requires every bundle.
//!
//! On Windows a test thread is not in a COM apartment.  VSTGUI builds its
//! WIC factory at module load and the editor crashes on the first bitmap
//! without one, so the VST3 helper joins the calling thread first.

use std::path::PathBuf;

/// `None` when `RIGWERK_VST3_FIXTURES` is unset; panics if the bundle is
/// missing.  Call it on the thread that loads the plugin.
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

/// `None` when `RIGWERK_LV2_FIXTURES` is unset; panics if the bundle is
/// missing.
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

#[cfg(target_os = "windows")]
fn join_com_apartment() {
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
    // S_FALSE (already joined) is fine; the thread is never uninitialized.
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
}

#[cfg(not(target_os = "windows"))]
fn join_com_apartment() {}
