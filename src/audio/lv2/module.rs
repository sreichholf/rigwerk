//! One [`Lv2Module`] per binary, shared through [`ModuleRegistry`].  The OS
//! shares one mapping, and a binary expects its globals set up once.
//! Nothing else opens a binary: the scan reads Turtle only.
//!
//! `lv2_lib_descriptor` is tried first, as in lilv, and its failure is a
//! load error; `lv2_descriptor` is the fallback.  The library descriptor gets
//! an empty feature array (features belong to one instance) and its
//! `cleanup` runs when the module drops, before the library unloads, under
//! the entry lock.
//!
//! The map lock only covers lookup and insert.  Entries are never removed,
//! so a waiter always waits in the entry the loader or the last release is
//! using, and one binary is never opened twice at once.  Both locks are
//! taken only on the UI thread: the audio callback never drops the last
//! reference, because the chain hands a released slot back to the UI.

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

type DescriptorFunction = unsafe extern "C" fn(u32) -> *const LV2_Descriptor;

type LibDescriptorFunction = unsafe extern "C" fn(
    *const std::ffi::c_char,
    *const *const LV2_Feature,
) -> *const LV2_Lib_Descriptor;

#[derive(Clone, Copy)]
pub(super) enum Descriptors {
    /// `lv2_lib_descriptor`.  `cleanup` runs when the module drops.
    Library(*const LV2_Lib_Descriptor),
    /// `lv2_descriptor`.
    Function(DescriptorFunction),
}

/// Caps the `lv2_descriptor` index walk.  A broken library must not spin
/// the host forever.
const MAX_DESCRIPTORS: u32 = 4096;

const LV2_BINARY: &str = "http://lv2plug.in/ns/lv2core#binary";

/// Shared by every instance of one binary, so the library stays mapped
/// until the last handle is gone.
pub(super) struct Lv2Module {
    /// A library with neither export, or a failed `lv2_lib_descriptor`, is a
    /// load error.
    descriptors: Descriptors,
    /// What `lv2_lib_descriptor` was given.  The library may keep the path
    /// and the feature array.
    _bundle_path: CString,
    _features: Box<[*const LV2_Feature; 1]>,
    /// Last, so it unloads after `Drop` ran the library's `cleanup`.  `None`
    /// only for the fakes the tests build without a binary.
    _library: Option<libloading::Library>,
}

// SAFETY: the descriptor function, the library descriptor and every
// descriptor they return are immutable data inside the library, which the
// module keeps loaded until after every handle is dropped.  The feature
// array is the module's own and only holds its null terminator.  The
// functions follow LV2's thread rules, which the two halves of an instance
// enforce; the module itself only hands out pointers and is not called
// concurrently.
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

    #[cfg(test)]
    pub(super) fn for_test(descriptors: Descriptors) -> Self {
        Self {
            descriptors,
            _bundle_path: CString::default(),
            _features: Box::new([ptr::null()]),
            _library: None,
        }
    }

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

/// Keyed by canonical path.  The map lock only covers lookup and insert;
/// each entry's lock covers that module's load, cleanup and unload.  An
/// entry is never removed, so one binary is never opened twice at once and
/// no unload races a load.
pub(super) struct ModuleRegistry {
    modules: Mutex<BTreeMap<PathBuf, Arc<ModuleEntry>>>,
}

/// The lock is held across this path's load, cleanup and unload.
struct ModuleEntry {
    state: Mutex<EntryState>,
}

/// `module` stays `None` after a failed `open`, so the next acquire retries
/// in place.
struct EntryState {
    module: Option<Arc<Lv2Module>>,
    users: usize,
}

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

    /// A failed `open` is not stored, so the next acquire retries in place.
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

/// Handles are never cloned.  Dropping the last one runs `cleanup` and
/// unloads, under the entry lock.
pub(super) struct ModuleHandle {
    entry: Arc<ModuleEntry>,
    /// `Some` until `drop`.
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
            // This handle and the entry's `Option` are the last two
            // references.  Clearing the entry first drops the library here,
            // under the lock the next acquire needs.
            debug_assert_eq!(Arc::strong_count(&module), 2);
            state.module = None;
        }
        drop(module);
        drop(state);
    }
}

/// Canonical path, so spellings of one file share a module.  Absolute when
/// canonicalise fails.
fn module_key(binary: &Path) -> PathBuf {
    std::fs::canonicalize(binary)
        .or_else(|_| std::path::absolute(binary))
        .unwrap_or_else(|_| binary.to_path_buf())
}

/// `bundle_path` ends in a separator, as `lv2_lib_descriptor` expects.
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

/// `lv2_lib_descriptor` first, as lilv does.  A null from it is an error,
/// not a fallback.
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

/// `bundle_path` is the bundle folder with a trailing separator.
pub(super) fn acquire_module(binary: &Path, bundle_path: &str) -> Result<ModuleHandle, String> {
    MODULES.acquire(module_key(binary), || open_module(binary, bundle_path))
}

/// The first `lv2:binary` inside the bundle that exists with this
/// platform's extension.  A foreign binary named first is skipped.
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
