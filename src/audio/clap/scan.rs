//! CLAP bundle discovery and descriptor scanning.  Unlike VST3 and LV2,
//! listing a CLAP bundle loads its module and calls the bundle's own `init`
//! in this process, so this is the only scan that runs third-party code
//! just to say what a bundle holds.

use std::ffi::c_char;
use std::path::{Path, PathBuf};

use clap_sys::plugin::clap_plugin_descriptor;

use super::cstring;
use super::module::{acquire_module, ClapModule};

pub(crate) fn fixed_string(buffer: &[c_char]) -> String {
    let bytes: Vec<u8> = buffer
        .iter()
        .take_while(|byte| **byte != 0)
        .map(|byte| *byte as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}
/// One discovered CLAP plugin, or one bundle that could not be read.
#[derive(Clone, Debug)]
pub(crate) struct ClapCandidate {
    pub(crate) path: PathBuf,
    /// The plugin id, empty on a bundle that could not be read.
    pub(crate) plugin_id: String,
    pub(crate) name: String,
    pub(crate) vendor: String,
    pub(crate) category: String,
    /// The descriptor's features as declared (`category` joins them).
    pub(crate) features: Vec<String>,
    /// Why the bundle could not be read.  Set only on a row that exists to
    /// report the failure; a readable bundle leaves it `None`.
    pub(crate) scan_error: Option<String>,
}

/// Standard CLAP locations, plus `CLAP_PATH` and `RIGWERK_CLAP_PATH`.
pub(crate) fn scan_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        let mut folder = PathBuf::from(home);
        #[cfg(target_os = "macos")]
        folder.push("Library/Audio/Plug-Ins/CLAP");
        #[cfg(target_os = "linux")]
        folder.push(".clap");
        #[cfg(target_os = "windows")]
        folder.push("AppData/Local/Programs/Common/CLAP");
        roots.push(folder);
    }
    #[cfg(target_os = "macos")]
    roots.push(PathBuf::from("/Library/Audio/Plug-Ins/CLAP"));
    #[cfg(target_os = "linux")]
    roots.push(PathBuf::from("/usr/lib/clap"));
    #[cfg(target_os = "windows")]
    {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            roots.push(PathBuf::from(local).join("Programs/Common/CLAP"));
        }
        if let Some(common) = std::env::var_os("CommonProgramFiles") {
            roots.push(PathBuf::from(common).join("CLAP"));
        }
    }
    if let Ok(extra) = std::env::var("CLAP_PATH") {
        roots.extend(std::env::split_paths(&extra));
    }
    if let Ok(extra) = std::env::var("RIGWERK_CLAP_PATH") {
        roots.extend(std::env::split_paths(&extra));
    }
    roots
}

/// Standard CLAP locations, plus `RIGWERK_CLAP_PATH` for a custom folder.
pub(crate) fn scan_library() -> Vec<ClapCandidate> {
    let mut bundles = Vec::new();
    for root in scan_roots() {
        collect_bundles(&root, 0, &mut bundles);
    }
    scan_bundles(bundles)
}

/// Lists the effects in `bundles`, keeping one row for a bundle that cannot
/// be read.  Split from the root walk so a test can scan a folder it filled.
pub(super) fn scan_bundles(mut bundles: Vec<PathBuf>) -> Vec<ClapCandidate> {
    bundles.sort();
    bundles.dedup();

    let mut found = Vec::new();
    for bundle in bundles {
        match scan_bundle(&bundle) {
            Ok(mut candidates) => found.append(&mut candidates),
            // Kept as a row rather than skipped: a bundle that cannot be
            // read has no descriptor, so this is the only thing the browser
            // can show for it.
            Err(error) => found.push(unreadable_bundle(&bundle, error)),
        }
    }
    found.sort_by_cached_key(|candidate| candidate.name.to_lowercase());
    found.dedup_by(|a, b| {
        a.path == b.path && a.plugin_id == b.plugin_id && a.name.eq_ignore_ascii_case(&b.name)
    });
    found
}

/// The row for a bundle whose module did not load or panicked.
fn unreadable_bundle(bundle: &Path, error: String) -> ClapCandidate {
    ClapCandidate {
        path: bundle.to_path_buf(),
        plugin_id: String::new(),
        name: bundle
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("CLAP plugin")
            .to_owned(),
        vendor: String::new(),
        category: "CLAP effect".to_owned(),
        features: Vec::new(),
        scan_error: Some(error),
    }
}

