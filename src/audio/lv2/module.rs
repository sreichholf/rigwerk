//! Shared LV2 module loading and the process-wide registry.
//!
//! A bundle names one binary per platform, and that binary may hold several
//! plugin descriptors.  The OS shares one copy of a library between every
//! `dlopen`/`LoadLibrary` of it, and a binary expects its global state to be
//! set up once, so every instance and scan of one binary shares a single
//! [`Lv2Module`] through the [`ModuleRegistry`], keyed by the canonical path.
//!
//! A library exports `lv2_lib_descriptor`, `lv2_descriptor` or both.  As in
//! lilv, `lv2_lib_descriptor(bundle_path, features)` is tried first and its
//! failure is a load error; `lv2_descriptor` is the fallback.  The library
//! descriptor gets an empty feature array, since the host's features belong
//! to one instance and the library outlives it, and its `cleanup` runs when
//! the module drops, before the library unloads.  The last [`ModuleHandle`]
//! removes the module and unloads the library under the registry lock, after
//! the instances that used it have called `cleanup`.  The lock is only taken on the UI and scan
//! threads; the audio callback never drops the last reference to a plugin,
//! because the chain runtime hands every slot it lets go of back to the UI.
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

/// The process-wide table of loaded LV2 modules, keyed by the canonical path
/// of the binary.  One lock covers every module, so a slow library load of
/// one module delays loads of others until it finishes; loads are rare and
/// happen off the audio thread, so this is accepted for the simpler
/// guarantee that no unload races a load of the same file.
struct ModuleRegistry {
    modules: Mutex<BTreeMap<PathBuf, Arc<Lv2Module>>>,
}

/// The registry every load goes through.
static MODULES: ModuleRegistry = ModuleRegistry::new();

impl ModuleRegistry {
    const fn new() -> Self {
        Self {
            modules: Mutex::new(BTreeMap::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<PathBuf, Arc<Lv2Module>>> {
        self.modules
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Hands out the module stored under `key`, opening it with `open` first
    /// when no one is using it.  A module whose `open` fails is not stored,
    /// so the next acquire tries again.
    fn acquire(
        &'static self,
        key: PathBuf,
        open: impl FnOnce() -> Result<Lv2Module, String>,
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
        let module = Arc::new(open()?);
        modules.insert(key.clone(), Arc::clone(&module));
        Ok(ModuleHandle {
            registry: self,
            key,
            module: Some(module),
        })
    }
}

/// One user's share of an [`Lv2Module`].  Handles are never cloned; each one
/// comes from [`acquire_module`].  Dropping the last one removes the module
/// and unloads its library, under the registry lock.
pub(super) struct ModuleHandle {
    registry: &'static ModuleRegistry,
    key: PathBuf,
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
        let mut modules = self.registry.lock();
        // Every reference is created (`acquire`) and released (here) under
        // the lock, so the count is stable while it is held: two references
        // (the table's and this one) mean this is the last user.
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
        // On the last release the library unloads here, still under the lock.
        drop(stored);
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
