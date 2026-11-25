//! Shared LV2 module loading and the process-wide registry.
//!
//! A bundle names one binary per platform, and that binary may hold several
//! plugin descriptors.  The OS shares one copy of a library between every
//! `dlopen`/`LoadLibrary` of it, and a binary expects its global state to be
//! set up once, so every instance of one binary shares a single [`Lv2Module`]
//! through the [`ModuleRegistry`], keyed by the canonical path.  Nothing else
//! opens a binary: the scan (`scan`) reads the Turtle only.
//!
//! A library exports `lv2_lib_descriptor`, `lv2_descriptor` or both.  As in
//! lilv, `lv2_lib_descriptor(bundle_path, features)` is tried first and its
//! failure is a load error; `lv2_descriptor` is the fallback.  The library
//! descriptor gets an empty feature array, since the host's features belong
//! to one instance and the library outlives it, and its `cleanup` runs when
//! the module drops, before the library unloads.  The last [`ModuleHandle`]
//! runs that `cleanup` and unloads the library under the module's own entry
//! lock, after the instances that used it are gone.
//!
//! The registry map's lock only guards the lookup and the insert, so a slow
//! load of one binary never blocks a load of another.  Entries are never
//! removed: the entry outlives its module, so a waiter always waits in the
//! entry the loader or the last release is using, and one binary is never
//! opened twice at once.  One `PathBuf` and one mutex per binary ever loaded
//! is the accepted cost.  Both locks are taken only where a module is loaded
//! or released, which in the app is the UI thread: the audio callback never
//! drops the last reference to a plugin, because the chain runtime hands
//! every slot it lets go of back to the UI.
//!
//! [`native_binary`] turns a plugin's `lv2:binary` into the path of the
//! binary this build can open.  A bundle may list several (one per
//! platform); the first one under the bundle that exists with this
//! platform's extension wins.

use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::{Arc, Mutex};

use lv2_sys::{
    LV2_Descriptor, LV2_Descriptor_Function, LV2_Feature, LV2_Lib_Descriptor,
    LV2_Lib_Descriptor_Function,
};

use super::scan::PLATFORM_BINARY_EXTENSION;
use super::ttl::{self, TtlStore};

/// The descriptor function a library exports.
type DescriptorFunction = unsafe extern "C" fn(u32) -> *const LV2_Descriptor;

/// The library descriptor function a library may export instead.
type LibDescriptorFunction = unsafe extern "C" fn(
    *const std::ffi::c_char,
    *const *const LV2_Feature,
) -> *const LV2_Lib_Descriptor;

/// Where a module's plugin descriptors come from.
#[derive(Clone, Copy)]
pub(super) enum Descriptors {
    /// `lv2_lib_descriptor`'s result, cleaned up when the module drops.
    Library(*const LV2_Lib_Descriptor),
    /// `lv2_descriptor`.
    Function(DescriptorFunction),
}

/// How many descriptors one binary may declare.  The spec says to call
/// `lv2_descriptor` with increasing indices until it returns null; the cap
/// only keeps a broken library from spinning the host forever.
const MAX_DESCRIPTORS: u32 = 4096;

/// The `lv2:binary` predicate of a plugin.
const LV2_BINARY: &str = "http://lv2plug.in/ns/lv2core#binary";

/// One loaded LV2 library.  Every instance created from it shares the
/// `libloading::Library` through the registry, so it stays mapped until the
/// last handle is gone.
pub(super) struct Lv2Module {
    /// Resolved when the library opened.  A library with neither function,
    /// or whose `lv2_lib_descriptor` fails, is a load error.
    descriptors: Descriptors,
    /// What `lv2_lib_descriptor` was given; the library may keep both.
    _bundle_path: CString,
    _features: Box<[*const LV2_Feature; 1]>,
    /// Last, so it unloads after `Drop` ran the library's `cleanup`.  `None`
    /// only for the fakes the tests build without a binary.
    _library: Option<libloading::Library>,
}

// SAFETY: the descriptor function, the library descriptor and every
// descriptor they return are immutable data inside the library, which the
// module keeps loaded until after every handle is dropped.  The feature
// array is the module's own and only holds its null terminator.  The functions behind them follow LV2's
// thread rules, which the two halves of an instance enforce; the module
// itself only hands out pointers and is not called concurrently.
unsafe impl Send for Lv2Module {}
unsafe impl Sync for Lv2Module {}

