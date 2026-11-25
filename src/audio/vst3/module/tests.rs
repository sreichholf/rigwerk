use std::io::Cursor;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use super::{
    binary_path, read_image, Image, ModuleCode, ModuleEntry, ModuleRegistry, Vst3Module,
    ARCH_FOLDERS, BINARY_EXTENSION, MACHINES,
};

/// The start of an ELF binary: the identification and `e_machine`.
pub(crate) fn elf(is_64_bit: bool, machine: u16) -> Vec<u8> {
    let mut bytes = vec![0; 64];
    bytes[..4].copy_from_slice(b"\x7fELF");
    bytes[4] = if is_64_bit { 2 } else { 1 };
    bytes[5] = 1;
    bytes[6] = 1;
    bytes[18..20].copy_from_slice(&machine.to_le_bytes());
    bytes
}

/// The start of a PE binary: a DOS header pointing at the PE signature
/// and `Machine`.
pub(crate) fn pe(machine: u16) -> Vec<u8> {
    let mut bytes = vec![0; 0x80 + 24];
    bytes[..2].copy_from_slice(b"MZ");
    bytes[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());
    bytes[0x80..0x84].copy_from_slice(b"PE\0\0");
    bytes[0x84..0x86].copy_from_slice(&machine.to_le_bytes());
    bytes
}

/// A header this build loads; the rest of a real binary is not needed.
pub(crate) fn this_build_binary() -> Vec<u8> {
    let machine = MACHINES.first().copied().unwrap_or_default();
    if cfg!(target_os = "windows") {
        pe(machine)
    } else {
        elf(cfg!(target_pointer_width = "64"), machine)
    }
}

/// A header for a machine this build does not load.
fn foreign_binary() -> Vec<u8> {
    if cfg!(target_os = "windows") {
        pe(if MACHINES.contains(&0x014C) {
            0xAA64
        } else {
            0x014C
        })
    } else {
        let machine = if MACHINES.contains(&3) { 183 } else { 3 };
        elf(cfg!(target_pointer_width = "64"), machine)
    }
}

/// What a plugin would do if its `ModuleExit` (or a release during an
/// unload) loaded a module: the entry lock is already held further up.
#[test]
fn registry_reentry_during_module_code_panics_instead_of_deadlocking() {
    let registry = ModuleRegistry::new();
    let key = PathBuf::from("reentry.vst3");
    let code = ModuleCode::enter();
    let reentry = std::panic::catch_unwind(|| {
        registry
            .acquire_with(key.clone(), |_| Err("unused".to_owned()))
            .is_ok()
    });
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
    // Outside module code the registry works as usual: the load is tried,
    // and its failure is reported.
    let error = registry
        .acquire_with(key, |_| Err("the load failed".to_owned()))
        .err()
        .expect("the load is tried");
    assert_eq!(error, "the load failed");
}

/// A module that takes its time to load blocks only its own entry: an
/// acquire of another key finishes while the first `open` is still inside.
#[test]
fn a_slow_load_of_one_module_does_not_block_another() {
    let registry = ModuleRegistry::new();
    let (opening, opened) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    std::thread::scope(|scope| {
        let registry = &registry;
        scope.spawn(move || {
            let slow = registry.acquire_with("slow.vst3".into(), |_| {
                opening.send(()).expect("signal the open");
                released.recv().expect("the test releases the open");
                Err("refused".to_owned())
            });
            assert!(slow.is_err());
        });
        // The slow open is inside its entry lock from here on.
        opened
            .recv_timeout(Duration::from_secs(5))
            .expect("the slow open started");
        let (done, finished) = mpsc::channel();
        scope.spawn(move || {
            let other = registry.acquire_with("fast.vst3".into(), |_| Err("missing".to_owned()));
            done.send(other.err()).expect("report the other open");
        });
        let other = finished.recv_timeout(Duration::from_secs(5));
        // Let the blocked open go before asserting, so a regression fails
        // instead of leaving the thread behind.
        release.send(()).expect("release the slow open");
        let other = other.expect("a load of another module finished while one was blocked");
        assert_eq!(other.as_deref(), Some("missing"));
    });
}

/// A module over `entry` with no binary behind it: nothing to release and
/// nothing to unload, but the same entry and the same `Drop` path.
fn empty_module(entry: &Arc<ModuleEntry>) -> Vst3Module {
    Vst3Module {
        factory: None,
        exit: None,
        library: None,
        entry: Arc::clone(entry),
        #[cfg(target_os = "linux")]
        base: 0,
    }
}

