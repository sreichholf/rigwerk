//! LV2 bundle discovery and the RDF a bundle declares.
//!
//! Nothing here loads a plugin binary.  A bundle is a folder whose name ends
//! in `.lv2` directly under a root; its `manifest.ttl` names the plugins and
//! links the files that describe them.  A plugin is listed when it has an
//! audio input and an audio output, and only when its `lv2:binary` exists
//! with this platform's extension, so a bundle built for another system is
//! left out as the VST3 scan leaves out one without a loadable binary.  An
//! `lv2:isSideChain` port is not a main input or output: it neither lists a
//! plugin nor counts as a channel.
//!
//! The `lv2:EffectPlugin` class never decides what is listed.  A plugin that
//! needs something RigWerk does not supply is still listed, with `scan_error`
//! saying what is missing; only the class joins the category line.  That
//! error is the loader's own: `PortTable::check` runs every rule the loader
//! reads from the Turtle (features, options, a default state to load, and
//! every port), and the binary is the one `module::native_binary` would
//! pick, so the scan and the loader refuse the same plugins.  Only what the
//! binary itself does is left to the load.

use std::path::{Path, PathBuf};

use super::module;
use super::ports::{has_property, PortTable};
use super::ttl::{self, TtlStore};

const LV2_PLUGIN: &str = "http://lv2plug.in/ns/lv2core#Plugin";
const LV2_INPUT_PORT: &str = "http://lv2plug.in/ns/lv2core#InputPort";
const LV2_OUTPUT_PORT: &str = "http://lv2plug.in/ns/lv2core#OutputPort";
const LV2_AUDIO_PORT: &str = "http://lv2plug.in/ns/lv2core#AudioPort";
const LV2_IS_SIDE_CHAIN: &str = "http://lv2plug.in/ns/lv2core#isSideChain";
const LV2_PORT: &str = "http://lv2plug.in/ns/lv2core#port";

const RDFS_SEE_ALSO: &str = "http://www.w3.org/2000/01/rdf-schema#seeAlso";
const DOAP_NAME: &str = "http://usefulinc.com/ns/doap#name";
const DOAP_MAINTAINER: &str = "http://usefulinc.com/ns/doap#maintainer";
const FOAF_NAME: &str = "http://xmlns.com/foaf/0.1/name";

/// The binary extension this build can open.  A bundle that only ships
/// another platform's binary is left out, not listed with an error.
#[cfg(target_os = "windows")]
pub(super) const PLATFORM_BINARY_EXTENSION: &str = "dll";
#[cfg(not(target_os = "windows"))]
pub(super) const PLATFORM_BINARY_EXTENSION: &str = "so";

/// One discovered LV2 effect.
#[derive(Clone, Debug)]
pub(crate) struct Lv2Candidate {
    /// The bundle folder, which is what the loader opens.
    pub(crate) path: PathBuf,
    pub(crate) plugin_uri: String,
    pub(crate) name: String,
    pub(crate) vendor: String,
    pub(crate) category: String,
    /// The number of main (not side-chain) audio inputs when that is 1 or 2.
    pub(crate) channels: Option<u32>,
    /// Why the scan expects the load to fail, when it does.  The row is
    /// still listed so the browser can say what is missing.
    pub(crate) scan_error: Option<String>,
}

/// The LV2 folders a scan reads.  `LV2_PATH` replaces the defaults, as the
/// convention says; `RIGWERK_LV2_PATH` is always appended.
pub(crate) fn scan_roots() -> Vec<PathBuf> {
    roots_from(
        std::env::var_os("LV2_PATH").as_deref(),
        std::env::var_os("RIGWERK_LV2_PATH").as_deref(),
        default_roots(),
    )
}

