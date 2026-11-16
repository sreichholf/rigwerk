use std::path::PathBuf;

use super::{acquire, binary_path, ModuleCode, ARCH_FOLDERS, BINARY_EXTENSION};

/// What a plugin would do if its `ModuleExit` (or a release during an
/// unload) loaded a module: the registry lock is already held further up.
#[cfg(any(target_os = "linux", target_os = "windows"))]
#[test]
fn registry_reentry_during_module_code_panics_instead_of_deadlocking() {
    let missing = std::env::temp_dir().join("rigwerk-vst3-missing.vst3");
    let code = ModuleCode::enter();
    let reentry = std::panic::catch_unwind(|| acquire(&missing).is_ok());
    let message = reentry.expect_err("re-entry panics");
    let message = message
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| message.downcast_ref::<&str>().copied())
        .unwrap_or_default();
    assert!(
        message.contains("re-entered the module registry"),
        "{message}"
    );
    drop(code);
    // Outside module code the registry works as usual.
    assert!(acquire(&missing).is_err());
}

/// A bundle directory in the temp dir, removed on drop.
struct TempBundle(PathBuf);

impl TempBundle {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("rigwerk-vst3-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("temp dir");
        Self(root)
    }

    fn file(&self, relative: &str) -> PathBuf {
        let path = self.0.join(relative);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("folders");
        std::fs::write(&path, b"").expect("file");
        path
    }
}

impl Drop for TempBundle {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
#[test]
fn bundle_binary_is_the_named_one_for_this_arch() {
    let root = TempBundle::new("layout");
    let arch = ARCH_FOLDERS[0];
    let bundle = root.0.join("Gain.vst3");
    // Another architecture's binary is never picked.
    root.file(&format!(
        "Gain.vst3/Contents/other-arch/Gain.{BINARY_EXTENSION}"
    ));
    let error = binary_path(&bundle).expect_err("no binary for this arch");
    assert!(error.contains(arch), "{error}");

    // Any binary in this arch's folder, the bundle-named one first.
    let other = root.file(&format!("Gain.vst3/Contents/{arch}/aaa.{BINARY_EXTENSION}"));
    assert_eq!(binary_path(&bundle), Ok(other));
    let named = root.file(&format!(
        "Gain.vst3/Contents/{arch}/Gain.{BINARY_EXTENSION}"
    ));
    assert_eq!(binary_path(&bundle), Ok(named));

    // A plain file is the legacy single-file layout.
    let single = root.file("Single.vst3");
    assert_eq!(binary_path(&single), Ok(single.clone()));
}
