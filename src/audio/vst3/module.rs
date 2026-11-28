//! One [`Vst3Module`] per binary.  The SDK counts `ModuleEntry` pairs,
//! but a factory keeps module globals a second load would fight over.
//! The last `Arc` purges run-loop entries, releases the factory, calls
//! exit, then unloads, in that order.  Loads and drops are UI-thread.
//!
//! The map lock only covers lookup and insert.  Each path's entry lock
//! covers that binary's load and its release, exit, and unload, so one
//! slow load does not block another and a load never overlaps an unload.
//! Entries are never removed, so a waiter stays in the entry a loader or
//! a last release is using.
//!
//! `Weak::upgrade` is `None` once the last `Arc` hits zero, before `Drop`
//! takes the entry lock.  An acquire in between issues a new `ModuleEntry`
//! before the old `ModuleExit`.  The counted 3.8 SDK accepts that; the
//! 3.6 single-handle form refuses it.  The mapping is the same object
//! with another reference, so the old unload drops one.  UI-thread loads
//! make this a parallel-test window (`docs/open-items.md`).
//!
//! Module code runs under the entry lock.  No COM object handed to a
//! plugin owns an `Arc<Vst3Module>`, so a plugin release cannot drop the
//! last one, and the last drop is never on the audio thread.  Re-entry
//! from that plugin code panics instead of deadlocking.
//!
//! `setHostContext` replaces the context for every user of the binary.
//! The scan reads the bundle and `moduleinfo.json` and does not load it.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use ::vst3::ComPtr;
use ::vst3::Steinberg::IPluginFactory;

use super::factory;

/// The answer on a platform without a VST3 host.
const UNSUPPORTED: &str = "VST3 plugins are not supported on this platform.";

/// `ModuleExit` on Linux, `ExitDll` on Windows.  Both are plain C
/// (`linuxmain.cpp`, `dllmain.cpp`).
type ExitFn = unsafe extern "C" fn() -> bool;

/// `__stdcall` on Windows, which `extern "system"` is.
type GetFactoryFn = unsafe extern "system" fn() -> *mut IPluginFactory;

/// One successful entry call.  Every instance from its factory holds an
/// `Arc`.
pub(crate) struct Vst3Module {
    /// Always `Some` until `drop`, which releases it before `exit`.
    factory: Option<ComPtr<IPluginFactory>>,
    /// Linux `ModuleExit` (required), Windows `ExitDll` (optional).
    exit: Option<ExitFn>,
    /// Always `Some` until `drop`, which unloads it last.
    library: Option<libloading::Library>,
    /// `drop` holds this entry's lock while the module is released.  The
    /// entry only keeps a weak reference, so there is no cycle.
    entry: Arc<ModuleEntry>,
    /// Load address (`dladdr`), to find this binary's run-loop entries.
    #[cfg(target_os = "linux")]
    base: usize,
}

impl Vst3Module {
    pub(crate) fn factory(&self) -> &ComPtr<IPluginFactory> {
        self.factory.as_ref().expect("a live module")
    }
}

thread_local! {
    /// True while this thread runs module code under an entry lock.
    static IN_MODULE_CODE: Cell<bool> = const { Cell::new(false) };
}

/// Set while this thread runs module code under an entry lock.
pub(super) struct ModuleCode(());

impl ModuleCode {
    /// Panics when the thread is already inside module code: the entry lock
    /// is held further up the stack, and taking it would deadlock.
    pub(super) fn enter() -> Self {
        assert!(
            !IN_MODULE_CODE.replace(true),
            "a VST3 module load or unload re-entered the module registry \
             (a plugin call loaded or dropped a module while one was loading or unloading)"
        );
        Self(())
    }
}

impl Drop for ModuleCode {
    fn drop(&mut self) {
        IN_MODULE_CODE.set(false);
    }
}

