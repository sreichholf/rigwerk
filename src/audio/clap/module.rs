//! Shared CLAP module loading and the process-wide registry.

use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};
use std::ptr;
#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
#[cfg(test)]
use std::time::Duration;

use clap_sys::entry::clap_plugin_entry;
use clap_sys::factory::plugin_factory::{clap_plugin_factory, CLAP_PLUGIN_FACTORY_ID};

/// One successful `clap_plugin_entry.init`.  Dropping it calls `deinit`
/// exactly once.  It must be dropped after every plugin created through the
/// entry is destroyed and before the library is unloaded.
///
/// Outside the tests only a [`ClapModule`] owns one, so the module registry
/// decides when `init` and `deinit` run.
pub(super) struct EntryInit {
    pub(super) entry: *const clap_plugin_entry,
}

impl EntryInit {
    /// Calls the entry's `init` (an entry without one counts as initialised).
    /// `None` when the module refused; nothing needs undoing then.
    ///
    /// # Safety
    /// `entry` must point to a valid CLAP entry whose library stays loaded
    /// until the returned guard is dropped.
    pub(super) unsafe fn init(entry: *const clap_plugin_entry, path: &CStr) -> Option<Self> {
        let initialised = match unsafe { (*entry).init } {
            Some(init) => unsafe { init(path.as_ptr()) },
            None => true,
        };
        // Not `then_some`: that would build (and drop, so `deinit`) the guard
        // even when `init` failed.
        if initialised {
            Some(Self { entry })
        } else {
            None
        }
    }

    pub(super) fn entry(&self) -> &clap_plugin_entry {
        unsafe { &*self.entry }
    }

    /// The entry's plugin factory, or null.
    pub(super) fn plugin_factory(&self) -> *const clap_plugin_factory {
        self.entry()
            .get_factory
            .map(|get| unsafe { get(CLAP_PLUGIN_FACTORY_ID.as_ptr()) })
            .unwrap_or(ptr::null())
            .cast::<clap_plugin_factory>()
    }
}

impl Drop for EntryInit {
    fn drop(&mut self) {
        if let Some(deinit) = self.entry().deinit {
            unsafe { deinit() };
        }
    }
}

/// A freshly opened CLAP library, before its entry is initialised.
pub(super) struct OpenedModule {
    pub(super) library: libloading::Library,
    pub(super) entry: *const clap_plugin_entry,
    /// The path handed to `entry.init`.
    pub(super) init_path: CString,
}

/// One loaded CLAP library with its one successful `entry.init`.  Every
/// instance and every scan of the library shares it through the
/// [`ModuleRegistry`], so `init` runs once per load of the library and
/// `deinit` only after the last user is gone.
pub(super) struct ClapModule {
    // `init` deinits the entry before the library is unloaded.
    pub(super) init: EntryInit,
    pub(super) _library: libloading::Library,
}

// SAFETY: the entry is immutable data inside the library, which the module
// keeps loaded.  `init` and `deinit` only run under the registry lock, and
// CLAP makes `get_factory` and the plugin factory callable from any thread.
unsafe impl Send for ClapModule {}
unsafe impl Sync for ClapModule {}

impl ClapModule {
    pub(super) fn plugin_factory(&self) -> *const clap_plugin_factory {
        self.init.plugin_factory()
    }
}

/// The process-wide table of loaded CLAP modules, keyed by the canonical path
/// of the binary.  CLAP 1.0/1.1 modules do not tolerate a second `init`, a
/// `deinit` under a live instance, or `init`/`deinit` racing each other, and
/// the OS shares one copy of a library between every `LoadLibrary`/`dlopen`
/// of it, so a per-instance or per-scan init is not safe.  The registry
/// therefore holds one library and one `init` per module, and its lock is
/// held for the whole of every `init` and every `deinit` (and the library
/// load and unload around them), so those never overlap.
///
/// One lock covers every module, so a slow library load or `init` of one
/// module delays loads and scans of others until it finishes; loads are
/// rare and happen off the audio thread, so this is accepted for the
/// simpler guarantee.
///
/// The lock is only ever taken on the UI, bridge and scan threads: a module is
/// released when the last [`ModuleHandle`] drops, and the audio callback never
/// drops the last reference to a plugin (see `PluginChain::process`).
pub(super) struct ModuleRegistry {
    pub(super) modules: Mutex<std::collections::BTreeMap<PathBuf, Arc<ClapModule>>>,
    /// Test hook: how long a [`ModuleHandle`] drop lingers after releasing
    /// the lock, to widen any window in which it still held a reference.
    #[cfg(test)]
    pub(super) release_pause_ms: AtomicU64,
}

/// The registry every load and scan goes through.
pub(super) static MODULES: ModuleRegistry = ModuleRegistry::new();

impl ModuleRegistry {
    pub(super) const fn new() -> Self {
        Self {
            modules: Mutex::new(std::collections::BTreeMap::new()),
            #[cfg(test)]
            release_pause_ms: AtomicU64::new(0),
        }
    }

    #[cfg(test)]
    pub(super) fn pause_after_release(&self) {
        let pause = self.release_pause_ms.load(Ordering::SeqCst);
        if pause > 0 {
            std::thread::sleep(Duration::from_millis(pause));
        }
    }

