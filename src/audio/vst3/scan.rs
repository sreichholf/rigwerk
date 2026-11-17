//! Finding VST3 bundles and reading what they declare, without loading
//! them.
//!
//! The scan runs on the `rigwerk-plugin-scan` thread, and the module
//! registry (`module.rs`) loads and drops binaries on the UI thread only,
//! so nothing here loads a binary.  A bundle is listed when it has a
//! binary this build can load (`module::binary_path`, the same lookup a
//! load uses: the architecture folder, then the binary's header), and its
//! classes come from `moduleinfo.json`.
//!
//! A bundle without `moduleinfo.json` (SDKs before 3.7) is not asked for
//! its classes here.  Listing them would mean loading every such binary on
//! the UI thread when the scan finishes, where a slow or crashing plugin
//! would stall or take down the app before anyone picked it.  The app
//! lists that bundle as one row without a class id instead, and the load
//! takes the factory's first audio class (`factory::audio_class`).
//!
//! `moduleinfo.json` is what Steinberg calls JSON5; `moduleinfotool`
//! writes JSON with trailing commas, and comments may appear.  Those two
//! are stripped before `serde_json` reads it; other JSON5 forms are
//! refused, and the bundle then gets the same one-row fallback.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// A larger `moduleinfo.json` is refused rather than read.
const MAX_MODULE_INFO_BYTES: u64 = 4 * 1024 * 1024;

/// What a bundle's `moduleinfo.json` declares, as far as the scan needs it.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ModuleInfo {
    #[serde(rename = "Factory Info", default)]
    pub(crate) factory: FactoryInfo,
    /// Only classes with a valid id, which is upper case here.
    #[serde(rename = "Classes", default)]
    pub(crate) classes: Vec<ModuleClass>,
}

/// The module's vendor, the fallback for a class without one.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct FactoryInfo {
    #[serde(rename = "Vendor", default)]
    pub(crate) vendor: String,
}

/// One class of the module.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ModuleClass {
    /// 32 hex digits in canonical order, as preferences save it.
    #[serde(rename = "CID", default)]
    pub(crate) class_id: String,
    #[serde(rename = "Category", default)]
    pub(crate) category: String,
    #[serde(rename = "Name", default)]
    pub(crate) name: String,
    #[serde(rename = "Vendor", default)]
    pub(crate) vendor: String,
    #[serde(rename = "Sub Categories", default)]
    pub(crate) sub_categories: Vec<String>,
}

/// The folders to scan: the platform's standard VST3 folders, then the
/// `;`-separated folders in `RIGWERK_VST3_PATH`.
pub(crate) fn search_roots() -> Vec<PathBuf> {
    let mut roots = standard_roots();
    if let Ok(extra) = std::env::var("RIGWERK_VST3_PATH") {
        roots.extend(extra_roots(&extra));
    }
    roots
}

/// The folders in a `RIGWERK_VST3_PATH` value: `;`-separated, trimmed,
/// empty parts skipped.
pub(super) fn extra_roots(value: &str) -> Vec<PathBuf> {
    value
        .split(';')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// The VST3 SDK's standard folders on Linux: the user's, then the
/// system's.
#[cfg(target_os = "linux")]
fn standard_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(PathBuf::from(home).join(".vst3"));
    }
    roots.push(PathBuf::from("/usr/lib/vst3"));
    roots.push(PathBuf::from("/usr/local/lib/vst3"));
    roots
}

/// The VST3 SDK's standard folders on Windows: the user's, then Common
/// Files.  `CommonProgramFiles` is the folder for this build's bitness, so
/// a 64-bit build does not scan the 32-bit plugins in `Program Files (x86)`.
#[cfg(target_os = "windows")]
fn standard_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        roots.push(PathBuf::from(local).join(r"Programs\Common\VST3"));
    }
    let common = std::env::var_os("CommonProgramFiles")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files\Common Files"));
    roots.push(common.join("VST3"));
    roots
}

/// No standard folders where RigWerk cannot host VST3.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn standard_roots() -> Vec<PathBuf> {
    Vec::new()
}

