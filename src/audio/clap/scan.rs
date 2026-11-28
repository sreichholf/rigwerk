//! CLAP scan loads the module and calls `init` in-process, unlike VST3
//! and LV2.  The browser runs it in a child ([`scan_library_isolated`]):
//! a hung `init` or an `abort` costs the child, not the app or its
//! module lock.

use std::ffi::c_char;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

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
/// One plugin, or one bundle that could not be read.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct ClapCandidate {
    pub(crate) path: PathBuf,
    /// Empty on a bundle that could not be read.
    pub(crate) plugin_id: String,
    pub(crate) name: String,
    pub(crate) vendor: String,
    pub(crate) category: String,
    /// Joined into `category`.
    pub(crate) features: Vec<String>,
    /// Set only on a row that exists to report the failure.
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

/// A parent that sees another version treats the scan as failed.
/// Bump when `ClapCandidate` changes shape.
const SCAN_FILE_VERSION: u32 = 1;

/// A version tag and the scan.  An unknown version is refused, so the
/// parent only trusts a file this build wrote.
#[derive(serde::Serialize, serde::Deserialize)]
struct ScanFile {
    version: u32,
    candidates: Vec<ClapCandidate>,
}

impl ScanFile {
    fn new(candidates: Vec<ClapCandidate>) -> Self {
        Self {
            version: SCAN_FILE_VERSION,
            candidates,
        }
    }
}

pub(super) fn encode_scan(candidates: Vec<ClapCandidate>) -> Result<String, ron::Error> {
    ron::ser::to_string(&ScanFile::new(candidates))
}

/// A path that is not valid UTF-8 cannot cross as itself.  It becomes
/// one error row, so the encode does not hide every other bundle.
pub(super) fn wire_candidates(candidates: Vec<ClapCandidate>) -> Vec<ClapCandidate> {
    candidates
        .into_iter()
        .map(|mut candidate| {
            if candidate.path.to_str().is_some() {
                return candidate;
            }
            candidate.plugin_id.clear();
            candidate.features.clear();
            candidate.scan_error = Some("The bundle path is not valid UTF-8.".to_owned());
            candidate.path = PathBuf::from(candidate.path.to_string_lossy().into_owned());
            candidate
        })
        .collect()
}

/// `None` when the text does not parse or another version wrote it.
pub(super) fn decode_scan(text: &str) -> Option<Vec<ClapCandidate>> {
    let file: ScanFile = ron::from_str(text).ok()?;
    (file.version == SCAN_FILE_VERSION).then_some(file.candidates)
}

/// `create_new`, so an existing path is never overwritten.  `Err` is
/// why the child cannot report; the parent then uses the exit status.
pub(crate) fn write_scan_file(path: &Path, candidates: Vec<ClapCandidate>) -> Result<(), String> {
    use std::io::Write;

    let text = encode_scan(wire_candidates(candidates))
        .map_err(|error| format!("The CLAP scan could not be encoded: {error}"))?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| format!("{} could not be created: {error}", path.display()))?;
    file.write_all(text.as_bytes())
        .map_err(|error| format!("{} could not be written: {error}", path.display()))
}

/// The child could not write its result.
pub(crate) const CHILD_EXIT_UNWRITABLE: i32 = 1;

/// The child's watchdog ended it: a bundle held it past every deadline
/// the parent has.
pub(crate) const CHILD_EXIT_ABANDONED: i32 = 3;

/// An `init` that never returns holds the child's module lock, and an
/// `abort` ends only the child.  `timeout` is the parent's whole budget;
/// past it the child is killed and each root gets a failure row.  Zero
/// starts no child.  This does not touch the parent's module registry.
pub(crate) fn scan_library_isolated(timeout: Duration) -> Vec<ClapCandidate> {
    let roots = scan_roots();
    if timeout.is_zero() {
        return resolve(Err(ScanFailure::NoTime), &roots);
    }
    resolve(run_scan_child(timeout), &roots)
}