impl Drop for Vst3Module {
    fn drop(&mut self) {
        let _code = ModuleCode::enter();
        // Held while the module is released, so no acquire can load the
        // binary again until the unload below is done.
        let mut state = self.entry.lock();
        // A load that found this module dying has already replaced the weak
        // with a newer module's; only our own weak is cleared here.
        if std::ptr::eq(state.as_ptr(), self) {
            *state = Weak::new();
        }
        // Run-loop handlers live in the module's code: release them while
        // it is still mapped.
        #[cfg(target_os = "linux")]
        super::run_loop::forget_module(self.base);
        drop(self.factory.take());
        if let Some(exit) = self.exit {
            // SAFETY: the exit function belongs to the still-loaded library,
            // the factory is released and every instance is gone (each held
            // an `Arc` to this module), so this is the matching exit call.
            unsafe { exit() };
        }
        drop(self.library.take());
        drop(state);
    }
}

pub(super) struct ModuleRegistry {
    modules: Mutex<BTreeMap<PathBuf, Arc<ModuleEntry>>>,
}

/// A weak reference to the loaded module.  Upgrade means loaded.  Never
/// removed, so an acquire waits here instead of loading during an unload.
struct ModuleEntry {
    state: Mutex<Weak<Vst3Module>>,
}

static MODULES: ModuleRegistry = ModuleRegistry::new();