/// Every `.vst3` under `roots`, sorted.  A folder is searched recursively,
/// but a bundle is not searched inside: on Windows its binary is a `.vst3`
/// too.  A root that is itself a bundle is taken as one.  Each folder is
/// read once, so a symlink back to a parent ends instead of looping.
pub(crate) fn find_bundles(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut visited = HashSet::new();
    for root in roots {
        if is_bundle_name(root) && root.exists() {
            found.push(root.clone());
        } else {
            walk(root, &mut visited, &mut found);
        }
    }
    found.sort();
    found.dedup();
    found
}

fn walk(dir: &Path, visited: &mut HashSet<PathBuf>, found: &mut Vec<PathBuf>) {
    let Ok(real) = dir.canonicalize() else {
        return;
    };
    if !visited.insert(real) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if is_bundle_name(&path) {
            found.push(path);
        } else if path.is_dir() {
            walk(&path, visited, found);
        }
    }
}

/// A `.vst3` bundle folder or single-file plugin, by its name.
fn is_bundle_name(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("vst3"))
}

/// Whether the bundle holds a binary this build can load.  A bundle that
/// only ships another architecture's binary is left out of the library.
/// Reads the start of the binary, never loads it.
pub(crate) fn has_loadable_binary(bundle: &Path) -> bool {
    super::module::binary_path(bundle).is_ok()
}

/// The bundle's `moduleinfo.json` (`Contents/Resources`, or the SDK 3.7.5
/// `Contents` location), or `None` when there is none.  `path` is the
/// bundle, or a file inside it.
pub(crate) fn read_module_info(path: &Path) -> Result<Option<ModuleInfo>, String> {
    let Some(bundle) = bundle_root(path) else {
        return Ok(None);
    };
    let Some(file) = [
        "Contents/Resources/moduleinfo.json",
        "Contents/moduleinfo.json",
    ]
    .iter()
    .map(|relative| bundle.join(relative))
    .find(|file| file.is_file()) else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    std::fs::File::open(&file)
        .and_then(|opened| {
            opened
                .take(MAX_MODULE_INFO_BYTES + 1)
                .read_to_end(&mut bytes)
        })
        .map_err(|error| format!("cannot read {}: {error}", file.display()))?;
    if bytes.len() as u64 > MAX_MODULE_INFO_BYTES {
        return Err(format!(
            "{} is larger than {MAX_MODULE_INFO_BYTES} bytes",
            file.display()
        ));
    }
    let text = String::from_utf8(bytes).map_err(|_| format!("{} is not UTF-8", file.display()))?;
    parse_module_info(&text)
        .map(Some)
        .map_err(|error| format!("{}: {error}", file.display()))
}

/// The bundle folder `path` names or lies in.
fn bundle_root(path: &Path) -> Option<&Path> {
    if path.is_dir() {
        Some(path)
    } else {
        path.ancestors()
            .skip(1)
            .find(|ancestor| is_bundle_name(ancestor) && ancestor.is_dir())
    }
}

/// `moduleinfo.json` text into its classes.  A class whose id is not 32
/// hex digits is dropped; the others keep their order.
pub(super) fn parse_module_info(text: &str) -> Result<ModuleInfo, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let json = strip_json5(text)?;
    let mut info: ModuleInfo = serde_json::from_str(&json).map_err(|error| error.to_string())?;
    info.classes.retain(|class| {
        class.class_id.len() == 32 && class.class_id.bytes().all(|byte| byte.is_ascii_hexdigit())
    });
    for class in &mut info.classes {
        class.class_id.make_ascii_uppercase();
    }
    Ok(info)
}

/// JSON with comments and trailing commas as plain JSON.  Strings are
/// copied as they are; a comment becomes whitespace.
pub(super) fn strip_json5(text: &str) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '/' if chars.peek() == Some(&'/') => {
                // The newline stays, so line numbers in errors still match.
                while chars.next_if(|&next| next != '\n').is_some() {}
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut previous = '\0';
                loop {
                    match chars.next() {
                        Some('/') if previous == '*' => break,
                        Some(next) => {
                            if next == '\n' {
                                out.push('\n');
                            }
                            previous = next;
                        }
                        None => return Err("unterminated block comment".to_owned()),
                    }
                }
                out.push(' ');
            }
            '}' | ']' => {
                let end = out.trim_end().len();
                if out[..end].ends_with(',') {
                    out.remove(end - 1);
                }
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    if in_string {
        return Err("unterminated string".to_owned());
    }
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests;
