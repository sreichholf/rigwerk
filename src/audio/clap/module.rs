//! Shared CLAP module loading and the process-wide registry.
//!
//! CLAP 1.0/1.1 modules do not tolerate a second `init`, a `deinit` under a
//! live instance, or `init`/`deinit` racing each other, and the OS shares one
//! copy of a library between every `LoadLibrary`/`dlopen` of it.  Every
//! instance and every scan of a binary therefore shares one [`ClapModule`]
//! through the [`ModuleRegistry`], keyed by the binary's canonical path.
//!
//! The map's lock only guards the lookup and the insert.  Each path has its
//! own entry, and that entry's lock is held across its module's `open`/`init`
//! and its `deinit`/unload, so those never overlap and a slow load of one
//! binary delays only loads of the same binary.  Entries are never removed:
//! an entry outlives the module it opened, so a waiter always waits in the
//! entry the loader is using.
//!
//! Neither lock is taken on the audio thread: a module is released when the
//! last [`ModuleHandle`] drops, and the chain runtime hands every slot it
//! lets go of back to the UI (see `plugins::chain`).

use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

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
// keeps loaded.  `init` and `deinit` only run under the module's entry lock,
// and CLAP makes `get_factory` and the plugin factory callable from any
// thread.
unsafe impl Send for ClapModule {}
unsafe impl Sync for ClapModule {}

impl ClapModule {
    pub(super) fn plugin_factory(&self) -> *const clap_plugin_factory {
        self.init.plugin_factory()
    }
}

/// The process-wide table of module entries, keyed by the canonical path of
/// the binary.  The map's lock is only held for the lookup and the insert;
/// everything that runs plugin code happens under one entry's own lock, so a
/// slow `dlopen`, `init`, `deinit` or unload of one module never blocks a
/// load or a scan of another.
///
/// An entry is never removed.  Removing it when its module was released
/// would let a third acquire insert a fresh entry while a waiter is still
/// opening in the old one, which is the double-`init` this registry exists
/// to prevent.  One empty `PathBuf` and one mutex per binary ever loaded is
/// the accepted cost.
pub(super) struct ModuleRegistry {
    modules: Mutex<BTreeMap<PathBuf, Arc<ModuleEntry>>>,
}

/// One canonical binary path's share of the registry: its loaded module and
/// how many handles hold it.  The lock is held across that module's `open`
/// and `init`, and across its `deinit` and unload, so those never overlap.
struct ModuleEntry {
    state: Mutex<EntryState>,
}

/// What one [`ModuleEntry`] holds.  `module` stays `None` after a failed
/// `open` or `init`, so the empty entry is reused and the next acquire
/// retries in place.
struct EntryState {
    module: Option<Arc<ClapModule>>,
    /// The live [`ModuleHandle`]s for `module`.
    users: usize,
}

/// The registry every load and scan goes through.
pub(super) static MODULES: ModuleRegistry = ModuleRegistry::new();

impl ModuleEntry {
    fn lock(&self) -> MutexGuard<'_, EntryState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl ModuleRegistry {
    pub(super) const fn new() -> Self {
        Self {
            modules: Mutex::new(BTreeMap::new()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<PathBuf, Arc<ModuleEntry>>> {
        self.modules.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The entry for `key`, inserting an empty one when the table has none.
    /// The map's lock is released before the entry's is taken.
    fn entry(&self, key: &Path) -> Arc<ModuleEntry> {
        let mut modules = self.lock();
        Arc::clone(modules.entry(key.to_path_buf()).or_insert_with(|| {
            Arc::new(ModuleEntry {
                state: Mutex::new(EntryState {
                    module: None,
                    users: 0,
                }),
            })
        }))
    }

    /// Hands out the module stored under `key`, opening and initialising it
    /// with `open` first when it is not loaded.  A module whose `open`,
    /// version check or `init` fails is not stored, so the next acquire tries
    /// again in place.
    pub(super) fn acquire(
        &self,
        key: PathBuf,
        open: impl FnOnce() -> Result<OpenedModule, String>,
    ) -> Result<ModuleHandle, String> {
        let entry = self.entry(&key);
        let mut state = entry.lock();
        if let Some(module) = state.module.as_ref().map(Arc::clone) {
            state.users += 1;
            return Ok(ModuleHandle {
                entry: Arc::clone(&entry),
                module: Some(module),
            });
        }
        // A failed return leaves the empty entry behind for the next
        // acquire; `opened` drops here, so the library unloads, still under
        // the entry lock.
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
        state.module = Some(Arc::clone(&module));
        state.users = 1;
        Ok(ModuleHandle {
            entry: Arc::clone(&entry),
            module: Some(module),
        })
    }

    /// Whether `key` has a loaded module.  An entry left behind by a failed
    /// `open` or a release does not count.
    #[cfg(test)]
    pub(super) fn holds(&self, key: &Path) -> bool {
        let entry = self.lock().get(key).map(Arc::clone);
        entry.is_some_and(|entry| entry.lock().module.is_some())
    }
}

/// One user's share of a [`ClapModule`].  Handles are never cloned; each one
/// comes from [`ModuleRegistry::acquire`].  Dropping the last one deinits the
/// module and unloads its library, under that module's entry lock.
pub(super) struct ModuleHandle {
    entry: Arc<ModuleEntry>,
    /// Always `Some` until `drop`.
    module: Option<Arc<ClapModule>>,
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
        let mut state = self.entry.lock();
        state.users -= 1;
        if state.users == 0 {
            // This handle and the entry's `Option` are the only references
            // left, so this handle holds the last one.  The entry's share goes
            // first and the drop below then runs `deinit` and the unload here,
            // under the entry lock the next acquire of this module needs.
            debug_assert_eq!(Arc::strong_count(&module), 2);
            state.module = None;
        }
        drop(module);
        drop(state);
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