/// Candidates pass through.  A failure is one row per scan root.
/// Pure, so a test drives both without a process.
pub(super) fn resolve(
    result: Result<Vec<ClapCandidate>, ScanFailure>,
    roots: &[PathBuf],
) -> Vec<ClapCandidate> {
    match result {
        Ok(candidates) => candidates,
        Err(cause) => failure_rows(roots, &cause),
    }
}

/// The name is predictable and the temp directory is shared, so the
/// file is not created there on its own.  The folder is mode 0700 on
/// Unix and the file is `create_new`.  Windows temp is already
/// user-scoped.
struct ScanResult {
    dir: PathBuf,
    file: PathBuf,
}

impl ScanResult {
    fn create() -> Result<Self, ScanFailure> {
        let dir = scan_result_dir()?;
        let file = dir.join("scan.ron");
        Ok(Self { dir, file })
    }
}

impl Drop for ScanResult {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.file);
        // A child the parent gave up on may still be writing.
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Pid, counter, and clock keep two scans apart.  A name already taken
/// is skipped, so a scan never writes a folder it did not create.
pub(super) fn scan_result_dir() -> Result<PathBuf, ScanFailure> {
    static NEXT: AtomicU64 = AtomicU64::new(0);

    let base = std::env::temp_dir();
    for _ in 0..8 {
        let serial = NEXT.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or_default();
        let dir = base.join(format!(
            "rigwerk-clap-scan-{}-{serial}-{nanos}",
            std::process::id()
        ));
        match create_private_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err(ScanFailure::Spawn),
        }
    }
    Err(ScanFailure::Spawn)
}

/// No group or other access on Unix.
#[cfg(unix)]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    std::fs::DirBuilder::new().mode(0o700).create(dir)
}

/// Windows temp is already user-scoped.
#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir(dir)
}

/// Short enough that a quick scan returns, long enough not to spin.
const SCAN_POLL: Duration = Duration::from_millis(25);

/// The child may outlive the parent's wait: the parent can be killed,
/// or its kill can fail.  The child's watchdog ends it here.
const CHILD_GRACE: Duration = Duration::from_secs(5);

/// `kill` can fail, and `wait` would then block until the child ends.
const KILL_SETTLE: Duration = Duration::from_secs(2);

/// stdin, stdout, and stderr are null: the release build is a
/// GUI-subsystem process, and a pipe would have to be drained.
/// [`ScanResult`] removes the file and folder on every path.
///
/// Not covered by a unit test: under `cargo test` the binary is the
/// harness, and libtest would take `--clap-scan`.
fn run_scan_child(timeout: Duration) -> Result<Vec<ClapCandidate>, ScanFailure> {
    let result = ScanResult::create()?;
    let path = result.file.as_path();
    let exe = std::env::current_exe().map_err(|_| ScanFailure::Spawn)?;
    let mut child = Command::new(exe)
        .arg("--clap-scan")
        .arg(path)
        // The child's own limit, so it stops even if this process cannot.
        .arg((timeout + CHILD_GRACE).as_millis().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| ScanFailure::Spawn)?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                abandon_child(&mut child);
                return Err(ScanFailure::TimedOut);
            }
            Ok(None) => std::thread::sleep(SCAN_POLL),
            Err(_) => {
                abandon_child(&mut child);
                return Err(ScanFailure::Unreadable);
            }
        }
    };
    if !status.success() {
        return Err(match status.code() {
            Some(CHILD_EXIT_ABANDONED) => ScanFailure::Abandoned,
            Some(CHILD_EXIT_UNWRITABLE) => ScanFailure::Unwritable,
            _ => ScanFailure::Exited(status.to_string()),
        });
    }
    let text = std::fs::read_to_string(path);
    decode_scan(&text.map_err(|_| ScanFailure::Unreadable)?).ok_or(ScanFailure::Unreadable)
}

/// Best effort.  If the child is not gone within [`KILL_SETTLE`], the
/// handle is dropped and the child is left to the OS.
fn abandon_child(child: &mut Child) {
    let _ = child.kill();
    let settle = Instant::now() + KILL_SETTLE;
    while Instant::now() < settle {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => std::thread::sleep(SCAN_POLL),
        }
    }
}

