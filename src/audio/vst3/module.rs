//! Loading a VST3 bundle, once per process.
//!
//! The SDK counts `ModuleEntry`/`ModuleExit` pairs, but a factory keeps
//! module globals (the host context, VSTGUI's run loop) that a second load
//! would fight over, so every user of a binary shares one [`Vst3Module`]
//! through the registry.  The last `Arc` to drop purges the module's
//! run-loop entries, releases the factory, calls the exit function and only
//! then unloads the library, in that order.  Loads and drops happen on the
//! UI thread; the registry lock is held across both, so a load never
//! overlaps an unload of the same binary.
//!
//! That lock is held while plugin code runs (`ModuleEntry`,
//! `setHostContext`, the releases, `ModuleExit`, the unload), so two rules
//! keep it from deadlocking.  No COM object handed to a plugin owns an
//! `Arc<Vst3Module>`, so a plugin's release can never drop the last one.
//! The last `Arc` drops on the UI thread, never on the audio thread.  A
//! re-entry into the registry from inside that plugin code (on the same
//! thread) panics with a message instead of deadlocking; from inside a
//! plugin callback that becomes an abort.
//!
//! The SDK factory is one per binary, not per host: `setHostContext`
//! replaces the context (and VSTGUI's run loop) for every user of the
//! binary.  Nothing else may load a VST3 binary: the library scan
//! (`scan`) only reads the bundle layout and `moduleinfo.json`.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use ::vst3::ComPtr;
use ::vst3::Steinberg::IPluginFactory;

use super::factory;

/// The answer on a platform without a VST3 host.
const UNSUPPORTED: &str = "VST3 plugins are not supported on this platform.";

/// `bool ModuleExit (void)` on Linux, `bool ExitDll ()` on Windows.  Both
/// are plain C exports (`linuxmain.cpp`, `dllmain.cpp`).
type ExitFn = unsafe extern "C" fn() -> bool;

/// `IPluginFactory* PLUGIN_API GetPluginFactory ()`: `__stdcall` on
/// Windows, which `system` is.
type GetFactoryFn = unsafe extern "system" fn() -> *mut IPluginFactory;

/// One loaded VST3 binary with its one successful entry call.  Get it with
/// [`acquire`]; every instance created from its factory keeps an `Arc`.
pub(crate) struct Vst3Module {
    /// Always `Some` until `drop`, which releases it before `exit`.
    factory: Option<ComPtr<IPluginFactory>>,
    /// Linux `ModuleExit` (required), Windows `ExitDll` (optional).
    exit: Option<ExitFn>,
    /// Always `Some` until `drop`, which unloads it last.
    library: Option<libloading::Library>,
    /// The registry key: the canonical binary path.
    key: PathBuf,
    /// Where the binary is mapped (`dladdr`), to find its run-loop entries.
    #[cfg(target_os = "linux")]
    base: usize,
}

impl Vst3Module {
    pub(crate) fn factory(&self) -> &ComPtr<IPluginFactory> {
        self.factory.as_ref().expect("a live module")
    }

    #[cfg(test)]
    pub(crate) fn key(&self) -> &Path {
        &self.key
    }
}

thread_local! {
    /// True while this thread runs module code under the registry lock.
    static IN_MODULE_CODE: Cell<bool> = const { Cell::new(false) };
}

/// Marks this thread as running module code under the registry lock, for
/// as long as it lives.
pub(super) struct ModuleCode(());

impl ModuleCode {
    /// Panics when the thread is already inside module code: the registry
    /// lock is held further up the stack, and taking it would deadlock.
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
        let mut modules = registry();
        // A load that found this module dying has already replaced it.
        if modules
            .get(&self.key)
            .is_some_and(|weak| std::ptr::eq(weak.as_ptr(), self))
        {
            modules.remove(&self.key);
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
        drop(modules);
    }
}

/// Loaded modules by canonical binary path.  An entry is removed by the
/// module's own `drop`.
static MODULES: Mutex<BTreeMap<PathBuf, Weak<Vst3Module>>> = Mutex::new(BTreeMap::new());

