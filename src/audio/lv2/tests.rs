use std::path::PathBuf;

use super::scan::scan_bundle;
use super::ttl::{file_uri, path_from_file_uri, TtlStore};

const LV2_PLUGIN: &str = "http://lv2plug.in/ns/lv2core#Plugin";
const LV2_BINARY: &str = "http://lv2plug.in/ns/lv2core#binary";
const LV2_PORT: &str = "http://lv2plug.in/ns/lv2core#port";
const LV2_SYMBOL: &str = "http://lv2plug.in/ns/lv2core#symbol";
const DOAP_NAME: &str = "http://usefulinc.com/ns/doap#name";

fn testdata(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src/audio/lv2/testdata")
        .join(name)
}

#[test]
fn a_manifest_and_its_see_also_files_are_read_with_prefixes_and_relative_iris() {
    let bundle = testdata("example.lv2");
    let found = scan_bundle(&bundle).expect("scan the example bundle");
    let rows: Vec<_> = found
        .iter()
        .map(|candidate| {
            (
                candidate.plugin_uri.as_str(),
                candidate.name.as_str(),
                candidate.vendor.as_str(),
                candidate.category.as_str(),
                candidate.channels,
                candidate.scan_error.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            (
                "http://example.com/lv2/example",
                "Example Amp",
                "Jane Doe",
                "Effect · Amplifier",
                Some(2),
                None
            ),
            (
                "http://example.com/lv2/tone",
                "Tone",
                "john",
                "Filter",
                Some(1),
                None
            ),
        ],
        "the seeAlso file was read; the maintainer falls back to its IRI"
    );
    assert!(found.iter().all(|candidate| candidate.path == bundle));
}

#[test]
fn an_instrument_without_an_audio_input_is_left_out() {
    let found = scan_bundle(&testdata("example.lv2")).expect("scan the example bundle");
    assert!(
        found
            .iter()
            .all(|candidate| candidate.plugin_uri != "http://example.com/lv2/synth"),
        "the instrument was listed"
    );
}

#[test]
fn a_fallback_name_comes_from_the_bundle_stem() {
    let found = scan_bundle(&testdata("nameless.lv2")).expect("scan");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].name, "nameless");
    assert_eq!(found[0].vendor, "");
    assert_eq!(found[0].category, "Delay");
}

#[test]
fn an_unsupported_required_feature_is_listed_with_its_error() {
    let found = scan_bundle(&testdata("fixed-block.lv2")).expect("scan");
    assert_eq!(found.len(), 1);
    assert_eq!(
        found[0].scan_error.as_deref(),
        Some("Needs a fixed block length")
    );
}

#[test]
fn a_cv_port_that_is_not_optional_is_listed_with_its_error() {
    let found = scan_bundle(&testdata("cv-port.lv2")).expect("scan");
    assert_eq!(found.len(), 1);
    let error = found[0].scan_error.as_deref().expect("a scan error");
    assert!(error.contains("CV port"), "{error}");
    assert!(error.contains("Vibrato"), "{error}");
}

#[test]
fn a_relative_bundle_path_still_lists_its_effects() {
    // Cargo runs a test binary from the package root, so this path exists
    // relative to it; the binary check must not depend on an absolute input.
    let relative = std::path::Path::new("src/audio/lv2/testdata/example.lv2");
    let found = scan_bundle(relative).expect("scan a relative bundle path");
    let names: Vec<&str> = found
        .iter()
        .map(|candidate| candidate.name.as_str())
        .collect();
    assert_eq!(names, ["Example Amp", "Tone"]);
    assert!(
        found.iter().all(|candidate| candidate.path == relative),
        "the candidate keeps the caller's path"
    );
}

#[test]
fn the_native_binary_is_picked_even_when_another_extension_is_named_first() {
    let found = scan_bundle(&testdata("binary-order.lv2")).expect("scan");
    let names: Vec<&str> = found
        .iter()
        .map(|candidate| candidate.name.as_str())
        .collect();
    assert_eq!(
        names,
        ["DLL First", "SO First"],
        "both plugins name one foreign and one native binary"
    );
}

#[test]
fn a_binary_for_another_platform_is_left_out() {
    #[cfg(target_os = "windows")]
    let (foreign, native) = (testdata("so-only.lv2"), testdata("dll-only.lv2"));
    #[cfg(not(target_os = "windows"))]
    let (foreign, native) = (testdata("dll-only.lv2"), testdata("so-only.lv2"));
    assert!(
        scan_bundle(&foreign).expect("scan").is_empty(),
        "a bundle without this platform's binary must not be listed"
    );
    assert_eq!(
        scan_bundle(&native).expect("scan").len(),
        1,
        "the same bundle with this platform's binary is listed"
    );
}

#[test]
fn a_file_uri_becomes_a_windows_drive_path() {
    let path = path_from_file_uri("file:///C:/Program%20Files/LV2/example.lv2/plugin.dll")
        .expect("a drive-letter file URI");
    assert_eq!(
        path,
        PathBuf::from("C:/Program Files/LV2/example.lv2/plugin.dll")
    );
}