/// A loaded module is handed out again until its last user lets go, and the
/// next acquire after that loads it anew.
#[test]
fn an_acquire_shares_a_loaded_module_and_loads_it_again_after_the_last_release() {
    static OPENS: AtomicUsize = AtomicUsize::new(0);
    let registry = ModuleRegistry::new();
    let key = PathBuf::from("shared.vst3");
    let open = |entry: &Arc<ModuleEntry>| {
        OPENS.fetch_add(1, Ordering::SeqCst);
        Ok(empty_module(entry))
    };
    let first = registry
        .acquire_with(key.clone(), open)
        .expect("first acquire");
    let second = registry
        .acquire_with(key.clone(), open)
        .expect("second acquire");
    assert!(Arc::ptr_eq(&first, &second), "one module per key");
    assert_eq!(OPENS.load(Ordering::SeqCst), 1);
    // One user letting go does not load the binary again.
    drop(first);
    let third = registry
        .acquire_with(key.clone(), open)
        .expect("third acquire");
    assert!(Arc::ptr_eq(&second, &third));
    assert_eq!(OPENS.load(Ordering::SeqCst), 1, "still held");
    drop((second, third));
    // The last release drops the module, so the next acquire loads again.
    let fourth = registry.acquire_with(key, open).expect("reacquire");
    assert_eq!(OPENS.load(Ordering::SeqCst), 2);
    drop(fourth);
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

    fn file(&self, relative: &str, contents: &[u8]) -> PathBuf {
        let path = self.0.join(relative);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("folders");
        std::fs::write(&path, contents).expect("file");
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
    let own = this_build_binary();
    // Another architecture's folder is never searched.
    root.file(
        &format!("Gain.vst3/Contents/other-arch/Gain.{BINARY_EXTENSION}"),
        &own,
    );
    let error = binary_path(&bundle).expect_err("no binary for this arch");
    assert!(error.contains(arch), "{error}");

    // Any binary in this arch's folder, the bundle-named one first.
    let other = root.file(
        &format!("Gain.vst3/Contents/{arch}/aaa.{BINARY_EXTENSION}"),
        &own,
    );
    assert_eq!(binary_path(&bundle), Ok(other.clone()));
    let named = root.file(
        &format!("Gain.vst3/Contents/{arch}/Gain.{BINARY_EXTENSION}"),
        &own,
    );
    assert_eq!(binary_path(&bundle), Ok(named.clone()));

    // A named binary for another machine is skipped for one that loads.
    root.file(
        &format!("Gain.vst3/Contents/{arch}/Gain.{BINARY_EXTENSION}"),
        &foreign_binary(),
    );
    assert_eq!(binary_path(&bundle), Ok(other.clone()));

    // With none left, the error says what the binary was.
    root.file(
        &format!("Gain.vst3/Contents/{arch}/aaa.{BINARY_EXTENSION}"),
        b"not a binary",
    );
    let error = binary_path(&bundle).expect_err("nothing this build loads");
    assert!(error.contains("not one this"), "{error}");
}

#[cfg(target_os = "windows")]
#[test]
fn a_single_file_plugin_loads_only_for_this_machine() {
    let root = TempBundle::new("single");
    let single = root.file("Single.vst3", &this_build_binary());
    assert_eq!(binary_path(&single), Ok(single.clone()));
    let foreign = root.file("Foreign.vst3", &foreign_binary());
    let error = binary_path(&foreign).expect_err("another machine");
    assert!(error.contains("a Windows binary for"), "{error}");
    let empty = root.file("Empty.vst3", b"");
    assert!(binary_path(&empty).is_err());
}

#[cfg(target_os = "linux")]
#[test]
fn linux_has_no_single_file_layout() {
    let root = TempBundle::new("single");
    let single = root.file("Single.vst3", &this_build_binary());
    let error = binary_path(&single).expect_err("a file is not a bundle");
    assert!(error.contains("not a bundle folder"), "{error}");
}

#[test]
fn the_header_names_the_format_and_the_machine() {
    let read = |bytes: Vec<u8>| read_image(&mut Cursor::new(bytes));
    assert_eq!(
        read(elf(true, 62)),
        Some(Image::Elf {
            is_64_bit: true,
            little_endian: true,
            machine: 62
        })
    );
    let mut big = elf(false, 0);
    big[5] = 2;
    big[18..20].copy_from_slice(&40u16.to_be_bytes());
    assert_eq!(
        read(big),
        Some(Image::Elf {
            is_64_bit: false,
            little_endian: false,
            machine: 40
        })
    );
    assert_eq!(read(pe(0xAA64)), Some(Image::Pe { machine: 0xAA64 }));

    // Not a binary, cut short, a bad identification, or a PE offset past
    // the end.
    assert_eq!(read(b"#!/bin/sh\n".repeat(8)), None);
    assert_eq!(read(elf(true, 62)[..10].to_vec()), None);
    let mut odd_class = elf(true, 62);
    odd_class[4] = 7;
    assert_eq!(read(odd_class), None);
    assert_eq!(read(pe(0x8664)[..0x82].to_vec()), None);
    let mut far = pe(0x8664);
    far[0x3C..0x40].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(read(far), None);
    let mut unsigned = pe(0x8664);
    unsigned[0x80] = b'X';
    assert_eq!(read(unsigned), None);
}

#[test]
fn only_this_builds_platform_and_machine_load() {
    let own = read_image(&mut Cursor::new(this_build_binary())).expect("a header");
    assert_eq!(own.is_loadable(), !MACHINES.is_empty());
    let foreign = read_image(&mut Cursor::new(foreign_binary())).expect("a header");
    assert!(!foreign.is_loadable());
    // The other platform's format never loads, whatever its machine.
    for machine in [3, 62, 183, 0x014C, 0x8664, 0xAA64] {
        let other = if cfg!(target_os = "windows") {
            elf(true, machine)
        } else {
            pe(machine)
        };
        let image = read_image(&mut Cursor::new(other)).expect("a header");
        assert!(!image.is_loadable(), "{image:?}");
    }
    // The other word size does not load either.
    let narrow = elf(
        !cfg!(target_pointer_width = "64"),
        MACHINES.first().copied().unwrap_or(62),
    );
    assert!(!read_image(&mut Cursor::new(narrow))
        .expect("a header")
        .is_loadable());
}
