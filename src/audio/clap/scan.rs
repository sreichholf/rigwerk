//! CLAP bundle discovery and descriptor scanning.  Unlike VST3 and LV2,
//! listing a CLAP bundle loads its module and calls the bundle's own `init`
//! in this process, so this is the only scan that runs third-party code
//! just to say what a bundle holds.  The browser therefore runs it in a
//! child process ([`scan_library_isolated`]): a hung `init` or an `abort`
//! inside a bundle costs that child, not the app and its module lock.

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
/// One discovered CLAP plugin, or one bundle that could not be read.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
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

/// The version of the `--clap-scan` result file.  A parent that finds another
/// version cannot trust the fields it names, so it treats the scan as failed;
/// bump this when `ClapCandidate` changes shape.
const SCAN_FILE_VERSION: u32 = 1;

/// What the child writes for the parent: a version tag and the scan.  Reading
/// decodes the whole file first and then refuses an unknown version, so the
/// shape the parent trusts is the one this build wrote.
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

/// Encodes a scan result for the parent (`--clap-scan`).
pub(super) fn encode_scan(candidates: Vec<ClapCandidate>) -> Result<String, ron::Error> {
    ron::ser::to_string(&ScanFile::new(candidates))
}

/// The rows a child can put on the wire.  The file carries a path as text, so
/// a bundle whose path is not valid UTF-8 cannot cross as itself.  It becomes
/// one error row rather than failing the encode, which would hide every other
/// bundle in the scan.
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

/// The candidates in a child's file, or `None` when the text does not parse or
/// was written by another version.
pub(super) fn decode_scan(text: &str) -> Option<Vec<ClapCandidate>> {
    let file: ScanFile = ron::from_str(text).ok()?;
    (file.version == SCAN_FILE_VERSION).then_some(file.candidates)
}

/// The child half of the isolated scan (`--clap-scan <file> <limit-ms>`):
/// writes a scan result where the parent reads it.  The file is created with
/// `create_new`, so an existing path is never overwritten; `Err` says why the
/// child cannot report, and the parent then goes by the exit status.
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

/// The child's exit code when it could not write its result.
pub(crate) const CHILD_EXIT_UNWRITABLE: i32 = 1;

/// The child's exit code when its own watchdog ended it, because a bundle held
/// it past every deadline the parent has.
pub(crate) const CHILD_EXIT_ABANDONED: i32 = 3;

/// Scans the CLAP library in a child process of this binary.
///
/// A child is what keeps a bundle's own code away from this process: an `init`
/// that never returns holds the child's module lock, not this process's, and
/// an `abort` in the bundle ends only the child.  `timeout` is the parent's
/// whole budget for the child: when it passes the child is killed and one row
/// per scan root reports why there are no results.  A zero timeout starts no
/// child at all.  Nothing here touches the parent's module registry, so a scan
/// and a load can run side by side.
pub(crate) fn scan_library_isolated(timeout: Duration) -> Vec<ClapCandidate> {
    let roots = scan_roots();
    if timeout.is_zero() {
        return resolve(Err(ScanFailure::NoTime), &roots);
    }
    resolve(run_scan_child(timeout), &roots)
}

/// Turns a child result into rows: a scan that produced candidates passes them
/// through unchanged, a failure becomes one row per scan root.  Pure, so a test
/// can drive both arms without starting a process.
pub(super) fn resolve(
    result: Result<Vec<ClapCandidate>, ScanFailure>,
    roots: &[PathBuf],
) -> Vec<ClapCandidate> {
    match result {
        Ok(candidates) => candidates,
        Err(cause) => failure_rows(roots, &cause),
    }
}

/// A folder for one child scan's result, created before the child starts and
/// removed with the file it holds.
///
/// The name is predictable and the temp directory is shared, so the file must
/// not sit there on its own: the folder is this scan's alone, it is created
/// with mode 0700 on Unix, and the file inside it is opened with `create_new`.
/// Windows' temp directory is already user-scoped, so the same code path holds
/// there.
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
        // A child the parent gave up on may still be writing, so the folder is
        // removed with anything else the late write left in it.
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The pid, a counter and the clock together keep two scans in one process and
/// two processes apart.  A name that is already taken is skipped rather than
/// reused, so a scan never writes into a folder it did not create.
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

/// Creates the result folder with no group or other access on Unix.
#[cfg(unix)]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    std::fs::DirBuilder::new().mode(0o700).create(dir)
}

/// Windows' temp directory is already user-scoped, so the default mode holds.
#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir(dir)
}

/// How often the parent checks on a child.  Short enough that a quick scan
/// comes back quickly, long enough not to spin a core.
const SCAN_POLL: Duration = Duration::from_millis(25);

/// How much longer the child may run than the parent waits.  The parent kills
/// a child that overruns, but the parent may itself be killed or its kill may
/// fail, so the child's own watchdog ends it at this point instead of leaving
/// it to load plugins for ever.
const CHILD_GRACE: Duration = Duration::from_secs(5);

/// How long the parent waits for a killed child to be reaped before dropping
/// the handle.  `kill` can fail, and `wait` would then block until the child
/// ends by itself, which is the hang this guards against.
const KILL_SETTLE: Duration = Duration::from_secs(2);

/// Runs `--clap-scan` from this binary and reads what it wrote.
///
/// stdin, stdout and stderr are null rather than pipes: the release build is a
/// GUI-subsystem process without a console, and a pipe would have to be
/// drained.  The file is the only result, and [`ScanResult`] removes it and its
/// folder on every path.
///
/// The spawn is not covered by a unit test: under `cargo test` the running
/// binary is the test harness, and `--clap-scan` would be taken by libtest
/// instead of by `main`.  Only a real `rigwerk` launch exercises the child.
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

/// Ends a child the parent has given up on.  The kill is best effort, and the
/// wait for it to be reaped is bounded too: if the child is not gone within
/// [`KILL_SETTLE`] the handle is dropped and this process carries on, leaving
/// the child to the OS rather than hanging on it.
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

/// Why an isolated scan gave no candidates.  The exit status of a cause this
/// build does not know is kept as text, not as an `ExitStatus`, so
/// [`failure_rows`] stays a pure function a test can call with a made-up cause.
#[derive(Debug)]
pub(super) enum ScanFailure {
    /// The parent's budget passed; the child was killed.
    TimedOut,
    /// The child's own watchdog ended it: a bundle would not return.
    Abandoned,
    /// The child could not write its result.
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

/// One row per scan root when the scan gave nothing back, so the browser shows
/// where CLAP was looked for and why those folders are unknown.
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