/// The roots a scan reads, from the two environment values and the platform
/// defaults, with duplicates dropped in the order they were named.
pub(super) fn roots_from(
    lv2_path: Option<&std::ffi::OsStr>,
    rigwerk_path: Option<&std::ffi::OsStr>,
    defaults: Vec<PathBuf>,
) -> Vec<PathBuf> {
    let mut roots = match lv2_path {
        Some(list) => non_empty_paths(list),
        None => defaults,
    };
    if let Some(list) = rigwerk_path {
        roots.extend(non_empty_paths(list));
    }
    let mut unique: Vec<PathBuf> = Vec::with_capacity(roots.len());
    for root in roots {
        if !unique.contains(&root) {
            unique.push(root);
        }
    }
    unique
}

fn non_empty_paths(list: &std::ffi::OsStr) -> Vec<PathBuf> {
    std::env::split_paths(list)
        .filter(|path| !path.as_os_str().is_empty())
        .collect()
}

fn default_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    #[cfg(target_os = "linux")]
    {
        if let Some(home) = std::env::var_os("HOME") {
            roots.push(PathBuf::from(home).join(".lv2"));
        }
        roots.push(PathBuf::from("/usr/local/lib/lv2"));
        roots.push(PathBuf::from("/usr/lib/lv2"));
        roots.push(PathBuf::from("/usr/lib64/lv2"));
        if let Some(multiarch) = multiarch_root() {
            roots.push(multiarch);
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            roots.push(PathBuf::from(appdata).join("LV2"));
        }
        if let Some(common) = std::env::var_os("CommonProgramFiles") {
            roots.push(PathBuf::from(common).join("LV2"));
        }
    }
    roots
}

/// Debian and Fedora install some bundles under the multiarch triplet.
#[cfg(target_os = "linux")]
fn multiarch_root() -> Option<PathBuf> {
    let triplet = match std::env::consts::ARCH {
        "x86_64" => "x86_64-linux-gnu",
        "aarch64" => "aarch64-linux-gnu",
        "x86" => "i386-linux-gnu",
        "arm" => "arm-linux-gnueabihf",
        "riscv64" => "riscv64-linux-gnu",
        "s390x" => "s390x-linux-gnu",
        "loongarch64" => "loongarch64-linux-gnu",
        _ => return None,
    };
    let root = PathBuf::from(format!("/usr/lib/{triplet}/lv2"));
    root.is_dir().then_some(root)
}

/// Every bundle directly under every root, sorted by name.
pub(crate) fn scan_library() -> Vec<Lv2Candidate> {
    let mut bundles = Vec::new();
    for root in scan_roots() {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let is_bundle = path.is_dir()
                && path
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("lv2"));
            if is_bundle {
                bundles.push(path);
            }
        }
    }
    bundles.sort();
    bundles.dedup();

    let mut found = Vec::new();
    for bundle in bundles {
        match scan_bundle(&bundle) {
            Ok(mut candidates) => found.append(&mut candidates),
            Err(error) => eprintln!("[audio-bridge] skipping {}: {error}", bundle.display()),
        }
    }
    found.sort_by_cached_key(|candidate| candidate.name.to_lowercase());
    found.dedup_by(|a, b| {
        a.path == b.path && a.plugin_uri == b.plugin_uri && a.name.eq_ignore_ascii_case(&b.name)
    });
    found
}

/// Parses a bundle's `manifest.ttl` and every `rdfs:seeAlso` file its plugins
/// name, in file order, and returns the store with the plugin the manifest
/// declares.  The loader and the scan share this so a plugin's description
/// is assembled the same way in both.
pub(super) fn read_bundle(bundle: &Path) -> Result<(TtlStore, Vec<String>), String> {
    let manifest = bundle.join("manifest.ttl");
    let mut store = TtlStore::new();
    store.load_file(&manifest)?;
    let plugins = store.subjects_of_type(LV2_PLUGIN);
    let mut loaded = vec![ttl::file_uri(&manifest)];
    for plugin in &plugins {
        for link in store.iris(plugin, RDFS_SEE_ALSO) {
            let Ok(path) = ttl::path_from_file_uri(&link) else {
                continue;
            };
            let uri = ttl::file_uri(&path);
            if loaded.contains(&uri) {
                continue;
            }
            store.load_file(&path)?;
            loaded.push(uri);
        }
    }
    Ok((store, plugins))
}