#[cfg(not(target_os = "windows"))]
#[test]
fn a_file_uri_round_trips_through_its_path() {
    let path = std::path::absolute("/usr/lib/lv2/My Bundle.lv2/manifest.ttl").expect("absolute");
    let uri = file_uri(&path);
    assert_eq!(uri, "file:///usr/lib/lv2/My%20Bundle.lv2/manifest.ttl");
    assert_eq!(path_from_file_uri(&uri).expect("a file URI"), path);
}

#[test]
fn a_path_with_spaces_and_a_hash_round_trips() {
    let path = std::env::temp_dir()
        .join("RigWerk LV2 #1.lv2")
        .join("manifest.ttl");
    let path = std::path::absolute(path).expect("absolute");
    let uri = file_uri(&path);
    assert!(uri.starts_with("file:///"), "{uri}");
    assert!(uri.contains("RigWerk%20LV2%20%231.lv2"), "{uri}");
    assert_eq!(path_from_file_uri(&uri).expect("a file URI"), path);
}

#[test]
fn a_uri_that_is_not_a_file_is_refused() {
    assert!(path_from_file_uri("https://example.com/plugin.ttl").is_err());
    assert!(path_from_file_uri("plugin.ttl").is_err());
    assert!(path_from_file_uri("file://server/share/plugin.ttl").is_err());
    assert!(path_from_file_uri("file:///bad%zz.ttl").is_err());
}

#[test]
fn a_store_appends_files_and_answers_by_subject_and_predicate() {
    let bundle = testdata("example.lv2");
    let mut store = TtlStore::new();
    store
        .load_file(&bundle.join("manifest.ttl"))
        .expect("manifest");
    assert_eq!(
        store.subjects_of_type(LV2_PLUGIN),
        [
            "http://example.com/lv2/example",
            "http://example.com/lv2/tone",
            "http://example.com/lv2/synth",
        ]
    );
    store
        .load_file(&bundle.join("plugin.ttl"))
        .expect("see also");

    let amp = "http://example.com/lv2/example";
    assert_eq!(store.literals(amp, DOAP_NAME), ["Example Amp"]);
    let ports = store.iris(amp, LV2_PORT);
    assert_eq!(ports.len(), 6);
    assert_eq!(store.literals(&ports[0], LV2_SYMBOL), ["in_l"]);
    assert!(store
        .types(amp)
        .contains(&"http://lv2plug.in/ns/lv2core#AmplifierPlugin".to_owned()));
    // The relative binaries resolved against the file that names them, in
    // file order.
    assert_eq!(
        store.iris(amp, LV2_BINARY),
        [
            file_uri(&bundle.join("example.so")),
            file_uri(&bundle.join("example.dll")),
        ]
    );
}

#[test]
fn blank_node_labels_stay_within_their_file() {
    const SCHEMA: &str = "http://example.com/schema#";
    let mut store = TtlStore::new();
    store
        .load_file(&testdata("blank-a.ttl"))
        .expect("first file");
    store
        .load_file(&testdata("blank-b.ttl"))
        .expect("second file");
    let port = |subject: &str| {
        store
            .iri(subject, &format!("{SCHEMA}port"))
            .expect("a port")
    };
    let (a, b) = (port("http://example.com/a"), port("http://example.com/b"));
    assert_ne!(a, b, "_:p1 in two files is two nodes");
    assert_eq!(store.literals(&a, &format!("{SCHEMA}name")), ["A"]);
    assert_eq!(store.literals(&b, &format!("{SCHEMA}name")), ["B"]);
}

#[test]
fn lv2_path_replaces_the_defaults_and_rigwerk_is_always_appended() {
    use super::scan::roots_from;

    let defaults = vec![PathBuf::from("/default/lv2")];
    let lv2 = std::env::join_paths(["/lv2-a", "/lv2-b"]).expect("join");
    let rigwerk = std::env::join_paths(["/rigwerk"]).expect("join");
    assert_eq!(
        roots_from(Some(&lv2), Some(&rigwerk), defaults.clone()),
        [
            PathBuf::from("/lv2-a"),
            PathBuf::from("/lv2-b"),
            PathBuf::from("/rigwerk"),
        ]
    );
    assert_eq!(
        roots_from(None, Some(&rigwerk), defaults.clone()),
        [PathBuf::from("/default/lv2"), PathBuf::from("/rigwerk")]
    );
    assert_eq!(roots_from(None, None, defaults.clone()), defaults);
    // A duplicate is kept once, in the order it was first named.
    let duplicate = std::env::join_paths(["/rigwerk", "/lv2-a"]).expect("join");
    assert_eq!(
        roots_from(
            Some(std::ffi::OsStr::new("/lv2-a")),
            Some(&duplicate),
            defaults
        ),
        [PathBuf::from("/lv2-a"), PathBuf::from("/rigwerk")]
    );
}