impl Drop for Lv2Module {
    fn drop(&mut self) {
        if let Descriptors::Library(library) = self.descriptors {
            // SAFETY: the library descriptor came from the library, which is
            // still loaded, and every instance made from it is gone.
            unsafe {
                if let Some(cleanup) = (*library).cleanup {
                    cleanup((*library).handle);
                }
            }
        }
    }
}

impl Lv2Module {
    /// The descriptor at `index`, or null past the last one.
    pub(super) fn descriptor(&self, index: u32) -> *const LV2_Descriptor {
        match self.descriptors {
            // SAFETY: the library descriptor and its `get_plugin` live in the
            // library the module keeps loaded; out of range returns null.
            Descriptors::Library(library) => unsafe {
                match (*library).get_plugin {
                    Some(get_plugin) => get_plugin((*library).handle, index),
                    None => ptr::null(),
                }
            },
            // SAFETY: the function is the library's `lv2_descriptor`, which
            // the module keeps loaded, and the spec defines it for any index.
            Descriptors::Function(descriptor) => unsafe { descriptor(index) },
        }
    }

    /// A module over `descriptors` with no library behind it, for the tests.
    #[cfg(test)]
    pub(super) fn for_test(descriptors: Descriptors) -> Self {
        Self {
            descriptors,
            _bundle_path: CString::default(),
            _features: Box::new([ptr::null()]),
            _library: None,
        }
    }

    /// The descriptor whose `URI` is `uri`, by iterating until null.
    pub(super) fn descriptor_for_uri(&self, uri: &str) -> Option<*const LV2_Descriptor> {
        for index in 0..MAX_DESCRIPTORS {
            let descriptor = self.descriptor(index);
            if descriptor.is_null() {
                return None;
            }
            // SAFETY: the module keeps the library loaded, and a descriptor
            // the library returned with a non-null `URI` points at a
            // NUL-terminated string in it.
            let current = unsafe { (*descriptor).URI };
            if current.is_null() {
                continue;
            }
            if unsafe { CStr::from_ptr(current) }.to_str() == Ok(uri) {
                return Some(descriptor);
            }
        }
        None
    }
}

/// The process-wide table of module entries, keyed by the canonical path of
/// the binary.  The map's lock only guards the lookup and the insert; each
/// path has its own entry, whose lock is held across that module's load and
/// across its cleanup and unload, so a slow load of one module never blocks a
/// load of another.
///
/// An entry is never removed.  It outlives the module it opened, so a waiter
/// always waits in the entry the loader or the last release is using: one
/// binary is never opened twice at once, and no unload races a load of it.
pub(super) struct ModuleRegistry {
    modules: Mutex<BTreeMap<PathBuf, Arc<ModuleEntry>>>,
}

/// One canonical binary path's share of the registry: its loaded module and
/// how many handles hold it.  The lock is held across that module's load and
/// across its cleanup and unload, so those never overlap.
struct ModuleEntry {
    state: Mutex<EntryState>,
}

/// What one [`ModuleEntry`] holds.  `module` stays `None` after a failed
/// `open`, so the empty entry is reused and the next acquire retries in
/// place.
struct EntryState {
    module: Option<Arc<Lv2Module>>,
    /// The live [`ModuleHandle`]s for `module`.
    users: usize,
}

/// The registry every load goes through.
static MODULES: ModuleRegistry = ModuleRegistry::new();