fn registry() -> MutexGuard<'static, BTreeMap<PathBuf, Weak<Vst3Module>>> {
    MODULES.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether the binary under `key` is loaded through the registry.
#[cfg(test)]
pub(crate) fn is_loaded(key: &Path) -> bool {
    registry()
        .get(key)
        .is_some_and(|weak| weak.strong_count() > 0)
}

/// The shared module for a `.vst3` bundle (or, on Windows, a legacy
/// single-file plugin), loading it and handing its factory the host context when no
/// one is using it yet.
pub(crate) fn acquire(bundle: &Path) -> Result<Arc<Vst3Module>, String> {
    let binary = binary_path(bundle)?;
    let key = std::fs::canonicalize(&binary)
        .or_else(|_| std::path::absolute(&binary))
        .unwrap_or_else(|_| binary.clone());
    let _code = ModuleCode::enter();
    let mut modules = registry();
    if let Some(module) = modules.get(&key).and_then(Weak::upgrade) {
        return Ok(module);
    }
    // Nothing after `open` fails, so a new module is never dropped (and
    // never takes the registry lock again) in here.
    let module = Arc::new(open(&binary, key.clone())?);
    // Once per load and before any `createInstance`, as the SDK's VSTGUI
    // takes its run loop from it (`vstguieditor.cpp`).
    factory::set_host_context(module.factory());
    modules.insert(key, Arc::downgrade(&module));
    Ok(module)
}

/// The `Contents` folders holding a binary this build can load, its own
/// architecture's first (SDK `module_linux.cpp`, `module_win32.cpp`).
/// Empty on a platform without a VST3 host.
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

/// The binary's extension inside a bundle.
pub(super) const BINARY_EXTENSION: &str = if cfg!(target_os = "windows") {
    "vst3"
} else {
    "so"
};

/// The machine ids a binary this build can load carries: ELF `e_machine`
/// on Linux, the PE `Machine` field on Windows.  An ARM64EC build also
/// loads x64 and ARM64X images, which are `0x8664` and `0xAA64`.  An
/// ARM64EC image says `0x8664` too, so an x64 build cannot tell one from
/// x64 by the header; the load refuses it.
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

/// What a binary's header says it is.  Nothing else of the file is read.
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
    /// Whether this build can load it: the platform's format, this
    /// build's machine and, for ELF, its word size and byte order.
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

/// The header of an ELF or PE binary: the ELF identification and
/// `e_machine`, or the DOS header's `e_lfanew` and the PE signature and
/// `Machine` it points at.  `None` for anything else.
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

/// `Ok` when the file at `path` is a binary this build can load, else why
/// not.  Reads the header only; the plugin is not loaded.
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

/// The binary inside a bundle directory: `Contents/<arch>/<name>.<ext>`,
/// or else the first `.<ext>` file there, whose header (`read_image`)
/// says this build can load it.  A binary for another machine is skipped,
/// so a broken install is refused here, not by the loader.  On Windows a
/// plain file is the legacy single-file layout and is checked the same way;
/// Linux has bundle folders only.
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

/// Opens a Linux module: `dlopen`, `ModuleEntry` with that handle, then
/// `GetPluginFactory`.  All three exports and `ModuleExit` are required.
#[cfg(target_os = "linux")]
fn open(binary: &Path, key: PathBuf) -> Result<Vst3Module, String> {
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
    let (entry, exit, get_factory) = unsafe {
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
    if !unsafe { entry(handle) } {
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
        key,
        base: super::run_loop::module_base(get_factory as *const std::ffi::c_void),
    })
}

/// Opens a Windows module: `LoadLibrary`, the optional `InitDll`, then
/// `GetPluginFactory`.  `ExitDll` is optional too.
#[cfg(target_os = "windows")]
fn open(binary: &Path, key: PathBuf) -> Result<Vst3Module, String> {
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
        key,
    })
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn open(_binary: &Path, _key: PathBuf) -> Result<Vst3Module, String> {
    Err(UNSUPPORTED.to_owned())
}

#[cfg(test)]
pub(super) mod tests;