    pub(super) fn lock(
        &self,
    ) -> std::sync::MutexGuard<'_, std::collections::BTreeMap<PathBuf, Arc<ClapModule>>> {
        self.modules
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Hands out the module stored under `key`, opening and initialising it
    /// with `open` first when no one is using it.  A module whose `open`,
    /// version check or `init` fails is not stored, so the next acquire tries
    /// again from scratch.
    pub(super) fn acquire(
        &'static self,
        key: PathBuf,
        open: impl FnOnce() -> Result<OpenedModule, String>,
    ) -> Result<ModuleHandle, String> {
        let mut modules = self.lock();
        if let Some(module) = modules.get(&key) {
            let module = Arc::clone(module);
            return Ok(ModuleHandle {
                registry: self,
                key,
                module: Some(module),
            });
        }
        let opened = open()?;
        if !clap_sys::version::clap_version_is_compatible(unsafe { (*opened.entry).clap_version }) {
            return Err("This CLAP plugin uses an incompatible CLAP version.".to_owned());
        }
        // SAFETY: `opened.entry` comes from `opened.library`, which the module
        // keeps loaded until after the guard is dropped.
        let Some(init) = (unsafe { EntryInit::init(opened.entry, &opened.init_path) }) else {
            return Err("The CLAP plugin could not be initialised.".to_owned());
        };
        let module = Arc::new(ClapModule {
            init,
            _library: opened.library,
        });
        modules.insert(key.clone(), Arc::clone(&module));
        Ok(ModuleHandle {
            registry: self,
            key,
            module: Some(module),
        })
    }

    #[cfg(test)]
    pub(super) fn holds(&self, key: &Path) -> bool {
        self.lock().contains_key(key)
    }
}

/// One user's share of a [`ClapModule`].  Handles are never cloned; each one
/// comes from [`ModuleRegistry::acquire`].  Dropping the last one deinits the
/// module and unloads its library, under the registry lock.
pub(super) struct ModuleHandle {
    pub(super) registry: &'static ModuleRegistry,
    pub(super) key: PathBuf,
    /// Always `Some` until `drop`.
    pub(super) module: Option<Arc<ClapModule>>,
}

impl std::ops::Deref for ModuleHandle {
    type Target = ClapModule;

    fn deref(&self) -> &ClapModule {
        self.module.as_deref().expect("a live module handle")
    }
}

impl Drop for ModuleHandle {
    fn drop(&mut self) {
        let Some(module) = self.module.take() else {
            return;
        };
        let mut modules = self.registry.lock();
        // Every reference to a module is created (`acquire`) and released
        // (here) under the lock, so the count is stable while it is held:
        // two references (the table's and this one) mean this is the last
        // user.  This handle's reference is released before the lock is, on
        // every path, so a concurrent drop never counts it.
        let last = Arc::strong_count(&module) == 2;
        let stored = if last {
            let stored = modules.remove(&self.key);
            debug_assert!(stored
                .as_ref()
                .is_some_and(|stored| Arc::ptr_eq(stored, &module)));
            stored
        } else {
            None
        };
        drop(module);
        // On the last release `deinit` and the unload run here, still under
        // the lock.
        drop(stored);
        drop(modules);
        #[cfg(test)]
        self.registry.pause_after_release();
    }
}

/// The registry key for a CLAP binary: its canonical path, so different
/// spellings of one file share one module.  When the path cannot be
/// canonicalised (a filesystem that cannot report the final path), the
/// absolute path is used instead; the library load then decides whether the
/// file can be opened at all.
pub(super) fn module_key(binary: &Path) -> PathBuf {
    std::fs::canonicalize(binary)
        .or_else(|_| std::path::absolute(binary))
        .unwrap_or_else(|_| binary.to_path_buf())
}

/// Opens a CLAP library and finds its entry, without initialising it.
pub(super) fn open_module(bundle: &Path, binary: &Path) -> Result<OpenedModule, String> {
    let init_path = CString::new(
        entry_init_path(bundle, binary)
            .to_string_lossy()
            .into_owned(),
    )
    .map_err(|_| "The CLAP path contains a NUL byte.".to_owned())?;
    let library = unsafe { libloading::Library::new(binary) }
        .map_err(|error| format!("CLAP load failed: {error}"))?;
    let entry = {
        let symbol: libloading::Symbol<'_, *const clap_plugin_entry> = unsafe {
            library
                .get(b"clap_entry\0")
                .map_err(|error| format!("Missing clap_entry symbol: {error}"))?
        };
        *symbol
    };
    if entry.is_null() {
        return Err("The clap_entry symbol is null.".to_owned());
    }
    Ok(OpenedModule {
        library,
        entry,
        init_path,
    })
}

/// The shared module for a `.clap` bundle (or bare binary), loading and
/// initialising it if no one else is using it.
pub(super) fn acquire_module(bundle: &Path) -> Result<ModuleHandle, String> {
    let binary = bundle_binary_path(bundle);
    MODULES.acquire(module_key(&binary), || open_module(bundle, &binary))
}

pub(super) fn entry_init_path(bundle: &Path, binary: &Path) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        let _ = binary;
        bundle.to_path_buf()
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = bundle;
        binary.to_path_buf()
    }
}

pub(super) fn bundle_binary_path(bundle: &Path) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        if bundle.is_dir() {
            let stem = bundle.file_stem().unwrap_or_default();
            return bundle.join("Contents/MacOS").join(stem);
        }
    }
    bundle.to_path_buf()
}