/// Lists the effects in one bundle.  The scan shares the module with any
/// loaded instance of it: a module already in use is neither initialised
/// again nor deinitialised when the scan finishes.
///
/// A Rust panic thrown while the bundle is read is contained and reported
/// as a failed bundle, with the panic's own text after that sentence when it
/// has one.  Dropping the handle runs the bundle's `deinit`; a panic there
/// cannot unwind the scan, so the rows already found are still returned.
/// A panic the module turns into `abort` at its `extern "C"` boundary cannot
/// be caught here: the process ends before the unwind reaches this frame,
/// and only a subprocess could survive that.
pub(crate) fn scan_bundle(bundle: &Path) -> Result<Vec<ClapCandidate>, String> {
    // `acquire_module` calls the bundle's `init`; `scan_module` calls its
    // factory and descriptors; dropping the handle calls `deinit`.  Each
    // step runs inside `contained`, so a host-side panic becomes a scan
    // error instead of an unwind that loses the rows already found.
    let module = contained(|| acquire_module(bundle))??;
    let found = contained(|| scan_module(bundle, &module));
    let _ = contained(move || drop(module));
    found
}

/// The sentence a contained panic reports, used as the prefix of the error
/// and on its own when the payload carries no text.
pub(super) const PANIC_WHILE_READ: &str = "The bundle panicked while it was read.";

/// Runs third-party code and turns a Rust panic into a scan error.
pub(super) fn contained<T>(work: impl FnOnce() -> T) -> Result<T, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)).map_err(|payload| {
        let text = payload
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| payload.downcast_ref::<String>().map(String::as_str));
        match text {
            Some(text) if !text.is_empty() => format!("{PANIC_WHILE_READ} {text}"),
            _ => PANIC_WHILE_READ.to_owned(),
        }
    })
}

pub(super) fn scan_module(bundle: &Path, module: &ClapModule) -> Vec<ClapCandidate> {
    let factory = module.plugin_factory();
    if factory.is_null() {
        return Vec::new();
    }
    let count = unsafe { (*factory).get_plugin_count }
        .map(|get| unsafe { get(factory) })
        .unwrap_or(0);
    let mut found = Vec::new();
    for index in 0..count {
        let Some(get) = (unsafe { (*factory).get_plugin_descriptor }) else {
            break;
        };
        let descriptor = unsafe { get(factory, index) };
        let Some(descriptor) = (unsafe { descriptor.as_ref() }) else {
            continue;
        };
        let Some(candidate) = candidate_from_descriptor(bundle, descriptor) else {
            continue;
        };
        found.push(candidate);
    }
    found
}

pub(super) fn candidate_from_descriptor(
    bundle: &Path,
    descriptor: &clap_plugin_descriptor,
) -> Option<ClapCandidate> {
    let features = descriptor_features(descriptor);
    if features.iter().any(|feature| {
        feature.eq_ignore_ascii_case("instrument") || feature.eq_ignore_ascii_case("note-effect")
    }) {
        return None;
    }
    let id = cstring(descriptor.id);
    if id.trim().is_empty() {
        return None;
    }
    let name = cstring(descriptor.name);
    let name = if name.trim().is_empty() {
        bundle
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("CLAP plugin")
            .to_owned()
    } else {
        name
    };
    let vendor = cstring(descriptor.vendor);
    let category = if features.is_empty() {
        "CLAP effect".to_owned()
    } else {
        features.join(" · ")
    };
    Some(ClapCandidate {
        path: bundle.to_path_buf(),
        plugin_id: id,
        name,
        vendor,
        category,
        features,
        scan_error: None,
    })
}

pub(super) fn descriptor_features(descriptor: &clap_plugin_descriptor) -> Vec<String> {
    let mut features = Vec::new();
    if descriptor.features.is_null() {
        return features;
    }
    for index in 0..64 {
        let feature = unsafe { *descriptor.features.add(index) };
        if feature.is_null() {
            break;
        }
        features.push(cstring(feature));
    }
    features
}

pub(super) fn collect_bundles(folder: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 4 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(folder) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_clap = path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("clap"));
        if is_clap {
            out.push(path);
        } else if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            collect_bundles(&path, depth + 1, out);
        }
    }
}