impl ModuleEntry {
    fn lock(&self) -> std::sync::MutexGuard<'_, EntryState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl ModuleRegistry {
    pub(super) const fn new() -> Self {
        Self {
            modules: Mutex::new(BTreeMap::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<PathBuf, Arc<ModuleEntry>>> {
        self.modules
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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

    /// Hands out the module stored under `key`, opening it with `open` first
    /// when it is not loaded.  A module whose `open` fails is not stored, so
    /// the next acquire tries again in place.
    pub(super) fn acquire(
        &self,
        key: PathBuf,
        open: impl FnOnce() -> Result<Lv2Module, String>,
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
        let module = Arc::new(open()?);
        state.module = Some(Arc::clone(&module));
        state.users = 1;
        Ok(ModuleHandle {
            entry: Arc::clone(&entry),
            module: Some(module),
        })
    }
}

/// One user's share of an [`Lv2Module`].  Handles are never cloned; each one
/// comes from [`acquire_module`].  Dropping the last one runs the library's
/// `cleanup` and unloads it, under that module's entry lock.
pub(super) struct ModuleHandle {
    entry: Arc<ModuleEntry>,
    /// Always `Some` until `drop`.
    module: Option<Arc<Lv2Module>>,
}

impl std::ops::Deref for ModuleHandle {
    type Target = Lv2Module;

    fn deref(&self) -> &Lv2Module {
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
            // first and the drop below then runs the library's `cleanup` and
            // the unload here, under the entry lock the next acquire of it
            // needs.
            debug_assert_eq!(Arc::strong_count(&module), 2);
            state.module = None;
        }
        drop(module);
        drop(state);
    }
}

/// The registry key for an LV2 binary: its canonical path, so different
/// spellings of one file share one module.  When the path cannot be
/// canonicalised, the absolute path is used instead.
fn module_key(binary: &Path) -> PathBuf {
    std::fs::canonicalize(binary)
        .or_else(|_| std::path::absolute(binary))
        .unwrap_or_else(|_| binary.to_path_buf())
}

/// Opens an LV2 library and resolves its descriptors.  `bundle_path` ends in
/// a separator, as `lv2_lib_descriptor` expects.
fn open_module(binary: &Path, bundle_path: &str) -> Result<Lv2Module, String> {
    let bundle_path = CString::new(bundle_path)
        .map_err(|_| "The LV2 bundle path contains a NUL byte.".to_owned())?;
    let features: Box<[*const LV2_Feature; 1]> = Box::new([ptr::null()]);
    let library = unsafe { libloading::Library::new(binary) }
        .map_err(|error| format!("LV2 load failed: {error}"))?;
    let lib_function =
        unsafe { library.get::<LV2_Lib_Descriptor_Function>(b"lv2_lib_descriptor\0") }
            .ok()
            .and_then(|symbol| *symbol);
    let function = unsafe { library.get::<LV2_Descriptor_Function>(b"lv2_descriptor\0") }
        .ok()
        .and_then(|symbol| *symbol);
    let descriptors = resolve(lib_function, function, &bundle_path, features.as_ptr())?;
    Ok(Lv2Module {
        descriptors,
        _bundle_path: bundle_path,
        _features: features,
        _library: Some(library),
    })
}

/// Picks the descriptors a library offers: `lv2_lib_descriptor` first, as
/// lilv does, and a null result from it is an error, not a fallback.
pub(super) fn resolve(
    lib_function: Option<LibDescriptorFunction>,
    function: Option<DescriptorFunction>,
    bundle_path: &CStr,
    features: *const *const LV2_Feature,
) -> Result<Descriptors, String> {
    if let Some(lib_function) = lib_function {
        // SAFETY: the function is the library's `lv2_lib_descriptor`; the
        // path and the null-terminated feature array outlive the module.
        let library = unsafe { lib_function(bundle_path.as_ptr(), features) };
        if library.is_null() {
            return Err("The LV2 library descriptor could not be created.".to_owned());
        }
        return Ok(Descriptors::Library(library));
    }
    function
        .map(Descriptors::Function)
        .ok_or_else(|| "The LV2 binary exports no lv2_descriptor.".to_owned())
}

/// The shared module for a platform binary, loading it if no one else uses
/// it.  `bundle_path` is the bundle folder with a trailing separator.
pub(super) fn acquire_module(binary: &Path, bundle_path: &str) -> Result<ModuleHandle, String> {
    MODULES.acquire(module_key(binary), || open_module(binary, bundle_path))
}

/// The plugin's binary for this platform, from the parsed bundle.  Every
/// `lv2:binary` is considered: the path must stay inside the bundle, exist,
/// and carry this platform's extension.  The first match wins, so a bundle
/// that names a foreign binary first still resolves.
pub(super) fn native_binary(
    store: &TtlStore,
    plugin: &str,
    bundle: &Path,
) -> Result<PathBuf, String> {
    let root = ttl::bundle_root(bundle);
    for binary in store.iris(plugin, LV2_BINARY) {
        let Ok(path) = ttl::path_from_file_uri(&binary) else {
            continue;
        };
        if !path.starts_with(&root) || !path.is_file() {
            continue;
        }
        if path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case(PLATFORM_BINARY_EXTENSION))
        {
            return Ok(path);
        }
    }
    Err("This LV2 plugin has no binary for this platform.".to_owned())
}