impl ModuleEntry {
    fn lock(&self) -> MutexGuard<'_, Weak<Vst3Module>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl ModuleRegistry {
    const fn new() -> Self {
        Self {
            modules: Mutex::new(BTreeMap::new()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<PathBuf, Arc<ModuleEntry>>> {
        self.modules.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The map's lock is released before the entry's is taken.
    fn entry(&self, key: &Path) -> Arc<ModuleEntry> {
        let mut modules = self.lock();
        Arc::clone(modules.entry(key.to_path_buf()).or_insert_with(|| {
            Arc::new(ModuleEntry {
                state: Mutex::new(Weak::new()),
            })
        }))
    }

    /// A failed `open` leaves the empty entry, so the next acquire retries
    /// in place.
    fn acquire_with(
        &self,
        key: PathBuf,
        open: impl FnOnce(&Arc<ModuleEntry>) -> Result<Vst3Module, String>,
    ) -> Result<Arc<Vst3Module>, String> {
        let _code = ModuleCode::enter();
        let entry = self.entry(&key);
        let mut state = entry.lock();
        if let Some(module) = state.upgrade() {
            return Ok(module);
        }
        // Nothing after `open` fails, so no module is dropped (and drops
        // back into this entry) here.
        let module = Arc::new(open(&entry)?);
        // Once per load, before any `createInstance`: VSTGUI takes its run
        // loop from the host context (`vstguieditor.cpp`).  A test module
        // has no factory to hand it to.
        if let Some(factory) = module.factory.as_ref() {
            factory::set_host_context(factory);
        }
        *state = Arc::downgrade(&module);
        Ok(module)
    }
}

#[cfg(test)]
pub(crate) fn is_loaded(key: &Path) -> bool {
    let entry = MODULES.lock().get(key).map(Arc::clone);
    entry.is_some_and(|entry| entry.lock().strong_count() > 0)
}

/// Canonical path, or absolute when the filesystem cannot report one.
fn key_of(binary: &Path) -> PathBuf {
    std::fs::canonicalize(binary)
        .or_else(|_| std::path::absolute(binary))
        .unwrap_or_else(|_| binary.to_path_buf())
}

/// The same key `key_of` gives a real load of this bundle's binary.
#[cfg(test)]
pub(super) fn module_key(bundle: &Path) -> Result<PathBuf, String> {
    binary_path(bundle).map(|binary| key_of(&binary))
}

/// On Windows a plain file is the legacy single-file layout.  The
/// factory gets the host context when this is the first user.
pub(crate) fn acquire(bundle: &Path) -> Result<Arc<Vst3Module>, String> {
    let binary = binary_path(bundle)?;
    let key = key_of(&binary);
    MODULES.acquire_with(key, |entry| open(&binary, entry))
}

/// `Contents` folders this build can load, its own architecture first
/// (SDK `module_linux.cpp`, `module_win32.cpp`).  Empty where there is
/// no VST3 host.
#[cfg(target_os = "linux")]
pub(super) const ARCH_FOLDERS: &[&str] = match std::env::consts::ARCH.as_bytes() {
    b"x86_64" => &["x86_64-linux"],
    b"aarch64" => &["aarch64-linux"],
    b"x86" => &["i386-linux"],
    _ => &[],
};
#[cfg(target_os = "windows")]
pub(super) const ARCH_FOLDERS: &[&str] = match std::env::consts::ARCH.as_bytes() {
    b"x86_64" => &["x86_64-win"],
    b"aarch64" => &["arm64-win", "arm64x-win"],
    b"arm64ec" => &["arm64ec-win", "arm64x-win", "x86_64-win"],
    b"x86" => &["x86-win"],
    _ => &[],
};
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub(super) const ARCH_FOLDERS: &[&str] = &[];

pub(super) const BINARY_EXTENSION: &str = if cfg!(target_os = "windows") {
    "vst3"
} else {
    "so"
};

/// ELF `e_machine` or the PE `Machine` field.  An ARM64EC image says
/// `0x8664` too, so an x64 build cannot tell it from x64 by the header
/// and the load refuses it.  An ARM64EC build also accepts x64 and
/// ARM64X (`0x8664`, `0xAA64`).
#[cfg(target_os = "linux")]
pub(super) const MACHINES: &[u16] = match std::env::consts::ARCH.as_bytes() {
    b"x86_64" => &[62],
    b"aarch64" => &[183],
    b"x86" => &[3],
    _ => &[],
};
#[cfg(target_os = "windows")]
pub(super) const MACHINES: &[u16] = match std::env::consts::ARCH.as_bytes() {
    b"x86_64" => &[0x8664],
    b"aarch64" => &[0xAA64],
    b"arm64ec" => &[0x8664, 0xAA64],
    b"x86" => &[0x014C],
    _ => &[],
};
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub(super) const MACHINES: &[u16] = &[];

/// What the header says.  Nothing else of the file is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Image {
    Elf {
        is_64_bit: bool,
        little_endian: bool,
        machine: u16,
    },
    Pe {
        machine: u16,
    },
}

impl Image {
    fn is_loadable(self) -> bool {
        match self {
            Image::Elf {
                is_64_bit,
                little_endian,
                machine,
            } => {
                cfg!(target_os = "linux")
                    && is_64_bit == cfg!(target_pointer_width = "64")
                    && little_endian == cfg!(target_endian = "little")
                    && MACHINES.contains(&machine)
            }
            Image::Pe { machine } => cfg!(target_os = "windows") && MACHINES.contains(&machine),
        }
    }

    fn describe(self) -> String {
        match self {
            Image::Elf {
                is_64_bit, machine, ..
            } => {
                let name = match machine {
                    3 => "x86".to_owned(),
                    40 => "ARM".to_owned(),
                    62 => "x86_64".to_owned(),
                    183 => "aarch64".to_owned(),
                    other => format!("machine {other}"),
                };
                format!(
                    "a {}-bit Linux binary for {name}",
                    if is_64_bit { 64 } else { 32 }
                )
            }
            Image::Pe { machine } => {
                let name = match machine {
                    0x014C => "x86".to_owned(),
                    0x01C4 => "ARM".to_owned(),
                    0x8664 => "x64".to_owned(),
                    0xAA64 => "ARM64".to_owned(),
                    other => format!("machine {other:#06x}"),
                };
                format!("a Windows binary for {name}")
            }
        }
    }
}

pub(super) fn read_image(file: &mut (impl std::io::Read + std::io::Seek)) -> Option<Image> {
    use std::io::SeekFrom;
    let mut header = [0u8; 64];
    file.read_exact(&mut header[..20]).ok()?;
    if header[..4] == *b"\x7fELF" {
        let little_endian = match header[5] {
            1 => true,
            2 => false,
            _ => return None,
        };
        let is_64_bit = match header[4] {
            1 => false,
            2 => true,
            _ => return None,
        };
        let bytes = [header[18], header[19]];
        let machine = if little_endian {
            u16::from_le_bytes(bytes)
        } else {
            u16::from_be_bytes(bytes)
        };
        return Some(Image::Elf {
            is_64_bit,
            little_endian,
            machine,
        });
    }
    if header[..2] != *b"MZ" {
        return None;
    }
    file.read_exact(&mut header[20..]).ok()?;
    let offset = u32::from_le_bytes(header[0x3C..0x40].try_into().ok()?);
    file.seek(SeekFrom::Start(u64::from(offset))).ok()?;
    let mut signature = [0u8; 6];
    file.read_exact(&mut signature).ok()?;
    if signature[..4] != *b"PE\0\0" {
        return None;
    }
    Some(Image::Pe {
        machine: u16::from_le_bytes([signature[4], signature[5]]),
    })
}

/// Header only.  The plugin is not loaded.
fn check_image(path: &Path) -> Result<(), String> {
    let mut file = std::fs::File::open(path)
        .map_err(|error| format!("Cannot read {}: {error}.", path.display()))?;
    match read_image(&mut file) {
        Some(image) if image.is_loadable() => Ok(()),
        Some(image) => Err(format!(
            "{} is {}, not one this {} build can load.",
            path.display(),
            image.describe(),
            std::env::consts::ARCH
        )),
        None => Err(format!("{} is not a plugin binary.", path.display())),
    }
}

/// `Contents/<arch>/<name>.<ext>`, else the first loadable `.<ext>`
/// there.  Another machine is refused here, not by the loader.  On
/// Windows a plain file is the legacy single-file layout; Linux has
/// bundle folders only.
pub(super) fn binary_path(bundle: &Path) -> Result<PathBuf, String> {
    if ARCH_FOLDERS.is_empty() {
        return Err(UNSUPPORTED.to_owned());
    }
    if !bundle.is_dir() {
        if !cfg!(target_os = "windows") {
            return Err(format!("{} is not a bundle folder.", bundle.display()));
        }
        check_image(bundle)?;
        return Ok(bundle.to_path_buf());
    }
    let mut name = bundle.file_stem().unwrap_or_default().to_os_string();
    name.push(".");
    name.push(BINARY_EXTENSION);
    let contents = bundle.join("Contents");
    // The first refusal, to say why a bundle with binaries has none to load.
    let mut refused = None;
    let mut loadable = |path: &Path| match check_image(path) {
        Ok(()) => true,
        Err(error) => {
            refused.get_or_insert(error);
            false
        }
    };
    let found = ARCH_FOLDERS.iter().find_map(|folder| {
        let folder = contents.join(folder);
        let named = folder.join(&name);
        if named.is_file() && loadable(&named) {
            return Some(named);
        }
        let mut others: Vec<PathBuf> = std::fs::read_dir(&folder)
            .ok()?
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                *path != named
                    && path.is_file()
                    && path.extension() == Some(std::ffi::OsStr::new(BINARY_EXTENSION))
            })
            .collect();
        others.sort();
        others.into_iter().find(|path| loadable(path))
    });
    found.ok_or_else(|| {
        let mut message = format!(
            "{} has no binary this {} build can load (searched Contents/{}).",
            bundle.display(),
            std::env::consts::ARCH,
            ARCH_FOLDERS.join(", Contents/")
        );
        if let Some(reason) = refused {
            message.push(' ');
            message.push_str(&reason);
        }
        message
    })
}

/// `dlopen`, then `ModuleEntry` with that handle, then
/// `GetPluginFactory`.  All three exports and `ModuleExit` are required.
#[cfg(target_os = "linux")]
fn open(binary: &Path, entry: &Arc<ModuleEntry>) -> Result<Vst3Module, String> {
    use libloading::os::unix::Library;

    type EntryFn = unsafe extern "C" fn(*mut std::ffi::c_void) -> bool;

    // SAFETY: loading runs the library's initialisers; the user chose this
    // plugin, as with any host.
    let library = unsafe { Library::new(binary) }
        .map_err(|error| format!("The VST3 binary could not be loaded: {error}"))?;
    // `ModuleEntry` takes the `dlopen` handle, which libloading only hands
    // out by giving up the library; take it back at once.
    let handle = library.into_raw();
    // SAFETY: `handle` is the live handle `into_raw` just released.
    let library = unsafe { Library::from_raw(handle) };
    let missing =
        |what: &str| format!("The VST3 binary does not export the required '{what}' function.");
    // SAFETY: the signatures are the SDK's (`linuxmain.cpp`,
    // `ipluginbase.h`), and the copied pointers are only called while the
    // module keeps `library` loaded.
    let (entry_fn, exit, get_factory) = unsafe {
        (
            *library
                .get::<EntryFn>(b"ModuleEntry\0")
                .map_err(|_| missing("ModuleEntry"))?,
            *library
                .get::<ExitFn>(b"ModuleExit\0")
                .map_err(|_| missing("ModuleExit"))?,
            *library
                .get::<GetFactoryFn>(b"GetPluginFactory\0")
                .map_err(|_| missing("GetPluginFactory"))?,
        )
    };
    // SAFETY: called once per load, before `GetPluginFactory`, with the
    // library's own handle.
    if !unsafe { entry_fn(handle) } {
        // SAFETY: a failed entry is still paired with the exit, as the
        // SDK's `~LinuxModule` does; the SDK's own count stays balanced.
        unsafe { exit() };
        return Err("The VST3 module refused to start (ModuleEntry failed).".to_owned());
    }
    // SAFETY: after `ModuleEntry`; the factory comes back with a reference
    // the `ComPtr` takes over (`module_linux.cpp` wraps it in `owned`).
    let Some(factory) = (unsafe { ComPtr::from_raw(get_factory()) }) else {
        // SAFETY: the `ModuleExit` that matches the `ModuleEntry` above.
        unsafe { exit() };
        return Err("The VST3 module has no plugin factory.".to_owned());
    };
    Ok(Vst3Module {
        factory: Some(factory),
        exit: Some(exit),
        library: Some(library.into()),
        entry: Arc::clone(entry),
        base: super::run_loop::module_base(get_factory as *const std::ffi::c_void),
    })
}

/// `LoadLibrary`, then optional `InitDll`, then `GetPluginFactory`.
/// `ExitDll` is optional too.
#[cfg(target_os = "windows")]
fn open(binary: &Path, entry: &Arc<ModuleEntry>) -> Result<Vst3Module, String> {
    type InitFn = unsafe extern "C" fn() -> bool;

    // SAFETY: loading runs the library's initialisers; the user chose this
    // plugin, as with any host.
    let library = unsafe { libloading::Library::new(binary) }
        .map_err(|error| format!("The VST3 binary could not be loaded: {error}"))?;
    // SAFETY: the signatures are the SDK's (`dllmain.cpp`, `ipluginbase.h`),
    // and the copied pointers are only called while the module keeps
    // `library` loaded.
    let (init, exit, get_factory) = unsafe {
        (
            library.get::<InitFn>(b"InitDll\0").ok().map(|init| *init),
            library.get::<ExitFn>(b"ExitDll\0").ok().map(|exit| *exit),
            library
                .get::<GetFactoryFn>(b"GetPluginFactory\0")
                .map(|get| *get)
                .map_err(|_| {
                    "The VST3 binary does not export the required 'GetPluginFactory' function."
                        .to_owned()
                })?,
        )
    };
    if let Some(init) = init {
        // SAFETY: called once per load, before `GetPluginFactory`.
        if !unsafe { init() } {
            if let Some(exit) = exit {
                // SAFETY: a failed init is still paired with the exit, as
                // the SDK's `~Win32Module` does.
                unsafe { exit() };
            }
            return Err("The VST3 module refused to start (InitDll failed).".to_owned());
        }
    }
    // SAFETY: after `InitDll`; the factory comes back with a reference the
    // `ComPtr` takes over.
    let Some(factory) = (unsafe { ComPtr::from_raw(get_factory()) }) else {
        if let Some(exit) = exit {
            // SAFETY: the `ExitDll` that matches the `InitDll` above.
            unsafe { exit() };
        }
        return Err("The VST3 module has no plugin factory.".to_owned());
    };
    Ok(Vst3Module {
        factory: Some(factory),
        exit,
        library: Some(library),
        entry: Arc::clone(entry),
    })
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn open(_binary: &Path, _entry: &Arc<ModuleEntry>) -> Result<Vst3Module, String> {
    Err(UNSUPPORTED.to_owned())
}

#[cfg(test)]
pub(super) mod tests;