/// An unknown exit is text, not `ExitStatus`, so [`failure_rows`] stays
/// pure.
#[derive(Debug)]
pub(super) enum ScanFailure {
    /// The parent's budget passed; the child was killed.
    TimedOut,
    /// The child's own watchdog ended it: a bundle would not return.
    Abandoned,
    Unwritable,
    /// Any other non-zero exit; the text is its `ExitStatus` as displayed.
    Exited(String),
    /// The child process could not be started, or the folder for its result
    /// could not be created.
    Spawn,
    /// The child ended but its result was missing, unreadable, unparsable, or
    /// from another version.
    Unreadable,
    /// No time was left to start a child at all.
    NoTime,
}

impl ScanFailure {
    fn message(&self) -> String {
        // One ending for every cause: the reason is the start of the sentence.
        const TAIL: &str = ", so no CLAP effects were listed.";
        match self {
            Self::TimedOut => format!("The CLAP scan did not finish in time{TAIL}"),
            Self::Abandoned => {
                format!("The CLAP scan abandoned a plugin that would not return{TAIL}")
            }
            Self::Unwritable => format!("The CLAP scan could not write its result{TAIL}"),
            Self::Exited(status) => format!("The CLAP scan ended unexpectedly ({status}){TAIL}"),
            Self::Spawn => format!("The CLAP scan process could not be started{TAIL}"),
            Self::Unreadable => format!("The CLAP scan produced no readable result{TAIL}"),
            Self::NoTime => format!("The CLAP scan ran out of time before it could start{TAIL}"),
        }
    }
}

/// One row per scan root, so the browser shows where CLAP was looked for.
pub(super) fn failure_rows(roots: &[PathBuf], cause: &ScanFailure) -> Vec<ClapCandidate> {
    roots
        .iter()
        .map(|root| ClapCandidate {
            path: root.clone(),
            plugin_id: String::new(),
            name: root.display().to_string(),
            vendor: String::new(),
            category: "CLAP effect".to_owned(),
            features: Vec::new(),
            scan_error: Some(cause.message()),
        })
        .collect()
}

/// One row for a bundle that cannot be read.  Split from the root walk
/// so a test can scan a folder it filled.
pub(super) fn scan_bundles(mut bundles: Vec<PathBuf>) -> Vec<ClapCandidate> {
    bundles.sort();
    bundles.dedup();

    let mut found = Vec::new();
    for bundle in bundles {
        match scan_bundle(&bundle) {
            Ok(mut candidates) => found.append(&mut candidates),
            // A bundle that cannot be read has no descriptor.  This row is
            // all the browser can show.
            Err(error) => found.push(unreadable_bundle(&bundle, error)),
        }
    }
    found.sort_by_cached_key(|candidate| candidate.name.to_lowercase());
    found.dedup_by(|a, b| {
        a.path == b.path && a.plugin_id == b.plugin_id && a.name.eq_ignore_ascii_case(&b.name)
    });
    found
}

/// The module did not load, or panicked.
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

/// Shares the module with a loaded instance: one already in use is
/// neither initialised again nor deinitialised when the scan finishes.
///
/// A panic while reading is a failed bundle.  `deinit` on drop cannot
/// unwind the scan, so rows already found are still returned.  A panic
/// the module turns into `abort` at `extern "C"` is not caught here.
pub(crate) fn scan_bundle(bundle: &Path) -> Result<Vec<ClapCandidate>, String> {
    // Each step is inside `contained`: a host-side panic becomes a scan
    // error instead of an unwind that loses the rows already found.
    let module = contained(|| acquire_module(bundle))??;
    let found = contained(|| scan_module(bundle, &module));
    let _ = contained(move || drop(module));
    found
}

/// The prefix of a contained panic, and the whole error when the
/// payload has no text.
pub(super) const PANIC_WHILE_READ: &str = "The bundle panicked while it was read.";

/// A Rust panic becomes a scan error.
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