/// The effects one bundle lists.  A file the manifest needs but cannot read
/// is an error for the whole bundle, so the caller logs one line and skips
/// it; a `rdfs:seeAlso` that is not a file URI is ignored.
pub(crate) fn scan_bundle(bundle: &Path) -> Result<Vec<Lv2Candidate>, String> {
    let (store, plugins) = read_bundle(bundle)?;
    let mut found: Vec<Lv2Candidate> = plugins
        .iter()
        .filter_map(|plugin| candidate(bundle, &store, &plugins, plugin))
        .collect();
    found.sort_by_cached_key(|candidate| candidate.name.to_lowercase());
    Ok(found)
}

fn candidate(
    bundle: &Path,
    store: &TtlStore,
    plugins: &[String],
    plugin: &str,
) -> Option<Lv2Candidate> {
    // The loader's own choice of binary, without opening it.
    module::native_binary(store, plugin, bundle).ok()?;
    let ports = read_ports(store, plugin);
    let main_inputs = ports
        .iter()
        .filter(|port| port.is_main() && port.input)
        .count();
    if main_inputs == 0 || !ports.iter().any(|port| port.is_main() && port.output) {
        return None;
    }
    let name = store
        .literal(plugin, DOAP_NAME)
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| {
            bundle
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("LV2 plugin")
                .to_owned()
        });
    Some(Lv2Candidate {
        path: bundle.to_path_buf(),
        plugin_uri: plugin.to_owned(),
        name,
        vendor: vendor(store, plugin),
        category: category(store, plugin),
        channels: match main_inputs {
            1 | 2 => Some(main_inputs as u32),
            _ => None,
        },
        scan_error: PortTable::check(store, plugins, plugin),
    })
}

/// One `lv2:port` as the scan reads it to decide whether a plugin is an
/// effect.  Whether the loader can connect it is `PortTable::check`'s
/// business, not this one's.
struct Port {
    input: bool,
    output: bool,
    audio: bool,
    side_chain: bool,
}

impl Port {
    /// An audio port of the main signal chain, not a side chain.
    fn is_main(&self) -> bool {
        self.audio && !self.side_chain
    }
}

fn read_ports(store: &TtlStore, plugin: &str) -> Vec<Port> {
    store
        .iris(plugin, LV2_PORT)
        .into_iter()
        .map(|port| {
            let types = store.types(&port);
            let has = |kind: &str| types.iter().any(|declared| declared == kind);
            Port {
                input: has(LV2_INPUT_PORT),
                output: has(LV2_OUTPUT_PORT),
                audio: has(LV2_AUDIO_PORT),
                side_chain: has_property(store, &port, LV2_IS_SIDE_CHAIN),
            }
        })
        .collect()
}

fn vendor(store: &TtlStore, plugin: &str) -> String {
    let Some(maintainer) = store.iri(plugin, DOAP_MAINTAINER) else {
        return String::new();
    };
    store
        .literal(&maintainer, FOAF_NAME)
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| local_name(maintainer.trim_end_matches('/')).to_owned())
}

/// The plugin's classes, without `lv2:Plugin`, as their shortened local
/// names.  A `lv2:DelayPlugin` reads "Delay"; with no class it is an
/// "LV2 effect".
fn category(store: &TtlStore, plugin: &str) -> String {
    let classes: Vec<String> = store
        .types(plugin)
        .iter()
        .filter(|kind| kind.as_str() != LV2_PLUGIN)
        .map(|kind| {
            let local = local_name(kind);
            local.strip_suffix("Plugin").unwrap_or(local).to_owned()
        })
        .filter(|name| !name.is_empty())
        .collect();
    if classes.is_empty() {
        "LV2 effect".to_owned()
    } else {
        classes.join(" · ")
    }
}

fn local_name(uri: &str) -> &str {
    uri.rsplit(['#', '/']).next().unwrap_or(uri)
}
