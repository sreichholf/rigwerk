use std::ffi::{c_void, CStr, CString};
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use lv2_sys::{
    LV2_Descriptor, LV2_Feature, LV2_Handle,
    LV2_Options_Context_LV2_OPTIONS_INSTANCE as LV2_OPTIONS_INSTANCE, LV2_State_Flags,
    LV2_State_Interface, LV2_State_Retrieve_Function, LV2_State_Status,
    LV2_State_Status_LV2_STATE_ERR_BAD_FLAGS as LV2_STATE_ERR_BAD_FLAGS,
    LV2_State_Status_LV2_STATE_ERR_BAD_TYPE as LV2_STATE_ERR_BAD_TYPE,
    LV2_State_Status_LV2_STATE_ERR_NO_FEATURE as LV2_STATE_ERR_NO_FEATURE,
    LV2_State_Status_LV2_STATE_ERR_NO_SPACE as LV2_STATE_ERR_NO_SPACE,
    LV2_State_Status_LV2_STATE_ERR_UNKNOWN as LV2_STATE_ERR_UNKNOWN,
    LV2_State_Status_LV2_STATE_SUCCESS as LV2_STATE_SUCCESS, LV2_State_Store_Function,
    LV2_Worker_Interface, LV2_Worker_Respond_Function, LV2_Worker_Respond_Handle,
    LV2_Worker_Schedule, LV2_Worker_Status,
    LV2_Worker_Status_LV2_WORKER_ERR_NO_SPACE as LV2_WORKER_ERR_NO_SPACE,
    LV2_Worker_Status_LV2_WORKER_ERR_UNKNOWN as LV2_WORKER_ERR_UNKNOWN,
    LV2_Worker_Status_LV2_WORKER_SUCCESS as LV2_WORKER_SUCCESS,
};

use crate::audio::clap::StateSave;
use crate::audio::plugins::{
    lv2_fixture, param_transfer, AudioThreadScope, LoadedPlugin, Lv2Failure, Outcome, ParamIds,
    ParamOut, PluginFormat, PluginPref, PluginShared, PluginState, Restore, Saved, MAX_BLOCK,
    MAX_STATE_BLOB,
};

use super::host::{used_uris, HostFeatures, SATISFIED, SUPPORTED};
use super::instance::{Lv2Core, Lv2Instance, Lv2Loaded};
use super::module::{native_binary, resolve, Descriptors, Lv2Module};
use super::ports::{
    symbol_id, Buffer, PortBuffers, PortInfo, PortKind, PortTable, ScalePoint, ATOM_CAPACITY,
};
use super::processor::{Lv2Processor, WorkerCallbacks};
use super::scan::{read_bundle, scan_bundle};
use super::state;
use super::ttl::{file_uri, lexical, path_from_file_uri, TtlStore};
use super::urid;
use super::worker::{
    hold_gate, loops_for_test, register, registered_for_test, respond, schedule_struct, unregister,
    wake, WorkerRings, RING_BYTES, TEARDOWN_ROUNDS,
};

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

/// A state status as the `i32` the tests store it in: the type is `u32` on
/// Linux and `i32` on Windows.
#[allow(clippy::unnecessary_cast)]
fn status_bits(status: LV2_State_Status) -> i32 {
    status as i32
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

/// `lv2:inPlaceBroken`, `lv2:isLive` and `lv2:hardRTCapable` are met by
/// how the host runs a plugin, so requiring them lists the plugin cleanly.
#[test]
fn required_features_the_host_meets_without_data_are_accepted() {
    let found = scan_bundle(&testdata("satisfied.lv2")).expect("scan");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].scan_error, None);
}

/// `opts:requiredOption`: an option the host does not supply blocks the
/// plugin in the scan and at load; the ones it supplies do not.
#[test]
fn a_required_option_the_host_does_not_supply_is_an_error() {
    let bundle = testdata("required-option.lv2");
    let found = scan_bundle(&bundle).expect("scan");
    assert_eq!(found.len(), 1);
    assert_eq!(
        found[0].scan_error.as_deref(),
        Some("Needs the option nominalBlockLength")
    );
    let error = PortTable::read(&bundle, "http://example.com/lv2/required-option", 48_000)
        .err()
        .expect("the loader refused the plugin");
    assert_eq!(error, "Needs the option nominalBlockLength");
    // satisfied.lv2 requires only options the host supplies.
    assert!(PortTable::read(
        &testdata("satisfied.lv2"),
        "http://example.com/lv2/satisfied",
        48_000
    )
    .is_ok());
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
fn a_file_uri_resolves_dot_segments_lexically() {
    let path = path_from_file_uri("file:///lv2/amp.lv2/../elsewhere/./x.so").expect("a path");
    assert_eq!(path, PathBuf::from("/lv2/elsewhere/x.so"));
    assert_eq!(
        lexical(std::path::Path::new("/../a/b/..")),
        PathBuf::from("/a")
    );
}

/// A binary named as `bundle/../elsewhere` is outside the bundle even though
/// its text starts with the bundle's path: neither the scan nor the loader
/// may pick it.
#[test]
fn a_binary_that_climbs_out_of_the_bundle_is_refused() {
    let base = std::env::temp_dir().join(format!("rigwerk-lv2-climb-{}", std::process::id()));
    let bundle = base.join("inside.lv2");
    let outside = base.join("outside");
    std::fs::create_dir_all(&bundle).expect("create the bundle");
    std::fs::create_dir_all(&outside).expect("create the outside folder");
    let binary = format!("x.{}", super::scan::PLATFORM_BINARY_EXTENSION);
    std::fs::write(outside.join(&binary), b"").expect("write the outside binary");
    let climbing = format!("{}/../outside/{binary}", file_uri(&bundle));
    std::fs::write(
        bundle.join("manifest.ttl"),
        format!(
            r#"@prefix lv2: <http://lv2plug.in/ns/lv2core#> .
<http://example.com/lv2/climb>
    a lv2:Plugin ;
    lv2:binary <{climbing}> ;
    lv2:port
        [ a lv2:InputPort, lv2:AudioPort ; lv2:index 0 ; lv2:symbol "in" ],
        [ a lv2:OutputPort, lv2:AudioPort ; lv2:index 1 ; lv2:symbol "out" ] .
"#
        ),
    )
    .expect("write the manifest");

    let found = scan_bundle(&bundle).expect("scan");
    let (store, _) = read_bundle(&bundle).expect("read the bundle");
    let picked = native_binary(&store, "http://example.com/lv2/climb", &bundle);
    let _ = std::fs::remove_dir_all(&base);
    assert!(
        found.is_empty(),
        "the scan listed a binary outside the bundle"
    );
    assert!(picked.is_err(), "the loader picked {picked:?}");
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

#[test]
fn a_urid_round_trips_through_map_and_unmap() {
    let map = urid::global();
    let uri = c"http://rigwerk.test/urid/round-trip";
    let id = map.map(uri);
    assert_ne!(id, 0, "a lookup outside the audio thread maps the URI");
    assert_eq!(map.map(uri), id, "the id is stable across calls");
    assert_eq!(map.uri(id), Some(uri));
    assert_eq!(unsafe { CStr::from_ptr(map.unmap(id)) }, uri);
    assert!(map.unmap(0).is_null());
    assert!(map.unmap(u32::MAX).is_null(), "an unknown id has no string");
    assert!(map.uri(0).is_none());
}

/// `urid.h`: a URI maps to the same id for the life of an instance.  A
/// lookup on the audio thread inserts, so its answer is the one every later
/// lookup gets, whichever thread asks.
#[test]
fn a_urid_first_mapped_on_the_audio_thread_keeps_its_id() {
    let map = urid::global();
    let uri = c"http://rigwerk.test/urid/audio-first";
    assert!(!map.contains_for_test(uri));
    let id = {
        let _audio = AudioThreadScope::enter();
        map.map(uri)
    };
    assert_ne!(id, 0, "the audio thread was refused a new URI");
    assert_eq!(map.map(uri), id, "the id changed off the audio thread");
    assert_eq!(map.uri(id), Some(uri));
}

#[test]
fn an_audio_thread_lookup_answers_an_already_mapped_uri() {
    let map = urid::global();
    let uri = c"http://rigwerk.test/urid/already-mapped";
    let id = map.map(uri);
    assert_ne!(id, 0);
    let _audio = AudioThreadScope::enter();
    assert_eq!(
        map.map(uri),
        id,
        "a known URI still answers on the audio thread"
    );
}

/// Threads that map the same new URIs at once, in different orders, agree
/// on every id, and different URIs never share one.
#[test]
fn concurrent_lookups_of_new_uris_agree() {
    const URIS: usize = 64;
    const THREADS: usize = 8;
    let uris: Arc<Vec<CString>> = Arc::new(
        (0..URIS)
            .map(|index| {
                CString::new(format!("http://rigwerk.test/urid/concurrent/{index}")).expect("a URI")
            })
            .collect(),
    );
    let barrier = Arc::new(std::sync::Barrier::new(THREADS));
    let threads: Vec<_> = (0..THREADS)
        .map(|thread| {
            let uris = Arc::clone(&uris);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let _audio = (thread % 2 == 0).then(AudioThreadScope::enter);
                barrier.wait();
                let mut ids = vec![0; URIS];
                for step in 0..URIS {
                    // Odd threads walk backwards, so they race the others.
                    let index = if thread % 2 == 0 {
                        step
                    } else {
                        URIS - 1 - step
                    };
                    ids[index] = urid::global().map(&uris[index]);
                }
                ids
            })
        })
        .collect();
    let results: Vec<Vec<u32>> = threads
        .into_iter()
        .map(|thread| thread.join().expect("join a lookup thread"))
        .collect();
    let first = &results[0];
    assert!(first.iter().all(|id| *id != 0), "a lookup was refused");
    for ids in &results[1..] {
        assert_eq!(ids, first, "two threads got different ids");
    }
    let mut unique = first.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), URIS, "two URIs share an id");
    for (index, id) in first.iter().enumerate() {
        assert_eq!(urid::global().uri(*id), Some(uris[index].as_c_str()));
    }
}

fn feature_data(host: &HostFeatures, wanted: &CStr) -> *mut c_void {
    for index in 0..16 {
        let feature = unsafe { *host.as_ptr().add(index) };
        if feature.is_null() {
            break;
        }
        let uri = unsafe { CStr::from_ptr((*feature).URI) };
        if uri == wanted {
            return unsafe { (*feature).data };
        }
    }
    panic!("no {wanted:?} feature");
}

fn feature_uris(host: &HostFeatures) -> Vec<String> {
    let mut found = Vec::new();
    for index in 0..16 {
        let feature = unsafe { *host.as_ptr().add(index) };
        if feature.is_null() {
            return found;
        }
        let uri = unsafe { CStr::from_ptr((*feature).URI) };
        found.push(uri.to_string_lossy().into_owned());
    }
    panic!("the feature array is not null-terminated");
}

#[test]
fn every_supported_feature_is_supplied() {
    let rings = Arc::new(WorkerRings::new());
    let host = HostFeatures::new(48000.0, 512, 8192, Some(Arc::clone(&rings)), true);
    let found = feature_uris(&host);
    for uri in SUPPORTED.iter().filter(|uri| !SATISFIED.contains(uri)) {
        assert!(
            found.iter().any(|feature| feature == uri),
            "{uri} is in SUPPORTED but not supplied"
        );
    }
    // The features met without data stay out of the array.
    assert_eq!(
        found.len(),
        SUPPORTED.len() - SATISFIED.len(),
        "an extra feature was supplied: {found:?}"
    );

    // The feature data is the host's own schedule struct, and its handle is
    // the rings the host now owns.
    let data = feature_data(&host, c"http://lv2plug.in/ns/ext/worker#schedule");
    assert!(!data.is_null());
    let schedule = unsafe { &*(data as *const LV2_Worker_Schedule) };
    assert_eq!(schedule.handle, Arc::as_ptr(&rings) as *mut c_void);

    let without_rings = HostFeatures::new(48000.0, 512, 8192, None, false);
    assert!(
        !feature_uris(&without_rings)
            .iter()
            .any(|uri| uri.ends_with("worker#schedule")),
        "worker:schedule appeared without rings"
    );

    let without_default = HostFeatures::new(48000.0, 512, 8192, Some(rings), false);
    assert!(
        !feature_uris(&without_default)
            .iter()
            .any(|uri| uri.contains("state#loadDefaultState")),
        "state:loadDefaultState was supplied when no default state is restored"
    );
}

#[test]
fn host_features_stay_put_when_the_handle_moves() {
    let host = HostFeatures::new(48000.0, 512, 8192, None, false);
    let before = host.as_ptr();
    let moved = host;
    assert_eq!(moved.as_ptr(), before, "the feature array moved");
    unsafe {
        let first = *moved.as_ptr();
        assert!(!first.is_null());
        assert_eq!(
            CStr::from_ptr((*first).URI).to_str().unwrap(),
            "http://lv2plug.in/ns/ext/urid#map"
        );
    }

    // The first option's value points into the host allocation itself, so
    // the test fails if the Box (or the value fields) go away.
    let base = &*moved as *const HostFeatures as usize;
    let end = base + std::mem::size_of::<HostFeatures>();
    let value = moved.options()[0].value as usize;
    assert!(
        (base..end).contains(&value),
        "the option value 0x{value:x} is outside the host at 0x{base:x}..0x{end:x}"
    );
}

#[test]
fn the_urid_seeds_cover_every_uri_the_host_uses() {
    urid::seed();
    let map = urid::global();
    for uri in super::urid::SEEDED {
        assert!(
            map.contains_for_test(uri),
            "{uri:?} is in SEEDED but not in the map"
        );
    }
    for uri in used_uris() {
        assert!(
            super::urid::SEEDED.contains(uri),
            "{uri:?} is used by the host but not seeded"
        );
        assert!(map.contains_for_test(uri), "{uri:?} is not mapped");
    }
}

#[test]
fn the_host_options_carry_the_block_sizes_and_sample_rate() {
    let host = HostFeatures::new(48000.0, 512, 8192, None, false);
    let map = urid::global();
    let key = |uri: &CStr| map.map(uri);
    let options = host.options();
    let find = |uri: &CStr| {
        options
            .iter()
            .find(|option| option.key != 0 && option.key == key(uri))
            .expect("an option")
    };
    let int = key(c"http://lv2plug.in/ns/ext/atom#Int");
    let float = key(c"http://lv2plug.in/ns/ext/atom#Float");

    let min = find(c"http://lv2plug.in/ns/ext/buf-size#minBlockLength");
    assert_eq!(min.context, LV2_OPTIONS_INSTANCE);
    assert_eq!(min.subject, 0);
    assert_eq!(min.size, 4);
    assert_eq!(min.type_, int);
    // The load calls `run(0)`, so the minimum block length is zero.
    assert_eq!(unsafe { *(min.value as *const i32) }, 0);

    for (uri, expected) in [
        (c"http://lv2plug.in/ns/ext/buf-size#maxBlockLength", 512),
        (c"http://lv2plug.in/ns/ext/buf-size#sequenceSize", 8192),
    ] {
        let option = find(uri);
        assert_eq!((option.size, option.type_), (4, int));
        assert_eq!(
            unsafe { *(option.value as *const i32) },
            expected,
            "{uri:?}"
        );
    }

    // The block a plugin runs is the bridge's, which a load does not know,
    // so no nominal length is claimed.
    let nominal = key(c"http://lv2plug.in/ns/ext/buf-size#nominalBlockLength");
    assert!(
        options.iter().all(|option| option.key != nominal),
        "a nominal block length is advertised"
    );

    let rate = find(c"http://lv2plug.in/ns/ext/parameters#sampleRate");
    assert_eq!((rate.size, rate.type_), (4, float));
    assert_eq!(unsafe { *(rate.value as *const f32) }, 48000.0);

    let terminator = options.last().expect("the terminator");
    assert_eq!(terminator.key, 0);
    assert_eq!(terminator.size, 0);
    assert!(terminator.value.is_null());
}

#[test]
fn the_state_path_callbacks_round_trip_and_free() {
    let host = HostFeatures::new(48000.0, 512, 8192, None, false);
    let (map_path, free_path) = host.paths();
    let input = CString::new("/tmp/RigWerk state/ir.wav").expect("a path");
    unsafe {
        let abstracted =
            (map_path.abstract_path.expect("abstract_path"))(map_path.handle, input.as_ptr());
        assert!(!abstracted.is_null());
        assert_eq!(CStr::from_ptr(abstracted), input.as_c_str());
        let absolute =
            (map_path.absolute_path.expect("absolute_path"))(map_path.handle, abstracted);
        assert!(!absolute.is_null());
        assert_eq!(CStr::from_ptr(absolute), input.as_c_str());
        let free = free_path.free_path.expect("free_path");
        free(free_path.handle, abstracted);
        free(free_path.handle, absolute);
        free(free_path.handle, ptr::null_mut());
        assert!(
            (map_path.abstract_path.expect("abstract_path"))(map_path.handle, ptr::null())
                .is_null(),
            "a null path maps to null"
        );
    }
}

#[test]
fn worker_messages_keep_their_order_and_lengths() {
    let rings = WorkerRings::new();
    let messages: [&[u8]; 4] = [b"", b"one", b"a longer message", &[7; 33]];
    for message in messages {
        assert!(rings.push_request(message));
    }
    assert!(rings.request_pending());
    let mut out = vec![0u8; RING_BYTES];
    for message in messages {
        assert_eq!(rings.pop_request(&mut out), Some(message.len()));
        assert_eq!(&out[..message.len()], message);
    }
    assert_eq!(rings.pop_request(&mut out), None);
    assert!(!rings.request_pending());
}

#[test]
fn a_worker_message_that_does_not_fit_is_refused() {
    let rings = WorkerRings::new();
    let too_big = vec![0u8; RING_BYTES];
    assert!(
        !rings.push_request(&too_big),
        "a message larger than the ring"
    );
    assert!(!rings.request_pending());

    let fits = vec![1u8; RING_BYTES - 4];
    assert!(rings.push_request(&fits), "a message that fills the ring");
    let mut out = vec![0u8; RING_BYTES];
    assert_eq!(rings.pop_request(&mut out), Some(RING_BYTES - 4));

    assert!(rings.push_request(&fits));
    assert!(
        !rings.push_request(b"x"),
        "a message with no space left is refused, not truncated"
    );
}

#[test]
fn worker_messages_wrap_around_the_ring() {
    let rings = WorkerRings::new();
    let mut out = vec![0u8; RING_BYTES];
    for round in 0..2000usize {
        let length = (round * 37 % 130) + 1;
        let message: Vec<u8> = (0..length).map(|index| (round + index) as u8).collect();
        assert!(rings.push_request(&message), "round {round}");
        assert_eq!(rings.pop_request(&mut out), Some(length), "round {round}");
        assert_eq!(&out[..length], &message[..], "round {round}");
    }
}

#[test]
fn an_empty_worker_ring_pops_nothing() {
    let rings = WorkerRings::new();
    let mut out = vec![0u8; RING_BYTES];
    assert_eq!(rings.pop_request(&mut out), None);
    assert_eq!(rings.pop_response(&mut out), None);
    assert!(!rings.request_pending());
    assert!(!rings.response_pending());
}

#[test]
fn a_response_that_does_not_fit_is_refused() {
    let rings = WorkerRings::new();
    let too_big = vec![0u8; RING_BYTES];
    assert!(
        !rings.push_response(&too_big),
        "a response larger than the ring"
    );
    assert!(!rings.response_pending());

    let fits = vec![3u8; RING_BYTES - 4];
    assert!(rings.push_response(&fits), "a response that fills the ring");
    assert!(
        !rings.push_response(b"x"),
        "a response with no space left is refused, not truncated"
    );
    let mut out = vec![0u8; RING_BYTES];
    assert_eq!(rings.pop_response(&mut out), Some(RING_BYTES - 4));
    assert_eq!(&out[..RING_BYTES - 4], &fits[..]);
}

#[test]
fn both_worker_directions_can_be_full_at_once() {
    let rings = WorkerRings::new();
    let request = vec![1u8; RING_BYTES - 4];
    let response = vec![2u8; RING_BYTES - 4];
    assert!(rings.push_request(&request));
    assert!(rings.push_response(&response));
    assert!(rings.request_pending());
    assert!(rings.response_pending());
    assert!(!rings.push_request(b"x"));
    assert!(!rings.push_response(b"x"));

    let mut out = vec![0u8; RING_BYTES];
    assert_eq!(rings.pop_response(&mut out), Some(RING_BYTES - 4));
    assert_eq!(&out[..RING_BYTES - 4], &response[..]);
    assert!(
        rings.request_pending(),
        "draining one ring drained the other"
    );
    assert_eq!(rings.pop_request(&mut out), Some(RING_BYTES - 4));
    assert_eq!(&out[..RING_BYTES - 4], &request[..]);
    assert!(!rings.request_pending());
    assert!(!rings.response_pending());
}

#[test]
fn response_messages_keep_their_framing_across_wrap_around() {
    let rings = WorkerRings::new();
    let mut out = vec![0u8; RING_BYTES];
    for round in 0..2000usize {
        let length = (round * 53 % 170) + 1;
        let message: Vec<u8> = (0..length).map(|index| (round * 3 + index) as u8).collect();
        assert!(rings.push_response(&message), "round {round}");
        assert_eq!(rings.pop_response(&mut out), Some(length), "round {round}");
        assert_eq!(&out[..length], &message[..], "round {round}");
    }
    assert!(!rings.response_pending());
}

#[test]
fn the_worker_directions_are_independent() {
    let rings = WorkerRings::new();
    assert!(rings.push_request(b"request"));
    assert!(!rings.response_pending());
    assert!(rings.push_response(b"response"));
    let mut out = vec![0u8; RING_BYTES];
    assert_eq!(rings.pop_response(&mut out), Some(8));
    assert_eq!(&out[..8], b"response");
    assert_eq!(rings.pop_request(&mut out), Some(7));
    assert_eq!(&out[..7], b"request");
}

#[test]
fn the_schedule_callback_reports_success_and_no_space() {
    let rings = Arc::new(WorkerRings::new());
    let schedule = schedule_struct(&rings);
    let schedule_work = schedule.schedule_work.expect("a schedule callback");
    let message = b"load the impulse";
    // Until the load knows the plugin has `work`, nothing is accepted.
    let status = unsafe {
        schedule_work(
            schedule.handle,
            message.len() as u32,
            message.as_ptr() as *const c_void,
        )
    };
    assert_eq!(status, LV2_WORKER_ERR_UNKNOWN);
    assert!(!rings.request_pending());

    rings.accept_requests();
    let status = unsafe {
        schedule_work(
            schedule.handle,
            message.len() as u32,
            message.as_ptr() as *const c_void,
        )
    };
    assert_eq!(status, LV2_WORKER_SUCCESS);
    let mut out = vec![0u8; RING_BYTES];
    assert_eq!(rings.pop_request(&mut out), Some(message.len()));
    assert_eq!(&out[..message.len()], message);

    let too_big = vec![0u8; RING_BYTES];
    let status = unsafe {
        schedule_work(
            schedule.handle,
            too_big.len() as u32,
            too_big.as_ptr() as *const c_void,
        )
    };
    assert_eq!(status, LV2_WORKER_ERR_NO_SPACE);

    let status = unsafe { schedule_work(schedule.handle, 4, ptr::null()) };
    assert_eq!(status, LV2_WORKER_ERR_UNKNOWN);
}

/// The worker thread is one per process, so the tests that block it take
/// turns; otherwise a `work` that waits would stall another test's request.
static WORKER_TESTS: Mutex<()> = Mutex::new(());

fn worker_tests() -> std::sync::MutexGuard<'static, ()> {
    WORKER_TESTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Pops one response, waiting up to two seconds for the worker thread.
fn wait_for_response(rings: &WorkerRings, out: &mut [u8]) -> usize {
    for _ in 0..2000 {
        if let Some(size) = rings.pop_response(out) {
            return size;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    panic!("the worker thread sent no response");
}

/// How many `work` calls are inside right now.
static WORK_INSIDE: AtomicU32 = AtomicU32::new(0);
/// How many `work` calls a test's instance has seen.
static WORK_CALLS: AtomicU32 = AtomicU32::new(0);

/// A fake `work` that echoes every request back through `respond`.
unsafe extern "C" fn echoing_work(
    _instance: LV2_Handle,
    respond: LV2_Worker_Respond_Function,
    handle: LV2_Worker_Respond_Handle,
    size: u32,
    data: *const c_void,
) -> LV2_Worker_Status {
    WORK_CALLS.fetch_add(1, Ordering::SeqCst);
    WORK_INSIDE.fetch_add(1, Ordering::SeqCst);
    let status = match respond {
        Some(respond) => unsafe { respond(handle, size, data) },
        None => LV2_WORKER_ERR_UNKNOWN,
    };
    WORK_INSIDE.fetch_sub(1, Ordering::SeqCst);
    status
}

#[test]
fn the_worker_thread_echoes_requests_in_order_and_forgets_an_unregistered_one() {
    let _serial = worker_tests();
    let rings = Arc::new(WorkerRings::new());
    let interface = LV2_Worker_Interface {
        work: Some(echoing_work),
        work_response: None,
        end_run: None,
    };
    let calls = WORK_CALLS.load(Ordering::SeqCst);
    let instance =
        register(ptr::null_mut(), &interface, Arc::clone(&rings)).expect("start the worker thread");

    let messages: [&[u8]; 3] = [b"first request", b"", b"a third, longer request"];
    for message in messages {
        assert!(rings.push_request(message));
    }
    wake(&instance);
    let mut out = vec![0u8; RING_BYTES];
    for message in messages {
        let size = wait_for_response(&rings, &mut out);
        assert_eq!(&out[..size], message, "a response lost its bytes or order");
    }
    assert_eq!(
        WORK_CALLS.load(Ordering::SeqCst) - calls,
        messages.len() as u32,
        "one work call per request"
    );

    unregister(&instance);
    assert_eq!(
        WORK_INSIDE.load(Ordering::SeqCst),
        0,
        "the worker was still inside work after unregister"
    );

    // A removed instance is not looked at again, even with a request in its
    // ring and a wake.
    let calls = WORK_CALLS.load(Ordering::SeqCst);
    assert!(rings.push_request(b"after unregister"));
    wake(&instance);
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(WORK_CALLS.load(Ordering::SeqCst), calls);
    assert!(!rings.response_pending());
    assert_eq!(
        registered_for_test(),
        0,
        "a test left an instance registered"
    );
}

/// With nothing registered the worker thread parks without a timeout, so an
/// idle process has no thread waking up every 50 ms.
#[test]
fn the_worker_thread_sleeps_while_no_instance_is_registered() {
    let _serial = worker_tests();
    let interface = LV2_Worker_Interface {
        work: Some(echoing_work),
        work_response: None,
        end_run: None,
    };
    let rings = Arc::new(WorkerRings::new());
    let instance =
        register(ptr::null_mut(), &interface, Arc::clone(&rings)).expect("start the worker thread");
    unregister(&instance);
    assert_eq!(registered_for_test(), 0);
    // Let the thread see the empty registry and park.
    std::thread::sleep(Duration::from_millis(100));
    let before = loops_for_test();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        loops_for_test(),
        before,
        "the worker thread woke with nothing registered"
    );

    // A registration still wakes it.
    let instance =
        register(ptr::null_mut(), &interface, Arc::clone(&rings)).expect("register again");
    let mut out = vec![0u8; RING_BYTES];
    assert!(rings.push_request(b"wake up"));
    let size = wait_for_response(&rings, &mut out);
    assert_eq!(&out[..size], b"wake up");
    unregister(&instance);
}

/// `worker.h`: a request `schedule_work` accepted must reach `work`.  The
/// gate is held while the instance is removed, so the worker thread skips it
/// and only `unregister` can serve the request.
#[test]
fn unregister_serves_the_requests_still_queued() {
    let _serial = worker_tests();
    let rings = Arc::new(WorkerRings::new());
    let interface = LV2_Worker_Interface {
        work: Some(echoing_work),
        work_response: None,
        end_run: None,
    };
    let instance =
        register(ptr::null_mut(), &interface, Arc::clone(&rings)).expect("start the worker thread");
    let calls = WORK_CALLS.load(Ordering::SeqCst);

    let gate = hold_gate(&instance);
    assert!(rings.push_request(b"free the old sample"));
    let removing = {
        let instance = Arc::clone(&instance);
        std::thread::spawn(move || unregister(&instance))
    };
    while registered_for_test() != 0 {
        std::thread::sleep(Duration::from_millis(1));
    }
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(
        WORK_CALLS.load(Ordering::SeqCst),
        calls,
        "work ran under the gate"
    );
    drop(gate);
    removing.join().expect("unregister");

    assert_eq!(
        WORK_CALLS.load(Ordering::SeqCst) - calls,
        1,
        "the queued request did not reach work"
    );
    assert!(!rings.request_pending());
}

#[test]
fn a_worker_response_is_refused_when_the_response_ring_is_full() {
    let rings = Arc::new(WorkerRings::new());
    let handle = Arc::as_ptr(&rings) as *mut c_void;
    let fills = vec![5u8; RING_BYTES - 4];
    assert_eq!(
        unsafe { respond(handle, fills.len() as u32, fills.as_ptr() as *const c_void) },
        LV2_WORKER_SUCCESS
    );
    let one = b"x";
    assert_eq!(
        unsafe { respond(handle, 1, one.as_ptr() as *const c_void) },
        LV2_WORKER_ERR_NO_SPACE,
        "a response with no space left was accepted"
    );
    assert_eq!(
        unsafe { respond(handle, 4, ptr::null()) },
        LV2_WORKER_ERR_UNKNOWN,
        "null data with a size was accepted"
    );

    let mut out = vec![0u8; RING_BYTES];
    assert_eq!(rings.pop_response(&mut out), Some(RING_BYTES - 4));
    assert_eq!(
        unsafe { respond(handle, 1, one.as_ptr() as *const c_void) },
        LV2_WORKER_SUCCESS,
        "the freed space was not usable"
    );

    // An interface the worker could never call is refused at registration,
    // so its requests cannot wait forever.
    let no_work = LV2_Worker_Interface {
        work: None,
        work_response: None,
        end_run: None,
    };
    assert!(
        register(ptr::null_mut(), &no_work, Arc::new(WorkerRings::new())).is_err(),
        "an interface without work was registered"
    );
}

/// The sender a blocking `work` reports its start on.
static BLOCK_STARTED: Mutex<Option<mpsc::Sender<()>>> = Mutex::new(None);
/// The receiver a blocking `work` waits on before it returns.
static BLOCK_RELEASE: Mutex<Option<mpsc::Receiver<()>>> = Mutex::new(None);

/// A fake `work` that waits until the test releases it.
unsafe extern "C" fn blocking_work(
    _instance: LV2_Handle,
    _respond: LV2_Worker_Respond_Function,
    _handle: LV2_Worker_Respond_Handle,
    _size: u32,
    _data: *const c_void,
) -> LV2_Worker_Status {
    if let Some(started) = BLOCK_STARTED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
    {
        let _ = started.send(());
    }
    let release = BLOCK_RELEASE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(release) = release {
        let _ = release.recv();
    }
    LV2_WORKER_SUCCESS
}

#[test]
fn unregister_waits_for_a_work_call_already_inside() {
    let _serial = worker_tests();
    let rings = Arc::new(WorkerRings::new());
    let interface = LV2_Worker_Interface {
        work: Some(blocking_work),
        work_response: None,
        end_run: None,
    };
    let instance =
        register(ptr::null_mut(), &interface, Arc::clone(&rings)).expect("start the worker thread");

    let (started_sender, started_receiver) = mpsc::channel();
    let (release_sender, release_receiver) = mpsc::channel();
    *BLOCK_STARTED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(started_sender);
    *BLOCK_RELEASE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(release_receiver);
    assert!(rings.push_request(b"hold the worker"));
    wake(&instance);
    started_receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("the worker did not start work");

    let returned = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&returned);
    let waiter_instance = Arc::clone(&instance);
    let waiter = std::thread::spawn(move || {
        unregister(&waiter_instance);
        flag.store(true, Ordering::Release);
    });
    // The worker is still inside `work`, which waits on the channel.
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        !returned.load(Ordering::Acquire),
        "unregister returned while work was still inside"
    );

    release_sender.send(()).expect("release the work");
    waiter.join().expect("join the unregister thread");
    assert!(returned.load(Ordering::Acquire));
    // The waiter already removed it from the registry; this second call is
    // the cleanup the caller owes, and proves it is idempotent.
    unregister(&instance);

    *BLOCK_STARTED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    *BLOCK_RELEASE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    assert_eq!(
        registered_for_test(),
        0,
        "a test left an instance registered"
    );
}

#[test]
fn one_instances_gate_does_not_block_another_and_unregistering_it_leaves_the_rest() {
    let _serial = worker_tests();
    let interface = LV2_Worker_Interface {
        work: Some(echoing_work),
        work_response: None,
        end_run: None,
    };
    let first_rings = Arc::new(WorkerRings::new());
    let first = register(ptr::null_mut(), &interface, Arc::clone(&first_rings))
        .expect("register the first instance");
    let second_rings = Arc::new(WorkerRings::new());
    let second = register(ptr::null_mut(), &interface, Arc::clone(&second_rings))
        .expect("register the second instance");
    assert_eq!(registered_for_test(), 2);

    // The first instance's gate is held by this thread; the second's gate is
    // a different lock and is free.  (One long `work` still occupies the
    // single worker thread by design.)
    let held = hold_gate(&first);
    let (paused_sender, paused_receiver) = mpsc::channel();
    let second_for_thread = Arc::clone(&second);
    let pauser = std::thread::spawn(move || {
        let _second_gate = hold_gate(&second_for_thread);
        let _ = paused_sender.send(());
    });
    paused_receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("the first instance's gate blocked the second");
    pauser.join().expect("join the pause thread");
    drop(held);

    // Removing the first leaves the second serving requests.
    unregister(&first);
    let mut out = vec![0u8; RING_BYTES];
    assert!(second_rings.push_request(b"still here"));
    wake(&second);
    let size = wait_for_response(&second_rings, &mut out);
    assert_eq!(&out[..size], b"still here");

    unregister(&second);
    assert_eq!(
        registered_for_test(),
        0,
        "a test left an instance registered"
    );
}

/// How many requests a failing `work` has seen.
static WORK_ERRORS: AtomicU32 = AtomicU32::new(0);

/// A fake `work` that refuses every request.
unsafe extern "C" fn failing_work(
    _instance: LV2_Handle,
    _respond: LV2_Worker_Respond_Function,
    _handle: LV2_Worker_Respond_Handle,
    _size: u32,
    _data: *const c_void,
) -> LV2_Worker_Status {
    WORK_ERRORS.fetch_add(1, Ordering::SeqCst);
    LV2_WORKER_ERR_UNKNOWN
}

#[test]
fn a_failing_work_does_not_wedge_the_instance() {
    let _serial = worker_tests();
    let rings = Arc::new(WorkerRings::new());
    let interface = LV2_Worker_Interface {
        work: Some(failing_work),
        work_response: None,
        end_run: None,
    };
    let instance =
        register(ptr::null_mut(), &interface, Arc::clone(&rings)).expect("start the worker thread");
    let errors = WORK_ERRORS.load(Ordering::SeqCst);

    assert!(rings.push_request(b"first"));
    wake(&instance);
    for _ in 0..2000 {
        if WORK_ERRORS.load(Ordering::SeqCst) > errors {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(WORK_ERRORS.load(Ordering::SeqCst) - errors, 1);

    // The failure is reported, not fatal: the next request still runs.
    assert!(rings.push_request(b"second"));
    wake(&instance);
    for _ in 0..2000 {
        if WORK_ERRORS.load(Ordering::SeqCst) - errors >= 2 {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        WORK_ERRORS.load(Ordering::SeqCst) - errors,
        2,
        "a failed work wedged the instance"
    );

    unregister(&instance);
    assert_eq!(
        registered_for_test(),
        0,
        "a test left an instance registered"
    );
}

const PARAMS_URI: &str = "http://example.com/lv2/params";

fn params_bundle() -> PathBuf {
    testdata("params.lv2")
}

fn params_table() -> PortTable {
    PortTable::read(&params_bundle(), PARAMS_URI, 48_000).expect("read params.lv2")
}

fn range_named<'a>(table: &'a PortTable, name: &str) -> &'a super::ports::Lv2Range {
    table
        .ranges
        .iter()
        .find(|range| range.name == name)
        .unwrap_or_else(|| panic!("no range {name:?}"))
}

#[test]
fn control_ranges_read_bounds_defaults_units_and_scale_points() {
    let table = params_table();
    assert_eq!(table.ranges.len(), 9, "inputs first, then the output");
    assert!(table.ranges[..8].iter().all(|range| !range.read_only));
    assert!(table.ranges[8].read_only, "the control output is read-only");

    let gain = range_named(&table, "Gain");
    assert_eq!((gain.min, gain.max, gain.default), (0.0, 10.0, 1.0));
    assert_eq!(gain.unit, "dB");
    assert_eq!(gain.step_count, 0);
    assert!(!gain.stepped && !gain.logarithmic);
    assert_eq!(gain.display(2.5), "2.50 dB");

    let cutoff = range_named(&table, "Cutoff");
    assert!(cutoff.logarithmic);
    assert_eq!(
        (cutoff.min, cutoff.max, cutoff.default),
        (20.0, 20_000.0, 1000.0)
    );
    assert_eq!(cutoff.unit, "Hz");

    // `lv2:sampleRate true` multiplies every bound by the sample rate.
    let rate = range_named(&table, "Rate Scale");
    assert_eq!(
        (rate.min, rate.max, rate.default),
        (4800.0, 480_000.0, 48_000.0)
    );

    let mode = range_named(&table, "Mode");
    assert!(mode.stepped);
    assert_eq!(mode.step_count, 3);
    assert_eq!(mode.points.len(), 4);
    assert_eq!(mode.display(2.0), "B");
    assert_eq!(mode.display(2.4), "B", "the nearest scale point wins");
    assert_eq!(
        mode.denormalise(0.5),
        2.0,
        "an integer snaps to whole values"
    );

    let on = range_named(&table, "On");
    assert!(on.stepped);
    assert_eq!(on.step_count, 1, "a toggle has one step");
    assert_eq!(on.denormalise(0.5), 1.0);
    assert_eq!(on.display(1.0), "1", "no decimals without a unit");

    let voices = range_named(&table, "Voices");
    assert!(voices.stepped);
    assert_eq!(voices.step_count, 7);
    assert_eq!(voices.unit, "");
    assert_eq!(voices.display(4.6), "5");

    let mix = range_named(&table, "Mix");
    assert_eq!(mix.unit, "%");
    assert_eq!(mix.display(50.0), "50.00 %");

    // A `notOnGUI` control and the latency report are not parameters.
    assert!(table.ranges.iter().all(|range| range.name != "Hidden"));
    assert!(table.ranges.iter().all(|range| range.name != "Latency"));
}

/// Every control input buffer starts at its `lv2:default`, scaled with the
/// sample rate, including a `notOnGUI` one that has no panel control: a
/// hidden "enable" that defaults to 1 must not run at 0.
#[test]
fn every_control_input_starts_at_its_default_even_when_hidden() {
    let table = params_table();
    let buffers = PortBuffers::new(&table, 4);
    assert_eq!(buffers.control(9), 1.0, "the hidden control's default");
    assert_eq!(buffers.control(4), 1.0, "the gain's default");
    assert_eq!(buffers.control(5), 1000.0, "the cutoff's default");
    assert_eq!(
        buffers.control(6),
        48_000.0,
        "the sample-rate control's default is scaled"
    );
    assert_eq!(buffers.control(16), 4.0, "the voices' default");
    assert_eq!(
        buffers.control(10),
        0.0,
        "an output is the plugin's to write"
    );
}

#[test]
fn port_properties_are_read_from_the_lv2_port_property_predicate() {
    // Every flag in the fixture is written the standard way,
    // `lv2:portProperty <property>`; the scan and the table must agree.
    let table = params_table();
    assert_eq!(table.ports[15].kind, PortKind::Audio);
    assert_eq!(
        table.ports[17].kind,
        PortKind::Cv,
        "the optional CV port from lv2:portProperty lv2:connectionOptional"
    );
    assert!(range_named(&table, "On").stepped, "lv2:toggled");
    assert!(range_named(&table, "Voices").stepped, "lv2:integer");
    assert!(
        range_named(&table, "Cutoff").logarithmic,
        "pprops:logarithmic"
    );
    assert_eq!(
        range_named(&table, "Rate Scale").min,
        4800.0,
        "lv2:sampleRate"
    );
    assert!(table.ranges.iter().all(|range| range.name != "Hidden"));

    let scanned = scan_bundle(&params_bundle()).expect("scan the fixture");
    assert_eq!(scanned.len(), 1);
    assert_eq!(
        scanned[0].scan_error, None,
        "the optional CV port needs no error"
    );
}

#[test]
fn linear_and_logarithmic_ranges_round_trip() {
    let table = params_table();
    let gain = range_named(&table, "Gain");
    assert_eq!(gain.normalise(5.0), 0.5);
    assert_eq!(gain.denormalise(0.5), 5.0);
    assert_eq!(gain.denormalise(2.0), 10.0, "a value is clamped");

    let cutoff = range_named(&table, "Cutoff");
    let expected = (1000.0f64 / 20.0).ln() / (20_000.0f64 / 20.0).ln();
    assert!((cutoff.normalise(1000.0) - expected).abs() < 1e-12);
    let back = cutoff.denormalise(cutoff.normalise(1000.0));
    assert!(
        (back - 1000.0).abs() < 1e-9,
        "the log scale round-trips: {back}"
    );
}

#[test]
fn parameter_ids_hash_the_symbol_and_fall_back_on_a_collision() {
    let table = params_table();
    assert_eq!(table.ids.id(0), Some(symbol_id("gain")));
    assert_eq!(table.ids.index_of(symbol_id("gain")), Some(0));

    // The second "gain" port keeps a distinct id: the port index.
    let copy = range_named(&table, "Gain Copy");
    assert_eq!(copy.id, 12, "the later collision uses its port index");
    assert_ne!(copy.id, symbol_id("gain"));
    assert_eq!(table.ids.index_of(copy.id), Some(5));

    let mut ids: Vec<u32> = table.ranges.iter().map(|range| range.id).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), table.ranges.len(), "every id is unique");

    // The hash is stable, not `DefaultHasher`.
    assert_eq!(symbol_id("gain"), 458_499_838);
}

#[test]
fn the_main_pair_is_the_first_two_audio_ports_and_the_rest_is_silent() {
    let table = params_table();
    let mut buffers = PortBuffers::new(&table, 4);
    assert_eq!(buffers.audio_inputs, [0, 1]);
    assert_eq!(buffers.audio_outputs, [2, 3]);
    assert_eq!(table.ports[15].kind, PortKind::Audio);
    assert!(table.ports[15].input, "the sidechain is an input");

    // An auxiliary input that held a signal is cleared rather than fed.
    match &mut buffers.slots[15].buffer {
        Buffer::Audio(samples) => samples.fill(1.0),
        _ => panic!("the sidechain has no audio buffer"),
    }
    let (left, right) = ([1.0f32, 2.0, 3.0, 4.0], [-1.0f32, -2.0, -3.0, -4.0]);
    buffers.silence(4);
    buffers.copy_input(&left, &right, 4);
    match &buffers.slots[0].buffer {
        Buffer::Audio(samples) => assert_eq!(samples, &left),
        _ => panic!("no main input buffer"),
    }
    match &buffers.slots[1].buffer {
        Buffer::Audio(samples) => assert_eq!(samples, &right),
        _ => panic!("no second input buffer"),
    }
    match &buffers.slots[15].buffer {
        Buffer::Audio(samples) => assert!(samples.iter().all(|&sample| sample == 0.0)),
        _ => panic!("no sidechain buffer"),
    }

    match &mut buffers.slots[2].buffer {
        Buffer::Audio(samples) => samples.copy_from_slice(&[5.0, 6.0, 7.0, 8.0]),
        _ => panic!("no main output buffer"),
    }
    match &mut buffers.slots[3].buffer {
        Buffer::Audio(samples) => samples.copy_from_slice(&[9.0, 10.0, 11.0, 12.0]),
        _ => panic!("no second output buffer"),
    }
    let (mut out_left, mut out_right) = ([0.0f32; 4], [0.0f32; 4]);
    buffers.copy_output(&mut out_left, &mut out_right, 4);
    assert_eq!(out_left, [5.0, 6.0, 7.0, 8.0]);
    assert_eq!(out_right, [9.0, 10.0, 11.0, 12.0]);
}

/// `lv2:isSideChain` ports are never the main pair, even when they come
/// first: the key input hears silence and the key output is ignored, and
/// the scan neither counts them as channels nor lists a plugin whose only
/// input is a side chain.
#[test]
fn side_chain_ports_are_not_the_main_pair_or_channels() {
    let bundle = testdata("side-chain.lv2");
    let table = PortTable::read(&bundle, "http://example.com/lv2/keyed", 48_000)
        .expect("read the keyed plugin");
    assert!(table.ports[0].side_chain && table.ports[2].side_chain);
    let mut buffers = PortBuffers::new(&table, 4);
    assert_eq!(
        buffers.audio_inputs,
        [1],
        "the key input is not a main input"
    );
    assert_eq!(
        buffers.audio_outputs,
        [3],
        "the key output is not a main output"
    );

    buffers.silence(4);
    buffers.copy_input(&[1.0; 4], &[3.0; 4], 4);
    match &buffers.slots[0].buffer {
        Buffer::Audio(samples) => assert_eq!(samples, &[0.0; 4], "the key heard the signal"),
        _ => panic!("no key buffer"),
    }
    match &buffers.slots[1].buffer {
        Buffer::Audio(samples) => assert_eq!(samples, &[2.0; 4], "the mono sum"),
        _ => panic!("no main input buffer"),
    }
    match &mut buffers.slots[2].buffer {
        Buffer::Audio(samples) => samples.fill(9.0),
        _ => panic!("no key output buffer"),
    }
    match &mut buffers.slots[3].buffer {
        Buffer::Audio(samples) => samples.fill(0.5),
        _ => panic!("no main output buffer"),
    }
    let (mut left, mut right) = ([0.0f32; 4], [0.0f32; 4]);
    buffers.copy_output(&mut left, &mut right, 4);
    assert_eq!((left, right), ([0.5; 4], [0.5; 4]));

    let listed = scan_bundle(&bundle).expect("scan the side-chain bundle");
    assert_eq!(
        listed.len(),
        1,
        "a plugin whose only input is a key was listed"
    );
    assert_eq!(listed[0].plugin_uri, "http://example.com/lv2/keyed");
    assert_eq!(listed[0].channels, Some(1), "the key counted as a channel");
}

#[test]
fn a_mono_plugin_sums_its_input_and_feeds_both_sides() {
    let bundle = testdata("example.lv2");
    let table = PortTable::read(&bundle, "http://example.com/lv2/tone", 48_000)
        .expect("read the mono plugin");
    let mut buffers = PortBuffers::new(&table, 4);
    assert_eq!(buffers.audio_inputs, [0]);
    assert_eq!(buffers.audio_outputs, [1]);

    buffers.silence(4);
    buffers.copy_input(&[1.0, 2.0, 3.0, 4.0], &[3.0, 4.0, 5.0, 6.0], 4);
    match &buffers.slots[0].buffer {
        Buffer::Audio(samples) => assert_eq!(samples, &[2.0, 3.0, 4.0, 5.0]),
        _ => panic!("no mono input buffer"),
    }

    match &mut buffers.slots[1].buffer {
        Buffer::Audio(samples) => samples.fill(0.75),
        _ => panic!("no mono output buffer"),
    }
    let (mut left, mut right) = ([0.0f32; 4], [0.0f32; 4]);
    buffers.copy_output(&mut left, &mut right, 4);
    assert_eq!(left, [0.75; 4], "the mono output feeds both sides");
    assert_eq!(right, [0.75; 4]);
}

/// The atom spec: an empty input sequence has the size of its body header,
/// as `lv2_atom_sequence_clear` leaves it, and an output is an `atom:Chunk`
/// sized to the space after its header before every `run`.
#[test]
fn atom_ports_are_rearmed_for_every_block() {
    let table = params_table();
    let mut buffers = PortBuffers::new(&table, 4);
    let sequence = urid::global().map(c"http://lv2plug.in/ns/ext/atom#Sequence");
    let chunk = urid::global().map(c"http://lv2plug.in/ns/ext/atom#Chunk");
    let body = std::mem::size_of::<lv2_sys::LV2_Atom_Sequence_Body>() as u32;
    buffers.reset_atoms();
    let input = buffers.atom(13).expect("the input atom port");
    let output = buffers.atom(14).expect("the output atom port");
    // The output asks for 32 KiB with `rsz:minimumSize`; the input has the
    // default capacity.
    let output_space = body + 32_768;
    unsafe {
        assert_eq!((*input).atom.size, body, "an empty sequence");
        assert_eq!((*input).atom.type_, sequence);
        assert_eq!((*input).body.unit, 0);
        assert_eq!((*output).atom.size, output_space, "the space to write");
        assert_eq!((*output).atom.type_, chunk);
        // A plugin that wrote events leaves another size and type behind.
        (*output).atom.size = 7;
        (*output).atom.type_ = sequence;
        (*input).atom.size = 64;
    }
    buffers.reset_atoms();
    unsafe {
        assert_eq!((*output).atom.size, output_space);
        assert_eq!((*output).atom.type_, chunk);
        assert_eq!((*input).atom.size, body);
    }
}

/// `rsz:minimumSize` counts the whole buffer, so an atom port's buffer is
/// at least that large, never below the default capacity, and the
/// `sequenceSize` option names the largest one.
#[test]
fn atom_buffers_honour_the_minimum_size() {
    let table = params_table();
    assert_eq!(table.ports[13].capacity, ATOM_CAPACITY);
    assert_eq!(table.ports[14].capacity, 32_768);
    let header = std::mem::size_of::<lv2_sys::LV2_Atom_Sequence>() as u32;
    assert_eq!(table.sequence_size(), header + 32_768);
    let buffers = PortBuffers::new(&table, 4);
    match &buffers.slots[14].buffer {
        Buffer::Atom(atom) => assert!(atom.bytes() >= 32_768, "{}", atom.bytes()),
        _ => panic!("no atom buffer"),
    }

    let huge = PortTable::read(
        &testdata("atom-types.lv2"),
        "http://example.com/lv2/huge-port",
        48_000,
    )
    .err()
    .expect("a 4 GiB atom buffer");
    assert!(huge.contains("buffer of more than"), "{huge}");
}

/// The host may only connect an atom port to a type it lists, and this host
/// only supplies `atom:Sequence`: another type (or none) refuses the plugin
/// at scan and load, unless the port is optional, which is connected to
/// NULL.
#[test]
fn an_atom_port_that_does_not_take_a_sequence_is_refused_or_left_unconnected() {
    let bundle = testdata("atom-types.lv2");
    let double = PortTable::read(&bundle, "http://example.com/lv2/double-port", 48_000)
        .err()
        .expect("an atom:Double port");
    assert!(double.contains("atom:Double"), "{double}");
    let untyped = PortTable::read(&bundle, "http://example.com/lv2/untyped-port", 48_000)
        .err()
        .expect("an atom port without a buffer type");
    assert!(untyped.contains("no atom:bufferType"), "{untyped}");

    let optional = PortTable::read(&bundle, "http://example.com/lv2/optional-double", 48_000)
        .expect("an optional atom:Double port");
    assert_eq!(optional.ports[2].kind, PortKind::Other);
    let buffers = PortBuffers::new(&optional, 4);
    assert!(
        !buffers.slots[2].connect,
        "the optional port is connected to NULL"
    );

    let listed = scan_bundle(&bundle).expect("scan the atom bundle");
    let error = |uri: &str| {
        listed
            .iter()
            .find(|candidate| candidate.plugin_uri == uri)
            .unwrap_or_else(|| panic!("{uri} is not listed"))
            .scan_error
            .clone()
    };
    assert!(error("http://example.com/lv2/double-port")
        .is_some_and(|error| error.contains("Amount") && error.contains("atom:Double")));
    assert!(error("http://example.com/lv2/untyped-port")
        .is_some_and(|error| error.contains("no atom:bufferType")));
    assert_eq!(error("http://example.com/lv2/optional-double"), None);
}

#[test]
fn the_latency_port_is_a_control_output_and_is_read_from_its_buffer() {
    let table = params_table();
    assert_eq!(table.latency, Some(11));
    let mut buffers = PortBuffers::new(&table, 4);
    assert_eq!(buffers.latency(), 0.0);
    buffers.set_control(11, 128.0);
    assert_eq!(buffers.latency(), 128.0);
}

#[test]
fn a_malformed_port_table_is_refused() {
    let cv = PortTable::read(
        &testdata("cv-port.lv2"),
        "http://example.com/lv2/cv",
        48_000,
    )
    .err()
    .expect("a required CV port");
    assert!(cv.contains("CV port"), "{cv}");
    assert!(cv.contains("Vibrato"), "{cv}");

    let fixed = PortTable::read(
        &testdata("fixed-block.lv2"),
        "http://example.com/lv2/fixed",
        48_000,
    )
    .err()
    .expect("an unsupported required feature");
    assert!(fixed.contains("fixed block length"), "{fixed}");

    let missing = PortTable::read(
        &testdata("no-index.lv2"),
        "http://example.com/lv2/no-index",
        48_000,
    )
    .err()
    .expect("a missing index");
    assert!(missing.contains("lv2:index"), "{missing}");

    let duplicate = PortTable::read(
        &testdata("duplicate-index.lv2"),
        "http://example.com/lv2/duplicate-index",
        48_000,
    )
    .err()
    .expect("a duplicated index");
    assert!(duplicate.contains("twice"), "{duplicate}");
}

#[test]
fn the_native_binary_is_the_platform_file_inside_the_bundle() {
    let bundle = testdata("example.lv2");
    let (store, plugins) = read_bundle(&bundle).expect("read the bundle");
    assert_eq!(plugins.len(), 3);
    let binary = native_binary(&store, "http://example.com/lv2/example", &bundle)
        .expect("the platform binary");
    let expected = if cfg!(target_os = "windows") {
        "example.dll"
    } else {
        "example.so"
    };
    assert_eq!(
        binary.file_name().and_then(|name| name.to_str()),
        Some(expected)
    );
    assert!(
        native_binary(
            &store,
            "http://example.com/lv2/example",
            &testdata("cv-port.lv2")
        )
        .is_err(),
        "a binary outside the bundle is refused"
    );
}

static PROCESS_RUNS: AtomicU32 = AtomicU32::new(0);
static FLUSH_RUNS: AtomicU32 = AtomicU32::new(0);
static HANDLE_RUNS: AtomicU32 = AtomicU32::new(0);

unsafe extern "C" fn process_run(_handle: LV2_Handle, frames: u32) {
    PROCESS_RUNS.fetch_add(frames, Ordering::Relaxed);
}

unsafe extern "C" fn flush_run(_handle: LV2_Handle, frames: u32) {
    FLUSH_RUNS.fetch_add(frames, Ordering::Relaxed);
}

unsafe extern "C" fn handle_run(_handle: LV2_Handle, frames: u32) {
    HANDLE_RUNS.fetch_add(frames, Ordering::Relaxed);
}

fn fake_processor(
    table: &PortTable,
    core: Arc<Lv2Core>,
    run: unsafe extern "C" fn(LV2_Handle, u32),
) -> (
    Lv2Processor,
    crate::audio::plugins::ParamSender,
    crate::audio::plugins::ParamReceiver,
) {
    let ranges: Arc<[super::ports::Lv2Range]> = Arc::from(table.ranges.clone().into_boxed_slice());
    let ids = Arc::clone(&table.ids);
    let buffers = PortBuffers::new(table, 8);
    let (to_audio, from_ui) = param_transfer(ids.len());
    let (to_ui, from_audio) = param_transfer(ids.len());
    let reported: Vec<u32> = ranges
        .iter()
        .map(|range| buffers.control(range.slot).to_bits())
        .collect();
    let processor = Lv2Processor {
        buffers,
        ranges,
        ids,
        from_ui,
        to_ui,
        handle: ptr::null_mut(),
        run,
        reported,
        max_block: 8,
        worker: None,
        core,
    };
    (processor, to_audio, from_audio)
}

#[test]
fn a_queued_value_reaches_the_control_buffer_and_outputs_report_back() {
    let table = params_table();
    let core = Arc::new(Lv2Core::for_test(true));
    let (mut processor, mut sender, mut from_audio) =
        fake_processor(&table, Arc::clone(&core), process_run);
    let gain = table.ids.index_of(symbol_id("gain")).expect("the gain");
    let level = table.ids.index_of(symbol_id("level")).expect("the level");
    let gain_slot = table.ranges[gain].slot;
    let level_slot = table.ranges[level].slot;
    let latency_slot = table.latency.expect("a latency port");

    let runs = PROCESS_RUNS.load(Ordering::Relaxed);
    assert!(sender.push(gain, 0.5, 0));
    let (mut left, mut right) = (vec![1.0f32; 16], vec![-1.0f32; 16]);
    processor
        .process(&mut left, &mut right)
        .expect("process a block");
    assert_eq!(
        PROCESS_RUNS.load(Ordering::Relaxed) - runs,
        16,
        "every frame was run"
    );
    assert!((processor.buffers.control(gain_slot) - 5.0).abs() < 1e-6);
    assert_eq!(sender.applied(), 1, "a written value counts as applied");
    match &processor.buffers.slots[0].buffer {
        Buffer::Audio(samples) => {
            assert_eq!(samples.len(), 8, "a buffer is one block long");
            assert!(samples.iter().all(|&sample| sample == 1.0));
        }
        _ => panic!("no main input buffer"),
    }

    // A control output moves only when it changes.
    processor.buffers.set_control(level_slot, 0.75);
    processor
        .process(&mut left, &mut right)
        .expect("process another block");
    let mut reported = Vec::new();
    from_audio.drain(|index, value, _| reported.push((index, value)));
    assert_eq!(reported, vec![(level, 0.75)]);
    processor
        .process(&mut left, &mut right)
        .expect("a quiet block");
    let mut again = Vec::new();
    from_audio.drain(|index, value, _| again.push((index, value)));
    assert!(again.is_empty(), "an unchanged output was reported again");

    // The latency port is published through the shared atomics.
    processor.buffers.set_control(latency_slot, 128.0);
    processor
        .process(&mut left, &mut right)
        .expect("a block after a latency change");
    assert_eq!(
        f32::from_bits(core.latency_raw.load(Ordering::Acquire)),
        128.0
    );
    assert!(core.latency_changed.load(Ordering::Acquire));
}

#[test]
fn a_flush_writes_queued_values_without_running_the_plugin() {
    let table = params_table();
    let core = Arc::new(Lv2Core::for_test(true));
    let (mut processor, mut sender, _from_audio) =
        fake_processor(&table, Arc::clone(&core), flush_run);
    let gain = table.ids.index_of(symbol_id("gain")).expect("the gain");
    let gain_slot = table.ranges[gain].slot;
    let runs = FLUSH_RUNS.load(Ordering::Relaxed);

    assert!(sender.push(gain, 1.0, 0));
    processor.flush_if_queued();
    assert!((processor.buffers.control(gain_slot) - 10.0).abs() < 1e-6);
    assert_eq!(sender.applied(), 1);
    assert_eq!(
        FLUSH_RUNS.load(Ordering::Relaxed),
        runs,
        "a flush never calls run"
    );
}

#[test]
fn an_inactive_lv2_instance_does_not_process() {
    let table = params_table();
    let core = Arc::new(Lv2Core::for_test(false));
    let (mut processor, _sender, _from_audio) = fake_processor(&table, core, process_run);
    let (mut left, mut right) = (vec![1.0f32; 4], vec![1.0f32; 4]);
    assert_eq!(
        processor.process(&mut left, &mut right),
        Err(Lv2Failure::NotActive)
    );
}

/// Every response body the fake `work_response` saw.
static WORK_RESPONSES: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());
/// How many responses the fake `run` had already seen when it was called.
static RESPONSES_BEFORE_RUN: AtomicU32 = AtomicU32::new(u32::MAX);
static END_RUN_CALLS: AtomicU32 = AtomicU32::new(0);

unsafe extern "C" fn recorded_work_response(
    _handle: LV2_Handle,
    size: u32,
    body: *const c_void,
) -> LV2_Worker_Status {
    let body = if body.is_null() {
        Vec::new()
    } else {
        // SAFETY: the host promises `size` readable bytes for the call.
        unsafe { std::slice::from_raw_parts(body as *const u8, size as usize) }.to_vec()
    };
    WORK_RESPONSES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(body);
    LV2_WORKER_SUCCESS
}

unsafe extern "C" fn response_counting_run(_handle: LV2_Handle, _frames: u32) {
    RESPONSES_BEFORE_RUN.store(
        WORK_RESPONSES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len() as u32,
        Ordering::SeqCst,
    );
}

unsafe extern "C" fn counted_end_run(_handle: LV2_Handle) -> LV2_Worker_Status {
    END_RUN_CALLS.fetch_add(1, Ordering::SeqCst);
    LV2_WORKER_SUCCESS
}

#[test]
fn responses_are_delivered_before_run_and_end_run_after_every_block() {
    let table = params_table();
    let core = Arc::new(Lv2Core::for_test(true));
    let (mut processor, _sender, _from_audio) =
        fake_processor(&table, Arc::clone(&core), response_counting_run);
    let rings = Arc::new(WorkerRings::new());
    let messages: [&[u8]; 2] = [b"impulse loaded", b"second"];
    for message in messages {
        assert!(rings.push_response(message));
    }
    processor.worker = Some(WorkerCallbacks {
        rings: Arc::clone(&rings),
        responses: vec![0u8; RING_BYTES],
        work_response: Some(recorded_work_response),
        end_run: Some(counted_end_run),
    });
    WORK_RESPONSES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
    let end_runs = END_RUN_CALLS.load(Ordering::SeqCst);

    let (mut left, mut right) = (vec![0.0f32; 8], vec![0.0f32; 8]);
    processor
        .process(&mut left, &mut right)
        .expect("process a block");
    {
        let responses = WORK_RESPONSES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(responses.len(), messages.len(), "a response was lost");
        for (response, message) in responses.iter().zip(messages) {
            assert_eq!(response.as_slice(), message, "a response was reordered");
        }
    }
    assert_eq!(
        RESPONSES_BEFORE_RUN.load(Ordering::SeqCst),
        messages.len() as u32,
        "run saw the block before its responses"
    );
    assert_eq!(
        END_RUN_CALLS.load(Ordering::SeqCst) - end_runs,
        1,
        "end_run was not called once after the block"
    );
    assert!(!rings.response_pending());

    // A quiet block still closes the cycle, and nothing is delivered twice.
    processor
        .process(&mut left, &mut right)
        .expect("a block with no response");
    assert_eq!(
        WORK_RESPONSES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        messages.len()
    );
    assert_eq!(
        END_RUN_CALLS.load(Ordering::SeqCst) - end_runs,
        2,
        "end_run must follow every run, response or not"
    );
    assert_eq!(
        RESPONSES_BEFORE_RUN.load(Ordering::SeqCst),
        messages.len() as u32
    );

    // A block the processor splits into chunks closes each run's cycle.
    let (mut left, mut right) = (vec![0.0f32; 16], vec![0.0f32; 16]);
    processor
        .process(&mut left, &mut right)
        .expect("a two-chunk block");
    assert_eq!(
        END_RUN_CALLS.load(Ordering::SeqCst) - end_runs,
        4,
        "end_run must follow every run, two chunks included"
    );
}

#[test]
fn a_plugin_without_work_response_has_its_responses_drained() {
    // Its own `run`: `process_run`'s counter belongs to the test that
    // counts frames, which runs in parallel.
    unsafe extern "C" fn uncounted_run(_handle: LV2_Handle, _frames: u32) {}

    let table = params_table();
    let core = Arc::new(Lv2Core::for_test(true));
    let (mut processor, _sender, _from_audio) = fake_processor(&table, core, uncounted_run);
    let rings = Arc::new(WorkerRings::new());
    assert!(rings.push_response(b"no handler"));
    assert!(rings.push_response(b"second"));
    processor.worker = Some(WorkerCallbacks {
        rings: Arc::clone(&rings),
        responses: vec![0u8; RING_BYTES],
        work_response: None,
        end_run: None,
    });
    let (mut left, mut right) = (vec![0.0f32; 8], vec![0.0f32; 8]);
    processor
        .process(&mut left, &mut right)
        .expect("process a block");
    assert!(
        !rings.response_pending(),
        "a response without a handler stayed in the ring"
    );
}

static ZERO_FRAME_RUNS: AtomicU32 = AtomicU32::new(0);
static ZERO_FRAME_FRAMES: AtomicU32 = AtomicU32::new(u32::MAX);
static ZERO_FRAME_END_RUNS: AtomicU32 = AtomicU32::new(0);

unsafe extern "C" fn zero_frame_run(_handle: LV2_Handle, frames: u32) {
    ZERO_FRAME_FRAMES.store(frames, Ordering::SeqCst);
    ZERO_FRAME_RUNS.fetch_add(1, Ordering::SeqCst);
}

unsafe extern "C" fn zero_frame_end_run(_handle: LV2_Handle) -> LV2_Worker_Status {
    ZERO_FRAME_END_RUNS.fetch_add(1, Ordering::SeqCst);
    LV2_WORKER_SUCCESS
}

#[test]
fn the_zero_frame_run_is_closed_by_end_run() {
    let table = params_table();
    let descriptor = Box::new(LV2_Descriptor {
        URI: c"http://rigwerk.test/zero-frame".as_ptr(),
        instantiate: None,
        connect_port: None,
        activate: None,
        run: Some(zero_frame_run),
        deactivate: None,
        cleanup: None,
        extension_data: None,
    });
    let core = Arc::new(Lv2Core {
        handle: ptr::null_mut(),
        descriptor: &*descriptor as *const LV2_Descriptor,
        _features: HostFeatures::new(48_000.0, 8, ATOM_CAPACITY, None, false),
        _rings: Arc::new(WorkerRings::new()),
        _bundle_path: CString::default(),
        retired: OnceLock::new(),
        worker: OnceLock::new(),
        active: AtomicBool::new(false),
        activated: AtomicBool::new(false),
        latency_raw: AtomicU32::new(0),
        latency_changed: AtomicBool::new(false),
        _module: None,
    });
    let ranges: Arc<[super::ports::Lv2Range]> = Arc::from(table.ranges.clone().into_boxed_slice());
    let values: Vec<f32> = table
        .ranges
        .iter()
        .map(|range| range.default as f32)
        .collect();
    let shared = Arc::new(PluginShared::new(0));
    let mut instance = Lv2Instance::for_test(ranges, values, shared, core);
    instance.set_worker_end_run_for_test(Some(zero_frame_end_run));
    let mut buffers = PortBuffers::new(&table, 8);

    let runs = ZERO_FRAME_RUNS.load(Ordering::SeqCst);
    let ends = ZERO_FRAME_END_RUNS.load(Ordering::SeqCst);
    instance.run_zero(&mut buffers);
    assert_eq!(ZERO_FRAME_FRAMES.load(Ordering::SeqCst), 0);
    assert_eq!(ZERO_FRAME_RUNS.load(Ordering::SeqCst) - runs, 1);
    assert_eq!(
        ZERO_FRAME_END_RUNS.load(Ordering::SeqCst) - ends,
        1,
        "end_run did not follow the zero-frame run"
    );
}

#[test]
fn swapped_bounds_are_ordered_and_bad_bounds_are_refused() {
    let bundle = testdata("bounds.lv2");
    let table =
        PortTable::read(&bundle, "http://example.com/lv2/swapped", 48_000).expect("swapped bounds");
    let bounded = range_named(&table, "Bounded");
    assert_eq!(
        (bounded.min, bounded.max),
        (0.0, 10.0),
        "the bounds are ordered"
    );
    assert_eq!(
        bounded.default, 10.0,
        "the default is clamped into the range"
    );
    assert_eq!(bounded.denormalise(0.5), 5.0);
    assert_eq!(bounded.normalise(f64::NAN), 0.0);
    assert_eq!(bounded.denormalise(f64::NAN), 0.0);

    let error = PortTable::read(&bundle, "http://example.com/lv2/nonfinite", 48_000)
        .err()
        .expect("non-finite bounds are refused");
    assert!(error.contains("non-finite"), "{error}");
}

#[test]
fn a_gap_in_the_port_indices_is_harmless() {
    let table = PortTable::read(
        &testdata("sparse.lv2"),
        "http://example.com/lv2/sparse",
        48_000,
    )
    .expect("sparse indices");
    let indexes: Vec<u32> = table.ports.iter().map(|port| port.index).collect();
    assert_eq!(indexes, [0, 2, 3]);
    let buffers = PortBuffers::new(&table, 4);
    let ports: Vec<u32> = buffers.slots.iter().map(|slot| slot.port).collect();
    assert_eq!(ports, [0, 2, 3], "a slot keeps its declared port index");
    assert_eq!(buffers.audio_inputs, [0]);
    assert_eq!(buffers.audio_outputs, [1], "slot 1 is declared port 2");
    assert_eq!(table.ranges[0].slot, 2, "the range keeps its buffer slot");
}

#[test]
fn a_continuous_control_shows_its_value_even_with_scale_points() {
    let range = super::ports::Lv2Range {
        id: 1,
        slot: 0,
        name: "Shape".to_owned(),
        min: 0.0,
        max: 3.0,
        default: 0.0,
        stepped: false,
        logarithmic: false,
        read_only: false,
        step_count: 0,
        unit: String::new(),
        points: vec![
            ScalePoint {
                value: 0.0,
                label: "Sine".to_owned(),
            },
            ScalePoint {
                value: 1.0,
                label: "Square".to_owned(),
            },
        ],
    };
    assert_eq!(range.display(0.0), "0.00", "not the point label");
    assert_eq!(range.display(1.0), "1.00");
    let mut stepped = range;
    stepped.stepped = true;
    assert_eq!(stepped.display(1.0), "Square");
}

#[test]
fn a_handle_sets_an_lv2_control_and_services_the_reported_output() {
    let table = params_table();
    let shared = Arc::new(PluginShared::new(0));
    let core = Arc::new(Lv2Core::for_test(true));
    let ranges: Arc<[super::ports::Lv2Range]> = Arc::from(table.ranges.clone().into_boxed_slice());
    let ids = Arc::clone(&table.ids);
    let values: Vec<f32> = table
        .ranges
        .iter()
        .map(|range| range.default as f32)
        .collect();
    let buffers = PortBuffers::new(&table, 8);
    let (to_audio, from_ui) = param_transfer(ids.len());
    let (to_ui, from_audio) = param_transfer(ids.len());
    let reported: Vec<u32> = ranges
        .iter()
        .map(|range| buffers.control(range.slot).to_bits())
        .collect();
    // A fake state extension, so the capture below has a blob to store.
    let interface = state_interface();
    let mut instance = Lv2Instance::for_test(
        Arc::clone(&ranges),
        values,
        Arc::clone(&shared),
        Arc::clone(&core),
    );
    instance.set_state_interface_for_test(&interface);
    let processor = Lv2Processor {
        buffers,
        ranges,
        ids: Arc::clone(&ids),
        from_ui,
        to_ui,
        handle: ptr::null_mut(),
        run: handle_run,
        reported,
        max_block: 8,
        worker: None,
        core,
    };
    let out = Arc::new(ParamOut::new(Arc::clone(&ids), to_audio));
    let views = instance.parameters();
    let loaded = Lv2Loaded {
        instance,
        processor,
        out,
        from_audio,
        ids: Arc::clone(&ids),
        views,
        has_editor: false,
        state_current: false,
        undelivered: Vec::new(),
    };
    let mut plugin = LoadedPlugin::lv2(
        loaded,
        Arc::clone(&shared),
        Saved {
            params: Vec::new(),
            state: None,
            stale: false,
        },
    );
    let handle = plugin.handle().clone();
    let gain = ids.index_of(symbol_id("gain")).expect("the gain");
    let gain_display = handle.set_parameter(symbol_id("gain"), 0.5).expect("set");
    assert_eq!(gain_display.as_deref(), Some("5.00 dB"));

    // The panel shows the queued value at once; a refresh must not rebuild
    // the views from the instance's older native value.
    handle.refresh_parameters();
    assert!((handle.parameters()[gain].value - 0.5).abs() < 1e-9);
    // The instance's own values follow the panel.
    assert!((handle.lv2_parameters_for_test()[gain].value - 0.5).abs() < 1e-9);

    let (mut left, mut right) = (vec![0.0f32; 8], vec![0.0f32; 8]);
    assert_eq!(
        plugin.run_for_test(&mut left, &mut right),
        Outcome::Processed
    );
    // No queued value is left, so the sync may read and keeps the value.
    let synced = handle.sync_full().expect("nothing is queued");
    assert!((synced[gain].value - 0.5).abs() < 1e-9);
    assert_eq!(synced[gain].display, "5.00 dB");

    // A reported control output reaches the view through `service`, which is
    // also the arm a wrong format match would break.
    let level = ids.index_of(symbol_id("level")).expect("the level");
    let level_slot = table.ranges[level].slot;
    plugin
        .lv2_processor_for_test()
        .buffers
        .set_control(level_slot, 0.75);
    assert_eq!(
        plugin.run_for_test(&mut left, &mut right),
        Outcome::Processed
    );
    assert!(handle.service(), "the reported output changed the views");
    let level_view = handle
        .parameters()
        .into_iter()
        .find(|view| view.id == symbol_id("level"))
        .expect("the level view");
    assert!((level_view.value - 0.75).abs() < 1e-9);
    assert_eq!(level_view.display, "0.75");
    assert!(!handle.service(), "nothing moved in the next frame");

    // An LV2 blob holds no control values, so its snapshot must be empty:
    // every saved parameter is applied on top of a restored blob.
    handle.capture_state();
    let (_, captured) = handle.saved();
    let captured = captured.expect("the fake state interface saved a blob");
    assert!(
        captured.params.is_empty(),
        "an LV2 blob claimed control values it does not hold"
    );

    // A reset returns the instance's own values to the defaults too.
    handle.reset_parameters().expect("reset");
    let default = table.ranges[gain].normalise(table.ranges[gain].default);
    assert!((handle.lv2_parameters_for_test()[gain].value - default).abs() < 1e-9);
}

// The fake state extension the state tests drive.  It has no plugin behind it:
// `save` stores two POD properties, one non-POD property and one under a URID
// the map never handed out; `restore` reads two back and asks for an absent
// key.  Statics, because an `extern "C"` function pointer cannot capture.
const STATE_GAIN_KEY: &CStr = c"http://rigwerk.test/state/gain";
const STATE_NAME_KEY: &CStr = c"http://rigwerk.test/state/name";
const STATE_FILE_KEY: &CStr = c"http://rigwerk.test/state/file";
const STATE_MISSING_KEY: &CStr = c"http://rigwerk.test/state/missing";
const STATE_DUPLICATE_KEY: &CStr = c"http://rigwerk.test/state/duplicate";
const STATE_ATOM_INT: &CStr = c"http://lv2plug.in/ns/ext/atom#Int";
const STATE_ATOM_STRING: &CStr = c"http://lv2plug.in/ns/ext/atom#String";
const STATE_ATOM_PATH: &CStr = c"http://lv2plug.in/ns/ext/atom#Path";
const STATE_MAP_PATH: &CStr = c"http://lv2plug.in/ns/ext/state#mapPath";
const STATE_FREE_PATH: &CStr = c"http://lv2plug.in/ns/ext/state#freePath";
/// A URID the map never handed out.
const STATE_UNKNOWN_URID: u32 = 0xDEAD_BEEF;

/// The feature array a real load passes to the state calls.
fn state_host() -> Box<HostFeatures> {
    HostFeatures::new(48_000.0, 8, ATOM_CAPACITY, None, false)
}

/// Whether `uri` is in the null-terminated feature array `features`.
fn feature_present(features: *const *const LV2_Feature, uri: &CStr) -> bool {
    if features.is_null() {
        return false;
    }
    for index in 0..32 {
        // SAFETY: a null-terminated array, as every LV2 call gets.
        let feature = unsafe { *features.add(index) };
        if feature.is_null() {
            return false;
        }
        // SAFETY: every feature in the array names its URI with a C string.
        if unsafe { CStr::from_ptr((*feature).URI) } == uri {
            return true;
        }
    }
    false
}

/// What the last fake save saw of its own call, for the tests that assert it.
static SAVE_FLAGS: AtomicU32 = AtomicU32::new(0);
static SAVE_SAW_MAP_PATH: AtomicBool = AtomicBool::new(false);
static SAVE_SAW_FREE_PATH: AtomicBool = AtomicBool::new(false);
static RESTORE_SAW_MAP_PATH: AtomicBool = AtomicBool::new(false);
static RESTORE_SAW_FREE_PATH: AtomicBool = AtomicBool::new(false);

fn pod_flag() -> u32 {
    u32::from(LV2_State_Flags::LV2_STATE_IS_POD)
}

fn native_flag() -> u32 {
    u32::from(LV2_State_Flags::LV2_STATE_IS_NATIVE)
}

/// The unwrapped `store` callback type the fake save calls.
type StoreFn = unsafe extern "C" fn(
    lv2_sys::LV2_State_Handle,
    u32,
    *const c_void,
    usize,
    u32,
    u32,
) -> LV2_State_Status;

unsafe fn store_value(
    store: StoreFn,
    handle: lv2_sys::LV2_State_Handle,
    key: u32,
    value: &[u8],
    type_: u32,
    flags: u32,
) -> LV2_State_Status {
    // SAFETY: the fake's caller passes a live store callback and its handle.
    unsafe {
        store(
            handle,
            key,
            value.as_ptr() as *const c_void,
            value.len(),
            type_,
            flags,
        )
    }
}

unsafe extern "C" fn fake_state_save(
    _instance: LV2_Handle,
    store: LV2_State_Store_Function,
    handle: lv2_sys::LV2_State_Handle,
    flags: u32,
    features: *const *const LV2_Feature,
) -> LV2_State_Status {
    SAVE_FLAGS.store(flags, Ordering::Relaxed);
    SAVE_SAW_MAP_PATH.store(feature_present(features, STATE_MAP_PATH), Ordering::Relaxed);
    SAVE_SAW_FREE_PATH.store(
        feature_present(features, STATE_FREE_PATH),
        Ordering::Relaxed,
    );
    let Some(store) = store else {
        return LV2_STATE_ERR_UNKNOWN;
    };
    let map = urid::global();
    let pod = pod_flag();
    let int = map.map(STATE_ATOM_INT);
    unsafe {
        store_value(
            store,
            handle,
            map.map(STATE_GAIN_KEY),
            &42i32.to_le_bytes(),
            int,
            pod,
        );
        store_value(
            store,
            handle,
            map.map(STATE_NAME_KEY),
            b"RigWerk",
            map.map(STATE_ATOM_STRING),
            pod,
        );
        // Not plain data: a pointer or a handle would not survive the save.
        store_value(
            store,
            handle,
            map.map(STATE_FILE_KEY),
            b"ir.wav",
            map.map(STATE_ATOM_PATH),
            0,
        );
        // A key no one mapped: the host cannot name it in the blob.
        store_value(
            store,
            handle,
            STATE_UNKNOWN_URID,
            &7i32.to_le_bytes(),
            int,
            pod,
        );
    }
    LV2_STATE_SUCCESS
}

/// What [`fake_state_restore`] read, for the test to assert.
#[derive(Default)]
struct RestoredState {
    gain: Vec<u8>,
    gain_size: usize,
    gain_type: u32,
    gain_flags: u32,
    name: Vec<u8>,
    absent_is_null: bool,
    null_out_pointers_ok: bool,
}

static RESTORED: Mutex<Option<RestoredState>> = Mutex::new(None);

unsafe extern "C" fn fake_state_restore(
    _instance: LV2_Handle,
    retrieve: LV2_State_Retrieve_Function,
    handle: lv2_sys::LV2_State_Handle,
    _flags: u32,
    features: *const *const LV2_Feature,
) -> LV2_State_Status {
    RESTORE_SAW_MAP_PATH.store(feature_present(features, STATE_MAP_PATH), Ordering::Relaxed);
    RESTORE_SAW_FREE_PATH.store(
        feature_present(features, STATE_FREE_PATH),
        Ordering::Relaxed,
    );
    let Some(retrieve) = retrieve else {
        return LV2_STATE_ERR_UNKNOWN;
    };
    let map = urid::global();
    let mut state = RestoredState::default();
    let (mut size, mut type_, mut flags) = (0usize, 0u32, 0u32);
    let value = unsafe {
        retrieve(
            handle,
            map.map(STATE_GAIN_KEY),
            &mut size,
            &mut type_,
            &mut flags,
        )
    };
    if value.is_null() {
        return LV2_STATE_ERR_UNKNOWN;
    }
    // SAFETY: the host promises `size` readable bytes at the returned pointer.
    state.gain = unsafe { std::slice::from_raw_parts(value as *const u8, size) }.to_vec();
    state.gain_size = size;
    state.gain_type = type_;
    state.gain_flags = flags;

    let value = unsafe {
        retrieve(
            handle,
            map.map(STATE_NAME_KEY),
            &mut size,
            &mut type_,
            &mut flags,
        )
    };
    if value.is_null() {
        return LV2_STATE_ERR_UNKNOWN;
    }
    // SAFETY: as above.
    state.name = unsafe { std::slice::from_raw_parts(value as *const u8, size) }.to_vec();

    let absent = unsafe {
        retrieve(
            handle,
            map.map(STATE_MISSING_KEY),
            &mut size,
            &mut type_,
            &mut flags,
        )
    };
    state.absent_is_null = absent.is_null();

    // The out-pointers are optional in the retrieve signature.
    let value = unsafe {
        retrieve(
            handle,
            map.map(STATE_GAIN_KEY),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    state.null_out_pointers_ok = !value.is_null();

    let Ok(mut slot) = RESTORED.lock() else {
        return LV2_STATE_ERR_UNKNOWN;
    };
    *slot = Some(state);
    LV2_STATE_SUCCESS
}

fn state_interface() -> LV2_State_Interface {
    LV2_State_Interface {
        save: Some(fake_state_save),
        restore: Some(fake_state_restore),
    }
}

/// Counts calls to a restore that must never run on a refused blob.
static COUNTED_RESTORES: AtomicU32 = AtomicU32::new(0);

unsafe extern "C" fn counted_restore(
    _instance: LV2_Handle,
    _retrieve: LV2_State_Retrieve_Function,
    _handle: lv2_sys::LV2_State_Handle,
    _flags: u32,
    _features: *const *const LV2_Feature,
) -> LV2_State_Status {
    COUNTED_RESTORES.fetch_add(1, Ordering::Relaxed);
    LV2_STATE_SUCCESS
}

fn counted_interface() -> LV2_State_Interface {
    LV2_State_Interface {
        save: None,
        restore: Some(counted_restore),
    }
}

#[test]
fn an_lv2_state_blob_round_trips_through_save_and_restore() {
    let host = state_host();
    let interface = state_interface();
    let bytes = state::save(ptr::null_mut(), &interface, host.as_ptr()).expect("save the state");
    assert_eq!(&bytes[..8], b"RWL2ST01");
    assert_eq!(
        u32::from_le_bytes(bytes[8..12].try_into().expect("the version")),
        1
    );
    assert_eq!(
        u32::from_le_bytes(bytes[12..16].try_into().expect("the count")),
        2,
        "the non-POD value and the unknown-key value were dropped"
    );
    assert!(
        !bytes
            .windows(STATE_FILE_KEY.to_bytes().len())
            .any(|window| window == STATE_FILE_KEY.to_bytes()),
        "the non-POD key reached the blob"
    );

    assert!(state::restore(
        ptr::null_mut(),
        &interface,
        &bytes,
        host.as_ptr()
    ));
    let restored = RESTORED
        .lock()
        .expect("the fake's restored state")
        .take()
        .expect("a restore");
    assert_eq!(restored.gain, 42i32.to_le_bytes());
    assert_eq!(restored.gain_size, 4);
    assert_eq!(restored.gain_type, urid::global().map(STATE_ATOM_INT));
    assert_eq!(restored.gain_flags, pod_flag(), "the flags round-trip");
    assert_eq!(restored.name, b"RigWerk");
    assert!(
        restored.absent_is_null,
        "a key the blob does not hold returns null"
    );
    assert!(
        restored.null_out_pointers_ok,
        "retrieve rejected null out-pointers"
    );
}

const STATE_URID_KEY: &CStr = c"http://rigwerk.test/state/urid";
const STATE_URID_VALUE: &CStr = c"http://rigwerk.test/state/impulse";
const STATE_LEVEL_KEY: &CStr = c"http://rigwerk.test/state/level";
const STATE_ATOM_URID: &CStr = c"http://lv2plug.in/ns/ext/atom#URID";
const STATE_ATOM_FLOAT: &CStr = c"http://lv2plug.in/ns/ext/atom#Float";

static URID_BAD_SIZE: AtomicI32 = AtomicI32::new(i32::MAX);
static URID_NULL: AtomicI32 = AtomicI32::new(i32::MAX);
static URID_UNKNOWN: AtomicI32 = AtomicI32::new(i32::MAX);
static URID_RESTORED: Mutex<Option<(u32, Vec<u8>)>> = Mutex::new(None);

unsafe extern "C" fn urid_state_save(
    _instance: LV2_Handle,
    store: LV2_State_Store_Function,
    handle: lv2_sys::LV2_State_Handle,
    _flags: u32,
    _features: *const *const LV2_Feature,
) -> LV2_State_Status {
    let Some(store) = store else {
        return LV2_STATE_ERR_UNKNOWN;
    };
    let map = urid::global();
    let pod = pod_flag();
    let urid_type = map.map(STATE_ATOM_URID);
    let id = map.map(STATE_URID_VALUE);
    let level = 1.25f32.to_ne_bytes();
    unsafe {
        store_value(
            store,
            handle,
            map.map(STATE_URID_KEY),
            &id.to_ne_bytes(),
            urid_type,
            pod,
        );
        store_value(
            store,
            handle,
            map.map(STATE_LEVEL_KEY),
            &level,
            map.map(STATE_ATOM_FLOAT),
            pod,
        );
        URID_BAD_SIZE.store(
            store_value(
                store,
                handle,
                map.map(STATE_URID_KEY),
                b"no",
                urid_type,
                pod,
            ) as i32,
            Ordering::Relaxed,
        );
        URID_NULL.store(
            store(
                handle,
                map.map(STATE_URID_KEY),
                ptr::null(),
                4,
                urid_type,
                pod,
            ) as i32,
            Ordering::Relaxed,
        );
        URID_UNKNOWN.store(
            store_value(
                store,
                handle,
                map.map(STATE_URID_KEY),
                &STATE_UNKNOWN_URID.to_ne_bytes(),
                urid_type,
                pod,
            ) as i32,
            Ordering::Relaxed,
        );
    }
    LV2_STATE_SUCCESS
}

unsafe extern "C" fn urid_state_restore(
    _instance: LV2_Handle,
    retrieve: LV2_State_Retrieve_Function,
    handle: lv2_sys::LV2_State_Handle,
    _flags: u32,
    _features: *const *const LV2_Feature,
) -> LV2_State_Status {
    let Some(retrieve) = retrieve else {
        return LV2_STATE_ERR_UNKNOWN;
    };
    let map = urid::global();
    let (mut size, mut type_, mut flags) = (0usize, 0u32, 0u32);
    let value = unsafe {
        retrieve(
            handle,
            map.map(STATE_URID_KEY),
            &mut size,
            &mut type_,
            &mut flags,
        )
    };
    if value.is_null() || size != 4 {
        return LV2_STATE_ERR_UNKNOWN;
    }
    let mut native = [0u8; 4];
    // SAFETY: the host promises `size` readable bytes, and `size` is 4.
    native.copy_from_slice(unsafe { std::slice::from_raw_parts(value as *const u8, 4) });
    let id = u32::from_ne_bytes(native);
    let level_ptr = unsafe {
        retrieve(
            handle,
            map.map(STATE_LEVEL_KEY),
            &mut size,
            &mut type_,
            &mut flags,
        )
    };
    if level_ptr.is_null() {
        return LV2_STATE_ERR_UNKNOWN;
    }
    // SAFETY: the host promises `size` readable bytes at the returned pointer.
    let level = unsafe { std::slice::from_raw_parts(level_ptr as *const u8, size) }.to_vec();
    *URID_RESTORED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((id, level));
    LV2_STATE_SUCCESS
}

/// One record's type URI and value bytes, walking the blob the state module
/// writes.  The test owns the layout check so a URID is not compared by its
/// numeric id.
fn state_record<'a>(blob: &'a [u8], key: &[u8]) -> (&'a [u8], &'a [u8]) {
    assert_eq!(&blob[..8], b"RWL2ST01");
    assert_eq!(
        u32::from_le_bytes(blob[8..12].try_into().expect("the version")),
        1
    );
    let count = u32::from_le_bytes(blob[12..16].try_into().expect("the count")) as usize;
    let mut offset = 16;
    for _ in 0..count {
        let key_len =
            u32::from_le_bytes(blob[offset..offset + 4].try_into().expect("a key length")) as usize;
        offset += 4;
        let record_key = &blob[offset..offset + key_len];
        offset += key_len;
        let type_len =
            u32::from_le_bytes(blob[offset..offset + 4].try_into().expect("a type length"))
                as usize;
        offset += 4;
        let record_type = &blob[offset..offset + type_len];
        offset += type_len;
        offset += 4;
        let value_len =
            u32::from_le_bytes(blob[offset..offset + 4].try_into().expect("a value length"))
                as usize;
        offset += 4;
        let value = &blob[offset..offset + value_len];
        offset += value_len;
        if record_key == key {
            return (record_type, value);
        }
    }
    panic!("no state record for {}", String::from_utf8_lossy(key));
}

#[test]
fn a_top_level_atom_urid_is_stored_as_its_uri() {
    let host = state_host();
    let interface = LV2_State_Interface {
        save: Some(urid_state_save),
        restore: Some(urid_state_restore),
    };
    let bytes = state::save(ptr::null_mut(), &interface, host.as_ptr()).expect("save the state");
    assert_eq!(
        u32::from_le_bytes(bytes[12..16].try_into().expect("the count")),
        2,
        "a URID the map cannot name reached the blob"
    );
    assert_eq!(
        URID_BAD_SIZE.load(Ordering::Relaxed),
        status_bits(LV2_STATE_ERR_BAD_TYPE),
        "an atom:URID that is not 4 bytes must be a bad type"
    );
    assert_eq!(
        URID_NULL.load(Ordering::Relaxed),
        status_bits(LV2_STATE_ERR_UNKNOWN),
        "a null atom:URID must be refused"
    );
    assert_eq!(
        URID_UNKNOWN.load(Ordering::Relaxed),
        status_bits(LV2_STATE_ERR_UNKNOWN),
        "an unmapped atom:URID must be refused"
    );

    let saved_id = urid::global().map(STATE_URID_VALUE);
    let (urid_type, urid_value) = state_record(&bytes, STATE_URID_KEY.to_bytes());
    assert_eq!(urid_type, STATE_ATOM_URID.to_bytes());
    assert_eq!(
        urid_value,
        STATE_URID_VALUE.to_bytes(),
        "the blob kept the numeric id"
    );
    assert_ne!(
        urid_value,
        saved_id.to_ne_bytes().as_slice(),
        "the stored value is the id from this process"
    );
    let (level_type, level_value) = state_record(&bytes, STATE_LEVEL_KEY.to_bytes());
    assert_eq!(level_type, STATE_ATOM_FLOAT.to_bytes());
    assert_eq!(level_value, 1.25f32.to_ne_bytes());

    assert!(state::restore(
        ptr::null_mut(),
        &interface,
        &bytes,
        host.as_ptr()
    ));
    let (id, level) = URID_RESTORED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .expect("the plugin's restore ran");
    assert_eq!(
        id,
        urid::global().map(STATE_URID_VALUE),
        "restore handed back a stale id"
    );
    assert_eq!(level, 1.25f32.to_ne_bytes());

    // A value that is not URI text refuses the whole blob, the way an
    // unmappable key does.  The numeric id this process stored is not a URI.
    let value = STATE_URID_VALUE.to_bytes();
    let start = bytes
        .windows(value.len())
        .position(|window| window == value)
        .expect("the stored URI");
    let mut not_utf8 = bytes.clone();
    not_utf8[start] = 0xff;
    assert!(
        !state::restore(ptr::null_mut(), &interface, &not_utf8, host.as_ptr()),
        "a non-UTF8 atom:URID was restored"
    );
    let mut interior_nul = bytes.clone();
    interior_nul[start] = 0;
    assert!(
        !state::restore(ptr::null_mut(), &interface, &interior_nul, host.as_ptr()),
        "an atom:URID with an interior NUL was restored"
    );
}

#[test]
fn the_state_calls_get_the_instance_features() {
    let host = state_host();
    let interface = state_interface();
    let bytes = state::save(ptr::null_mut(), &interface, host.as_ptr()).expect("save the state");
    assert!(
        SAVE_SAW_MAP_PATH.load(Ordering::Relaxed),
        "state:mapPath was missing from the save features"
    );
    assert!(
        SAVE_SAW_FREE_PATH.load(Ordering::Relaxed),
        "state:freePath was missing from the save features"
    );
    assert_ne!(
        SAVE_FLAGS.load(Ordering::Relaxed) & pod_flag(),
        0,
        "the save did not ask for POD values"
    );

    assert!(state::restore(
        ptr::null_mut(),
        &interface,
        &bytes,
        host.as_ptr()
    ));
    assert!(
        RESTORE_SAW_MAP_PATH.load(Ordering::Relaxed),
        "state:mapPath was missing from the restore features"
    );
    assert!(
        RESTORE_SAW_FREE_PATH.load(Ordering::Relaxed),
        "state:freePath was missing from the restore features"
    );
}

#[test]
fn an_lv2_state_blob_from_an_unknown_version_is_refused() {
    let host = state_host();
    let interface = state_interface();
    let bytes = state::save(ptr::null_mut(), &interface, host.as_ptr()).expect("save the state");
    let counted = counted_interface();
    let before = COUNTED_RESTORES.load(Ordering::Relaxed);

    let mut wrong_magic = bytes.clone();
    wrong_magic[0] = b'X';
    assert!(
        !state::restore(ptr::null_mut(), &counted, &wrong_magic, host.as_ptr()),
        "a blob with a foreign magic was accepted"
    );

    let mut wrong_version = bytes.clone();
    wrong_version[8..12].copy_from_slice(&2u32.to_le_bytes());
    assert!(
        !state::restore(ptr::null_mut(), &counted, &wrong_version, host.as_ptr()),
        "a blob from a newer layout was accepted"
    );

    let mut huge_count = bytes.clone();
    huge_count[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(
        !state::restore(ptr::null_mut(), &counted, &huge_count, host.as_ptr()),
        "an impossible record count was accepted"
    );

    let mut huge_key = bytes.clone();
    huge_key[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(
        !state::restore(ptr::null_mut(), &counted, &huge_key, host.as_ptr()),
        "a key longer than the blob was accepted"
    );

    assert_eq!(
        COUNTED_RESTORES.load(Ordering::Relaxed),
        before,
        "the plugin's restore ran on a blob it cannot read"
    );
}

#[test]
fn a_truncated_lv2_state_blob_is_refused_without_a_panic() {
    let host = state_host();
    let interface = state_interface();
    let bytes = state::save(ptr::null_mut(), &interface, host.as_ptr()).expect("save the state");
    let counted = counted_interface();
    let before = COUNTED_RESTORES.load(Ordering::Relaxed);
    for cut in [0, 3, 8, 12, 15, bytes.len() - 1] {
        assert!(
            !state::restore(ptr::null_mut(), &counted, &bytes[..cut], host.as_ptr()),
            "a blob cut at {cut} bytes was accepted"
        );
    }
    assert_eq!(
        COUNTED_RESTORES.load(Ordering::Relaxed),
        before,
        "the plugin's restore ran on a blob it cannot read"
    );
}

#[test]
fn trailing_bytes_after_a_complete_record_are_refused() {
    let host = state_host();
    let interface = state_interface();
    let mut bytes =
        state::save(ptr::null_mut(), &interface, host.as_ptr()).expect("save the state");
    bytes.push(0);
    assert!(
        !state::restore(ptr::null_mut(), &interface, &bytes, host.as_ptr()),
        "a blob with bytes past its last record was accepted"
    );
}

#[test]
fn a_rejected_blob_does_not_add_uris_to_the_process_map() {
    let host = state_host();
    let interface = state_interface();
    // A URI with the same length as the fake's gain key, so it can replace it
    // in a saved blob, and one nothing has mapped yet.
    let fresh = c"http://rigwerk.test/state/xain";
    assert_eq!(fresh.to_bytes().len(), STATE_GAIN_KEY.to_bytes().len());
    assert!(!urid::global().contains_for_test(fresh));

    let mut bytes =
        state::save(ptr::null_mut(), &interface, host.as_ptr()).expect("save the state");
    let start = bytes
        .windows(fresh.to_bytes().len())
        .position(|window| window == STATE_GAIN_KEY.to_bytes())
        .expect("the gain key in the blob");
    bytes[start..start + fresh.to_bytes().len()].copy_from_slice(fresh.to_bytes());
    // A complete record, but the blob as a whole does not parse.
    bytes.push(0);

    assert!(!state::restore(
        ptr::null_mut(),
        &interface,
        &bytes,
        host.as_ptr()
    ));
    assert!(
        !urid::global().contains_for_test(fresh),
        "a refused blob mapped a URI"
    );
}

#[test]
fn a_non_pod_state_value_is_dropped_from_the_blob() {
    let host = state_host();
    let interface = state_interface();
    let bytes = state::save(ptr::null_mut(), &interface, host.as_ptr()).expect("save the state");
    // Only the two POD values, gain and name, are in the blob.
    assert_eq!(
        u32::from_le_bytes(bytes[12..16].try_into().expect("the count")),
        2
    );
    assert!(
        !bytes
            .windows(b"ir.wav".len())
            .any(|window| window == b"ir.wav"),
        "the non-POD value's bytes reached the blob"
    );
}

/// The statuses a save got for values that must not be stored.
static NON_POD_STATUS: AtomicI32 = AtomicI32::new(i32::MAX);
static NATIVE_STATUS: AtomicI32 = AtomicI32::new(i32::MAX);
static NULL_VALUE_STATUS: AtomicI32 = AtomicI32::new(i32::MAX);

unsafe extern "C" fn odd_values_save(
    _instance: LV2_Handle,
    store: LV2_State_Store_Function,
    handle: lv2_sys::LV2_State_Handle,
    _flags: u32,
    _features: *const *const LV2_Feature,
) -> LV2_State_Status {
    let Some(store) = store else {
        return LV2_STATE_ERR_UNKNOWN;
    };
    let map = urid::global();
    let int = map.map(STATE_ATOM_INT);
    let gain = map.map(STATE_GAIN_KEY);
    let name = map.map(STATE_NAME_KEY);
    unsafe {
        NON_POD_STATUS.store(
            store_value(store, handle, gain, b"ir.wav", int, 0) as i32,
            Ordering::Relaxed,
        );
        NATIVE_STATUS.store(
            store_value(
                store,
                handle,
                gain,
                &1i32.to_le_bytes(),
                int,
                pod_flag() | native_flag(),
            ) as i32,
            Ordering::Relaxed,
        );
        NULL_VALUE_STATUS.store(
            store(
                handle,
                name,
                ptr::null(),
                std::mem::size_of::<i32>(),
                int,
                pod_flag(),
            ) as i32,
            Ordering::Relaxed,
        );
    }
    // The plugin carries on, as a plugin that ignores a failed store does.
    LV2_STATE_SUCCESS
}

#[test]
fn a_bad_state_value_gets_the_status_that_says_why() {
    let host = state_host();
    let interface = LV2_State_Interface {
        save: Some(odd_values_save),
        restore: None,
    };
    let bytes = state::save(ptr::null_mut(), &interface, host.as_ptr()).expect("save the state");
    assert_eq!(
        u32::from_le_bytes(bytes[12..16].try_into().expect("the count")),
        0,
        "a value that cannot be stored reached the blob"
    );
    assert_eq!(
        NON_POD_STATUS.load(Ordering::Relaxed),
        status_bits(LV2_STATE_ERR_BAD_FLAGS),
        "a non-POD value must be refused as bad flags"
    );
    assert_eq!(
        NATIVE_STATUS.load(Ordering::Relaxed),
        status_bits(LV2_STATE_ERR_BAD_FLAGS),
        "a native value must be refused as bad flags"
    );
    assert_eq!(
        NULL_VALUE_STATUS.load(Ordering::Relaxed),
        status_bits(LV2_STATE_ERR_UNKNOWN),
        "a null value with a length must be refused"
    );
}

#[test]
fn an_empty_state_blob_saves_and_restores() {
    static EMPTY_RESTORES: AtomicU32 = AtomicU32::new(0);
    unsafe extern "C" fn empty_save(
        _instance: LV2_Handle,
        _store: LV2_State_Store_Function,
        _handle: lv2_sys::LV2_State_Handle,
        _flags: u32,
        _features: *const *const LV2_Feature,
    ) -> LV2_State_Status {
        LV2_STATE_SUCCESS
    }
    unsafe extern "C" fn empty_restore(
        _instance: LV2_Handle,
        _retrieve: LV2_State_Retrieve_Function,
        _handle: lv2_sys::LV2_State_Handle,
        _flags: u32,
        _features: *const *const LV2_Feature,
    ) -> LV2_State_Status {
        EMPTY_RESTORES.fetch_add(1, Ordering::Relaxed);
        LV2_STATE_SUCCESS
    }

    let host = state_host();
    let interface = LV2_State_Interface {
        save: Some(empty_save),
        restore: Some(empty_restore),
    };
    let bytes =
        state::save(ptr::null_mut(), &interface, host.as_ptr()).expect("save an empty state");
    assert_eq!(bytes.len(), 16, "magic, version and count only");
    assert_eq!(
        u32::from_le_bytes(bytes[12..16].try_into().expect("the count")),
        0
    );
    assert!(state::restore(
        ptr::null_mut(),
        &interface,
        &bytes,
        host.as_ptr()
    ));
    assert_eq!(EMPTY_RESTORES.load(Ordering::Relaxed), 1);
}

/// What the empty-value restore read: size, type, flags, and whether the
/// pointer was null.
static EMPTY_RETRIEVED: Mutex<Option<(usize, u32, u32, bool)>> = Mutex::new(None);

unsafe extern "C" fn empty_value_save(
    _instance: LV2_Handle,
    store: LV2_State_Store_Function,
    handle: lv2_sys::LV2_State_Handle,
    _flags: u32,
    _features: *const *const LV2_Feature,
) -> LV2_State_Status {
    let Some(store) = store else {
        return LV2_STATE_ERR_UNKNOWN;
    };
    let map = urid::global();
    // A null pointer with size 0 is what a plugin's empty default can look
    // like; it must be stored, not refused.
    unsafe {
        store(
            handle,
            map.map(STATE_NAME_KEY),
            ptr::null(),
            0,
            map.map(STATE_ATOM_STRING),
            pod_flag(),
        )
    }
}

unsafe extern "C" fn empty_value_restore(
    _instance: LV2_Handle,
    retrieve: LV2_State_Retrieve_Function,
    handle: lv2_sys::LV2_State_Handle,
    _flags: u32,
    _features: *const *const LV2_Feature,
) -> LV2_State_Status {
    let Some(retrieve) = retrieve else {
        return LV2_STATE_ERR_UNKNOWN;
    };
    let map = urid::global();
    let (mut size, mut type_, mut flags) = (usize::MAX, 0u32, 0u32);
    let value = unsafe {
        retrieve(
            handle,
            map.map(STATE_NAME_KEY),
            &mut size,
            &mut type_,
            &mut flags,
        )
    };
    if value.is_null() {
        return LV2_STATE_ERR_UNKNOWN;
    }
    let Ok(mut slot) = EMPTY_RETRIEVED.lock() else {
        return LV2_STATE_ERR_UNKNOWN;
    };
    *slot = Some((size, type_, flags, value.is_null()));
    LV2_STATE_SUCCESS
}

#[test]
fn an_empty_state_value_saves_and_restores() {
    let host = state_host();
    let interface = LV2_State_Interface {
        save: Some(empty_value_save),
        restore: Some(empty_value_restore),
    };
    let bytes =
        state::save(ptr::null_mut(), &interface, host.as_ptr()).expect("save an empty value");
    assert_eq!(
        u32::from_le_bytes(bytes[12..16].try_into().expect("the count")),
        1,
        "the empty value was not stored"
    );
    // The record ends in a zero value length.
    assert_eq!(&bytes[bytes.len() - 4..], &0u32.to_le_bytes());

    *EMPTY_RETRIEVED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    assert!(state::restore(
        ptr::null_mut(),
        &interface,
        &bytes,
        host.as_ptr()
    ));
    let (size, type_, flags, null) = EMPTY_RETRIEVED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .expect("the empty value was not retrieved");
    assert_eq!(size, 0);
    assert_eq!(type_, urid::global().map(STATE_ATOM_STRING));
    assert_eq!(flags, pod_flag());
    assert!(!null, "an empty value must not look absent");
}

/// A blob with the same key twice must restore the later value.
static DUPLICATE_VALUE: Mutex<Option<Vec<u8>>> = Mutex::new(None);

unsafe extern "C" fn duplicate_save(
    _instance: LV2_Handle,
    store: LV2_State_Store_Function,
    handle: lv2_sys::LV2_State_Handle,
    _flags: u32,
    _features: *const *const LV2_Feature,
) -> LV2_State_Status {
    let Some(store) = store else {
        return LV2_STATE_ERR_UNKNOWN;
    };
    let map = urid::global();
    let key = map.map(STATE_DUPLICATE_KEY);
    let int = map.map(STATE_ATOM_INT);
    unsafe {
        store_value(store, handle, key, &42i32.to_le_bytes(), int, pod_flag());
        store_value(store, handle, key, &43i32.to_le_bytes(), int, pod_flag());
    }
    LV2_STATE_SUCCESS
}

unsafe extern "C" fn duplicate_restore(
    _instance: LV2_Handle,
    retrieve: LV2_State_Retrieve_Function,
    handle: lv2_sys::LV2_State_Handle,
    _flags: u32,
    _features: *const *const LV2_Feature,
) -> LV2_State_Status {
    let Some(retrieve) = retrieve else {
        return LV2_STATE_ERR_UNKNOWN;
    };
    let mut size = 0usize;
    let (mut type_, mut flags) = (0u32, 0u32);
    let value = unsafe {
        retrieve(
            handle,
            urid::global().map(STATE_DUPLICATE_KEY),
            &mut size,
            &mut type_,
            &mut flags,
        )
    };
    if value.is_null() {
        return LV2_STATE_ERR_UNKNOWN;
    }
    // SAFETY: the host promises `size` readable bytes at the returned pointer.
    let bytes = unsafe { std::slice::from_raw_parts(value as *const u8, size) }.to_vec();
    let Ok(mut slot) = DUPLICATE_VALUE.lock() else {
        return LV2_STATE_ERR_UNKNOWN;
    };
    *slot = Some(bytes);
    LV2_STATE_SUCCESS
}

#[test]
fn a_duplicate_state_key_restores_the_last_value() {
    let host = state_host();
    let interface = LV2_State_Interface {
        save: Some(duplicate_save),
        restore: Some(duplicate_restore),
    };
    let bytes = state::save(ptr::null_mut(), &interface, host.as_ptr()).expect("save the state");
    assert_eq!(
        u32::from_le_bytes(bytes[12..16].try_into().expect("the count")),
        2,
        "both properties are kept in the blob"
    );
    assert!(state::restore(
        ptr::null_mut(),
        &interface,
        &bytes,
        host.as_ptr()
    ));
    let value = DUPLICATE_VALUE
        .lock()
        .expect("the fake's duplicate value")
        .take();
    assert_eq!(
        value.as_deref(),
        Some(&43i32.to_le_bytes()[..]),
        "the later property of a duplicate key wins"
    );
}

/// The status [`oversized_save`] got from the store callback.
static OVERSIZE_STATUS: AtomicI32 = AtomicI32::new(i32::MAX);

unsafe extern "C" fn oversized_save(
    _instance: LV2_Handle,
    store: LV2_State_Store_Function,
    handle: lv2_sys::LV2_State_Handle,
    _flags: u32,
    _features: *const *const LV2_Feature,
) -> LV2_State_Status {
    let Some(store) = store else {
        return LV2_STATE_ERR_UNKNOWN;
    };
    let value = vec![0u8; MAX_STATE_BLOB];
    let map = urid::global();
    // SAFETY: the fake's caller passes a live store callback and its handle.
    let status = unsafe {
        store(
            handle,
            map.map(STATE_GAIN_KEY),
            value.as_ptr() as *const c_void,
            value.len(),
            map.map(STATE_ATOM_INT),
            pod_flag(),
        )
    };
    OVERSIZE_STATUS.store(status_bits(status), Ordering::Relaxed);
    // A plugin that cannot fall back passes the failure on.
    status
}

#[test]
fn a_state_value_larger_than_the_cap_is_refused() {
    let host = state_host();
    let interface = LV2_State_Interface {
        save: Some(oversized_save),
        restore: None,
    };
    let error =
        state::save(ptr::null_mut(), &interface, host.as_ptr()).expect_err("an oversized save");
    assert!(error.contains("larger than"), "{error}");
    assert_eq!(
        OVERSIZE_STATUS.load(Ordering::Relaxed),
        status_bits(LV2_STATE_ERR_NO_SPACE),
        "the store callback did not report the missing space"
    );
}

/// A plugin that refuses every save, and the last status it returned.
static REFUSED_STATUS: AtomicI32 = AtomicI32::new(i32::MAX);

unsafe extern "C" fn refusing_save(
    _instance: LV2_Handle,
    _store: LV2_State_Store_Function,
    _handle: lv2_sys::LV2_State_Handle,
    _flags: u32,
    _features: *const *const LV2_Feature,
) -> LV2_State_Status {
    REFUSED_STATUS.store(status_bits(LV2_STATE_ERR_NO_FEATURE), Ordering::Relaxed);
    LV2_STATE_ERR_NO_FEATURE
}

#[test]
fn a_plugin_that_handles_no_space_itself_still_saves() {
    // The plugin sees `ERR_NO_SPACE` for one value, stores a smaller one and
    // reports success: the host must not latch its own overflow.
    unsafe extern "C" fn retrying_save(
        _instance: LV2_Handle,
        store: LV2_State_Store_Function,
        handle: lv2_sys::LV2_State_Handle,
        _flags: u32,
        _features: *const *const LV2_Feature,
    ) -> LV2_State_Status {
        let Some(store) = store else {
            return LV2_STATE_ERR_UNKNOWN;
        };
        let map = urid::global();
        let key = map.map(STATE_GAIN_KEY);
        let int = map.map(STATE_ATOM_INT);
        let too_big = vec![0u8; MAX_STATE_BLOB];
        let status = unsafe {
            store(
                handle,
                key,
                too_big.as_ptr() as *const c_void,
                too_big.len(),
                int,
                pod_flag(),
            )
        };
        if status != LV2_STATE_ERR_NO_SPACE {
            return LV2_STATE_ERR_UNKNOWN;
        }
        let smaller = &1i32.to_le_bytes();
        unsafe {
            store_value(store, handle, key, smaller, int, pod_flag());
        }
        LV2_STATE_SUCCESS
    }

    let host = state_host();
    let interface = LV2_State_Interface {
        save: Some(retrying_save),
        restore: None,
    };
    let bytes = state::save(ptr::null_mut(), &interface, host.as_ptr())
        .expect("a plugin that fell back to a smaller value must save");
    assert_eq!(
        u32::from_le_bytes(bytes[12..16].try_into().expect("the count")),
        1
    );
}

#[test]
fn a_failed_save_names_the_status() {
    let host = state_host();
    let interface = LV2_State_Interface {
        save: Some(refusing_save),
        restore: None,
    };
    let error =
        state::save(ptr::null_mut(), &interface, host.as_ptr()).expect_err("a refused save");
    assert!(error.contains("ERR_NO_FEATURE"), "{error}");
    assert_eq!(
        REFUSED_STATUS.load(Ordering::Relaxed),
        status_bits(LV2_STATE_ERR_NO_FEATURE)
    );
}

#[test]
fn a_state_save_without_an_interface_is_an_error() {
    let host = state_host();
    let missing = ptr::null::<LV2_State_Interface>();
    assert!(state::save(ptr::null_mut(), missing, host.as_ptr()).is_err());
    let empty = LV2_State_Interface {
        save: None,
        restore: None,
    };
    assert!(state::save(ptr::null_mut(), &empty, host.as_ptr()).is_err());
    assert!(!state::restore(
        ptr::null_mut(),
        missing,
        &[0u8; 16],
        host.as_ptr()
    ));

    // A present interface without a restore function is refused too.
    let interface = state_interface();
    let bytes = state::save(ptr::null_mut(), &interface, host.as_ptr()).expect("save the state");
    let no_restore = LV2_State_Interface {
        save: None,
        restore: None,
    };
    assert!(!state::restore(
        ptr::null_mut(),
        &no_restore,
        &bytes,
        host.as_ptr()
    ));
}

#[test]
fn an_lv2_instance_saves_through_its_state_interface() {
    let table = params_table();
    let shared = Arc::new(PluginShared::new(0));
    let core = Arc::new(Lv2Core::for_test(true));
    let ranges: Arc<[super::ports::Lv2Range]> = Arc::from(table.ranges.clone().into_boxed_slice());
    let values: Vec<f32> = table
        .ranges
        .iter()
        .map(|range| range.default as f32)
        .collect();
    let mut instance =
        Lv2Instance::for_test(ranges, values, Arc::clone(&shared), Arc::clone(&core));
    assert_eq!(
        instance.save_state(),
        StateSave::Unavailable,
        "a plugin without a state interface saves nothing"
    );

    let interface = state_interface();
    instance.set_state_interface_for_test(&interface);
    match instance.save_state() {
        StateSave::Saved(bytes) => {
            assert_eq!(&bytes[..8], b"RWL2ST01");
            assert_eq!(
                u32::from_le_bytes(bytes[12..16].try_into().expect("the count")),
                2
            );
        }
        other => panic!("the state interface was not used: {other:?}"),
    }
}

// Real LV2 plugins: the LV2 repository's examples, built by
// `scripts/lv2-fixtures.sh` (see `crate::audio::plugins::fixtures`).  Each
// test goes through `Lv2Instance::load` with the preference a browser add
// would save, not through a fake descriptor, so it covers the binary, the
// parsed ports and the host features together.  With `RIGWERK_LV2_FIXTURES`
// unset they return at once.

const EG_AMP_URI: &str = "http://lv2plug.in/plugins/eg-amp";
const EG_PARAMS_URI: &str = "http://lv2plug.in/plugins/eg-params";

/// The block the fixture tests process: well under [`MAX_BLOCK`].
const FIXTURE_BLOCK: usize = 64;

/// The saved effect a browser add would store for fixture `name`, or `None`
/// when the fixtures are not configured.
fn fixture_pref(name: &str, uri: &str) -> Option<PluginPref> {
    let path = lv2_fixture(name)?;
    Some(PluginPref {
        path: path.to_string_lossy().into_owned(),
        format: PluginFormat::Lv2,
        class_id: Some(uri.to_owned()),
        name: name.to_owned(),
        ..PluginPref::default()
    })
}

#[test]
fn lv2_fixture_loads_processes_and_reads_eg_amps_gain() {
    let Some(pref) = fixture_pref("eg-amp", EG_AMP_URI) else {
        return;
    };
    let bundle = PathBuf::from(&pref.path);
    let shared = Arc::new(PluginShared::new(0));
    let mut loaded = Lv2Instance::load(
        &bundle,
        EG_AMP_URI,
        48_000,
        MAX_BLOCK,
        &Restore::from_pref(&pref),
        &shared,
    )
    .expect("load eg-amp");

    let gain = loaded
        .views
        .iter()
        .find(|view| view.id == symbol_id("gain"))
        .expect("eg-amp's gain control");
    assert!(
        gain.display.contains("dB"),
        "the gain panel shows {:?}, not dB",
        gain.display
    );

    // A unity impulse: eg-amp's 0 dB default passes it through.
    let mut left = vec![0.0f32; FIXTURE_BLOCK];
    left[0] = 1.0;
    let mut right = left.clone();
    loaded
        .processor
        .process(&mut left, &mut right)
        .expect("process eg-amp");
    assert!(
        left.iter().chain(&right).all(|sample| sample.is_finite()),
        "the output is not finite"
    );
    assert!((left[0] - 1.0).abs() < 1e-6, "unity gain wrote {}", left[0]);
    assert!(
        left[1..].iter().all(|sample| sample.abs() < 1e-9),
        "an impulse spread past its frame"
    );

    // 0.0 normalized is the gain minimum (-90 dB), at which eg-amp's
    // `DB_CO` writes silence.
    let index = loaded
        .ids
        .index_of(symbol_id("gain"))
        .expect("the gain's range index");
    assert!(loaded.out.push(index, 0.0), "queue the gain minimum");
    let mut left = vec![0.0f32; FIXTURE_BLOCK];
    left[0] = 1.0;
    let mut right = left.clone();
    loaded
        .processor
        .process(&mut left, &mut right)
        .expect("process the gain minimum");
    assert!(
        left.iter().chain(&right).all(|sample| sample.abs() < 1e-6),
        "the minimum gain is not silent: {:?}",
        &left[..4]
    );
}

#[test]
fn lv2_fixture_saves_and_restores_eg_params_state() {
    let Some(pref) = fixture_pref("eg-params", EG_PARAMS_URI) else {
        return;
    };
    let bundle = PathBuf::from(&pref.path);
    let shared = Arc::new(PluginShared::new(0));
    let loaded = Lv2Instance::load(
        &bundle,
        EG_PARAMS_URI,
        48_000,
        MAX_BLOCK,
        &Restore::from_pref(&pref),
        &shared,
    )
    .expect("load eg-params");

    let StateSave::Saved(bytes) = loaded.instance.save_state() else {
        panic!("eg-params has a state interface but saved nothing");
    };
    assert_eq!(&bytes[..8], b"RWL2ST01", "the blob has no magic");
    assert_eq!(
        u32::from_le_bytes(bytes[8..12].try_into().expect("the version")),
        1
    );
    assert_eq!(
        u32::from_le_bytes(bytes[12..16].try_into().expect("the count")),
        9,
        "eg-params saved a different number of properties"
    );

    // A session restore: the saved blob reopens and the load reports it
    // current.  eg-params has only atom ports, so the browser does not list
    // it; loading it directly is what covers state, mapPath and the blob.
    let mut restored = pref.clone();
    restored.state = Some(PluginState::capture(&bytes, &[]));
    let reloaded = Lv2Instance::load(
        &bundle,
        EG_PARAMS_URI,
        48_000,
        MAX_BLOCK,
        &Restore::from_pref(&restored),
        &shared,
    )
    .expect("reload eg-params with its saved state");
    assert!(
        reloaded.state_current,
        "the restored blob was not reported current"
    );

    // The browser filter on a real bundle: eg-amp has an audio input and
    // output, eg-params has neither.
    let amp = lv2_fixture("eg-amp").expect("eg-amp is built with eg-params");
    assert_eq!(
        scan_bundle(&amp).expect("scan eg-amp").len(),
        1,
        "eg-amp is not listed"
    );
    assert!(
        scan_bundle(&bundle).expect("scan eg-params").is_empty(),
        "eg-params without audio ports was listed"
    );
}

const EG_SAMPLER_URI: &str = "http://lv2plug.in/plugins/eg-sampler";

/// Appends one atom (`type_`, `body`) at frame 0 to the input sequence `seq`,
/// which a block's re-arm left empty, padding to 8 bytes as `atom.h` asks.
fn push_event(seq: *mut lv2_sys::LV2_Atom_Sequence, type_: u32, body: &[u8]) {
    let padded = body.len().div_ceil(8) * 8;
    unsafe {
        let used = (*seq).atom.size as usize;
        let start = (seq as *mut u8).add(8 + used);
        let event = start as *mut lv2_sys::LV2_Atom_Event;
        (*event).time.frames = 0;
        (*event).body.size = body.len() as u32;
        (*event).body.type_ = type_;
        let data = start.add(std::mem::size_of::<lv2_sys::LV2_Atom_Event>());
        ptr::copy_nonoverlapping(body.as_ptr(), data, body.len());
        ptr::write_bytes(data.add(body.len()), 0, padded - body.len());
        (*seq).atom.size += (std::mem::size_of::<lv2_sys::LV2_Atom_Event>() + padded) as u32;
    }
}

/// The body of a `patch:Set` object for eg-sampler's `eg:sample` to `path`.
fn sample_set_body(path: &str) -> Vec<u8> {
    let map = urid::global();
    let urid = |uri: &CStr| map.map(uri).to_ne_bytes();
    let mut body = Vec::new();
    body.extend(0u32.to_ne_bytes());
    body.extend(urid(c"http://lv2plug.in/ns/ext/patch#Set"));
    // patch:property = eg:sample, as an atom:URID.
    body.extend(urid(c"http://lv2plug.in/ns/ext/patch#property"));
    body.extend(0u32.to_ne_bytes());
    body.extend(4u32.to_ne_bytes());
    body.extend(urid(c"http://lv2plug.in/ns/ext/atom#URID"));
    body.extend(urid(c"http://lv2plug.in/plugins/eg-sampler#sample"));
    body.extend([0u8; 4]);
    // patch:value = the path, as an atom:Path with its NUL.
    let mut value = path.as_bytes().to_vec();
    value.push(0);
    body.extend(urid(c"http://lv2plug.in/ns/ext/patch#value"));
    body.extend(0u32.to_ne_bytes());
    body.extend((value.len() as u32).to_ne_bytes());
    body.extend(urid(c"http://lv2plug.in/ns/ext/atom#Path"));
    let padded = value.len().div_ceil(8) * 8;
    value.resize(padded, 0);
    body.extend(value);
    body
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// eg-sampler requires `state:loadDefaultState` and `worker:schedule`: the
/// load restores its default sample, a `patch:Set` from `run` loads another
/// through the worker thread, and the saved state names it and restores.
/// It has no audio input, so it is loaded directly, as eg-params is.
#[test]
fn lv2_fixture_loads_a_sample_through_eg_samplers_worker() {
    let Some(pref) = fixture_pref("eg-sampler", EG_SAMPLER_URI) else {
        return;
    };
    let _serial = worker_tests();
    let bundle = PathBuf::from(&pref.path);
    let shared = Arc::new(PluginShared::new(0));
    let mut loaded = Lv2Instance::load(
        &bundle,
        EG_SAMPLER_URI,
        48_000,
        FIXTURE_BLOCK,
        &Restore::from_pref(&pref),
        &shared,
    )
    .expect("load eg-sampler");
    let StateSave::Saved(bytes) = loaded.instance.save_state() else {
        panic!("eg-sampler saved nothing");
    };
    assert!(
        contains(&bytes, b"click.wav"),
        "the default state did not load click.wav"
    );

    // Another sample, so the change is visible in the saved state.
    let dir = std::env::temp_dir().join(format!("rigwerk-eg-sampler-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the sample folder");
    let other = dir.join("other.wav");
    std::fs::copy(bundle.join("click.wav"), &other).expect("copy the sample");

    let processor = &mut loaded.processor;
    let control = processor
        .buffers
        .slots
        .iter()
        .position(|slot| slot.input && matches!(slot.buffer, Buffer::Atom(_)))
        .expect("eg-sampler's control port");
    processor.buffers.reset_atoms();
    let sequence = processor
        .buffers
        .atom(control)
        .expect("the control sequence");
    push_event(
        sequence,
        urid::global().map(c"http://lv2plug.in/ns/ext/atom#Object"),
        &sample_set_body(&other.to_string_lossy()),
    );
    // The run that reads the message schedules the load.
    unsafe { (processor.run)(processor.handle, FIXTURE_BLOCK as u32) };
    let rings = Arc::clone(&loaded.instance.core._rings);
    for _ in 0..2000 {
        if rings.response_pending() {
            break;
        }
        loaded.instance.service_worker();
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(rings.response_pending(), "the worker sent no response");
    // The next block delivers it, and the plugin swaps the sample in.
    let mut left = vec![0.0f32; FIXTURE_BLOCK];
    let mut right = left.clone();
    loaded
        .processor
        .process(&mut left, &mut right)
        .expect("process eg-sampler");
    assert!(left.iter().chain(&right).all(|sample| sample.is_finite()));
    let StateSave::Saved(bytes) = loaded.instance.save_state() else {
        panic!("eg-sampler saved nothing after the change");
    };
    assert!(
        contains(&bytes, b"other.wav"),
        "the sample the worker loaded is not in the state"
    );

    // A session restore loads the saved sample, not the default.
    let mut restored = pref.clone();
    restored.state = Some(PluginState::capture(&bytes, &[]));
    drop(loaded);
    let reloaded = Lv2Instance::load(
        &bundle,
        EG_SAMPLER_URI,
        48_000,
        FIXTURE_BLOCK,
        &Restore::from_pref(&restored),
        &shared,
    )
    .expect("reload eg-sampler with its saved state");
    assert!(reloaded.state_current, "the saved state was not current");
    let StateSave::Saved(again) = reloaded.instance.save_state() else {
        panic!("the reloaded eg-sampler saved nothing");
    };
    assert!(
        contains(&again, b"other.wav"),
        "the restore lost the sample"
    );
    drop(reloaded);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(registered_for_test(), 0, "eg-sampler stayed registered");
}

/// The port an optional port of an unknown kind must be left as: NULL.  LV2
/// does not allow the host to skip `connect_port` for a declared port.
#[test]
fn an_optional_port_of_an_unknown_kind_is_connected_to_null() {
    static SEEN: AtomicUsize = AtomicUsize::new(usize::MAX);
    unsafe extern "C" fn connect(_handle: LV2_Handle, _port: u32, data: *mut c_void) {
        SEEN.store(data as usize, Ordering::SeqCst);
    }

    let table = PortTable {
        ports: vec![PortInfo {
            index: 0,
            kind: PortKind::Other,
            input: true,
            side_chain: false,
            capacity: 0,
            default: 0.0,
        }],
        ranges: Vec::new(),
        ids: Arc::new(ParamIds::new(Vec::new())),
        latency: None,
    };
    let mut buffers = PortBuffers::new(&table, 8);
    let mut descriptor: LV2_Descriptor = unsafe { std::mem::zeroed() };
    descriptor.connect_port = Some(connect);
    SEEN.store(1, Ordering::SeqCst);
    buffers.connect(
        std::ptr::NonNull::<c_void>::dangling().as_ptr(),
        &descriptor,
    );
    assert_eq!(
        SEEN.load(Ordering::SeqCst),
        0,
        "the port was not connected to NULL"
    );
}

// A fake plugin that keeps the bundle path `instantiate` gave it, as LV2
// allows, and reads it again in `cleanup`.
static KEPT_PATH: AtomicUsize = AtomicUsize::new(0);
static PATH_AT_CLEANUP: Mutex<Option<String>> = Mutex::new(None);
static PATH_KEEPER: u8 = 0;

unsafe extern "C" fn path_keeping_instantiate(
    _descriptor: *const LV2_Descriptor,
    _rate: f64,
    bundle_path: *const std::ffi::c_char,
    _features: *const *const LV2_Feature,
) -> LV2_Handle {
    KEPT_PATH.store(bundle_path as usize, Ordering::SeqCst);
    &PATH_KEEPER as *const u8 as LV2_Handle
}

unsafe extern "C" fn ignored_connect(_handle: LV2_Handle, _port: u32, _data: *mut c_void) {}

unsafe extern "C" fn ignored_run(_handle: LV2_Handle, _frames: u32) {}

unsafe extern "C" fn path_reading_cleanup(_handle: LV2_Handle) {
    let path = KEPT_PATH.load(Ordering::SeqCst) as *const std::ffi::c_char;
    // SAFETY: the host keeps the path alive until after `cleanup`.
    let text = unsafe { CStr::from_ptr(path) }
        .to_string_lossy()
        .into_owned();
    *PATH_AT_CLEANUP.lock().expect("the cleanup record") = Some(text);
}

/// The bundle path a plugin was instantiated with stays valid until after
/// `cleanup`: the core owns it, as lilv does.
#[test]
fn the_bundle_path_outlives_the_plugin() {
    let descriptor = LV2_Descriptor {
        URI: c"http://example.com/lv2/params".as_ptr(),
        instantiate: Some(path_keeping_instantiate),
        connect_port: Some(ignored_connect),
        activate: None,
        run: Some(ignored_run),
        deactivate: None,
        cleanup: Some(path_reading_cleanup),
        extension_data: None,
    };
    let pref = PluginPref::default();
    let shared = Arc::new(PluginShared::new(0));
    let loaded = Lv2Instance::load_descriptor(
        &params_bundle(),
        params_table(),
        &descriptor,
        None,
        None,
        48_000.0,
        64,
        &Restore::from_pref(&pref),
        &shared,
    )
    .expect("load the fake plugin");
    assert_eq!(
        KEPT_PATH.load(Ordering::SeqCst),
        loaded.instance.core._bundle_path.as_ptr() as usize,
        "the plugin was given a path the core does not own"
    );
    drop(loaded);
    let mut expected = params_bundle().to_string_lossy().into_owned();
    expected.push(std::path::MAIN_SEPARATOR);
    assert_eq!(
        PATH_AT_CLEANUP.lock().expect("the cleanup record").take(),
        Some(expected)
    );
}

// A fake plugin that remembers where its gain control was connected and
// reads it in `deactivate` and `cleanup`, as a plugin may.
static GAIN_PORT: AtomicUsize = AtomicUsize::new(0);
static GAIN_AT_CLEANUP: Mutex<Option<f32>> = Mutex::new(None);
static PORT_KEEPER: u8 = 0;

unsafe extern "C" fn port_keeping_instantiate(
    _descriptor: *const LV2_Descriptor,
    _rate: f64,
    _bundle_path: *const std::ffi::c_char,
    _features: *const *const LV2_Feature,
) -> LV2_Handle {
    &PORT_KEEPER as *const u8 as LV2_Handle
}

unsafe extern "C" fn gain_keeping_connect(_handle: LV2_Handle, port: u32, data: *mut c_void) {
    if port == 4 {
        GAIN_PORT.store(data as usize, Ordering::SeqCst);
    }
}

unsafe extern "C" fn gain_reading_cleanup(_handle: LV2_Handle) {
    let gain = GAIN_PORT.load(Ordering::SeqCst) as *const f32;
    // SAFETY: the host frees the port buffers only after `cleanup`.
    *GAIN_AT_CLEANUP.lock().expect("the cleanup record") = Some(unsafe { *gain });
}

/// The port buffers outlive `cleanup` even when the processor goes first
/// and the instance keeps the core: the processor hands them to the core.
#[test]
fn the_port_buffers_outlive_cleanup() {
    let descriptor = LV2_Descriptor {
        URI: c"http://example.com/lv2/params".as_ptr(),
        instantiate: Some(port_keeping_instantiate),
        connect_port: Some(gain_keeping_connect),
        activate: None,
        run: Some(ignored_run),
        deactivate: None,
        cleanup: Some(gain_reading_cleanup),
        extension_data: None,
    };
    let pref = PluginPref::default();
    let shared = Arc::new(PluginShared::new(0));
    let Lv2Loaded {
        instance,
        mut processor,
        ..
    } = Lv2Instance::load_descriptor(
        &params_bundle(),
        params_table(),
        &descriptor,
        None,
        None,
        48_000.0,
        64,
        &Restore::from_pref(&pref),
        &shared,
    )
    .expect("load the fake plugin");
    let gain = table_slot(&processor, "gain");
    let connected = match &mut processor.buffers.slots[gain].buffer {
        Buffer::Control(samples) => samples.as_mut_ptr() as usize,
        _ => panic!("the gain has no control buffer"),
    };
    assert_eq!(GAIN_PORT.load(Ordering::SeqCst), connected);

    drop(processor);
    let core = Arc::clone(&instance.core);
    assert!(
        core.retired
            .get()
            .is_some_and(|buffers| buffers.control(gain) == 1.0),
        "the processor did not hand its buffers to the core"
    );
    drop(instance);
    assert!(GAIN_AT_CLEANUP.lock().expect("the record").is_none());
    drop(core);
    assert_eq!(
        GAIN_AT_CLEANUP.lock().expect("the cleanup record").take(),
        Some(1.0),
        "cleanup did not see the gain's buffer"
    );
}

// A fake plugin that keeps the `worker:schedule` feature it was given and
// schedules one request from its first `run`, the load's zero-frame one.
static SCHEDULE_FEATURE: AtomicUsize = AtomicUsize::new(0);
static ZERO_RUN_STATUS: AtomicI64 = AtomicI64::new(i64::MAX);
static SCHEDULER: u8 = 0;

unsafe extern "C" fn schedule_keeping_instantiate(
    _descriptor: *const LV2_Descriptor,
    _rate: f64,
    _bundle_path: *const std::ffi::c_char,
    features: *const *const LV2_Feature,
) -> LV2_Handle {
    let mut index = 0;
    loop {
        let feature = unsafe { *features.add(index) };
        if feature.is_null() {
            break;
        }
        if unsafe { CStr::from_ptr((*feature).URI) } == c"http://lv2plug.in/ns/ext/worker#schedule"
        {
            SCHEDULE_FEATURE.store(unsafe { (*feature).data } as usize, Ordering::SeqCst);
        }
        index += 1;
    }
    &SCHEDULER as *const u8 as LV2_Handle
}

/// Calls the kept `schedule_work` with `message`.
fn schedule_kept(message: &[u8]) -> LV2_Worker_Status {
    let schedule = SCHEDULE_FEATURE.load(Ordering::SeqCst) as *const LV2_Worker_Schedule;
    let schedule = unsafe { schedule.as_ref() }.expect("the plugin kept worker:schedule");
    let schedule_work = schedule.schedule_work.expect("a schedule callback");
    unsafe {
        schedule_work(
            schedule.handle,
            message.len() as u32,
            message.as_ptr() as *const c_void,
        )
    }
}

unsafe extern "C" fn scheduling_run(_handle: LV2_Handle, frames: u32) {
    if frames == 0 {
        ZERO_RUN_STATUS.store(i64::from(schedule_kept(b"from run(0)")), Ordering::SeqCst);
    }
}

static WORKER_INTERFACE: LV2_Worker_Interface = LV2_Worker_Interface {
    work: Some(echoing_work),
    work_response: None,
    end_run: None,
};

unsafe extern "C" fn worker_extension(uri: *const std::ffi::c_char) -> *const c_void {
    if unsafe { CStr::from_ptr(uri) } == c"http://lv2plug.in/ns/ext/worker#interface" {
        &WORKER_INTERFACE as *const LV2_Worker_Interface as *const c_void
    } else {
        ptr::null()
    }
}

fn scheduling_descriptor(
    extension_data: Option<unsafe extern "C" fn(*const std::ffi::c_char) -> *const c_void>,
) -> LV2_Descriptor {
    LV2_Descriptor {
        URI: c"http://example.com/lv2/params".as_ptr(),
        instantiate: Some(schedule_keeping_instantiate),
        connect_port: Some(ignored_connect),
        activate: None,
        run: Some(scheduling_run),
        deactivate: None,
        cleanup: None,
        extension_data,
    }
}

fn load_scheduling(descriptor: &LV2_Descriptor) -> Lv2Loaded {
    let pref = PluginPref::default();
    let shared = Arc::new(PluginShared::new(0));
    Lv2Instance::load_descriptor(
        &params_bundle(),
        params_table(),
        descriptor,
        None,
        None,
        48_000.0,
        64,
        &Restore::from_pref(&pref),
        &shared,
    )
    .expect("load the fake plugin")
}

/// `worker.h`: the host must pass every request it accepts to `work`.  A
/// plugin without a worker interface has no `work`, so `schedule_work`
/// refuses instead of accepting a request nothing will serve.
#[test]
fn schedule_work_refuses_a_plugin_without_a_worker() {
    let _serial = worker_tests();
    let loaded = load_scheduling(&scheduling_descriptor(None));
    assert_eq!(
        ZERO_RUN_STATUS.load(Ordering::SeqCst),
        i64::from(LV2_WORKER_ERR_UNKNOWN),
        "run(0) had a request accepted"
    );
    assert_eq!(schedule_kept(b"later"), LV2_WORKER_ERR_UNKNOWN);
    assert!(!loaded.instance.core._rings.request_pending());
    drop(loaded);
    assert_eq!(registered_for_test(), 0);
}

/// A request scheduled before the worker is registered (here from the
/// load's zero-frame run) is accepted and reaches `work` once it is.
#[test]
fn a_request_scheduled_before_registration_reaches_work() {
    let _serial = worker_tests();
    let calls = WORK_CALLS.load(Ordering::SeqCst);
    let loaded = load_scheduling(&scheduling_descriptor(Some(worker_extension)));
    assert_eq!(
        ZERO_RUN_STATUS.load(Ordering::SeqCst),
        i64::from(LV2_WORKER_SUCCESS),
        "run(0) had its request refused"
    );
    let rings = Arc::clone(&loaded.instance.core._rings);
    let mut out = vec![0u8; RING_BYTES];
    let size = wait_for_response(&rings, &mut out);
    assert_eq!(&out[..size], b"from run(0)");
    assert_eq!(WORK_CALLS.load(Ordering::SeqCst) - calls, 1);
    assert_eq!(schedule_kept(b"later"), LV2_WORKER_SUCCESS);
    drop(loaded);
    assert_eq!(registered_for_test(), 0);
}

/// Responses `work_response` saw during the load's zero-frame run.
static RESTORE_RESPONSES: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());
/// How many responses were already delivered when `run(0)` started.
static RESPONSES_BEFORE_RUN0: AtomicU32 = AtomicU32::new(u32::MAX);
/// How many `work` calls `run(0)` itself made.  A request it schedules must
/// stay queued.
static WORK_DURING_RUN0: AtomicU32 = AtomicU32::new(u32::MAX);
static RESTORE_WORK_CALLS: AtomicU32 = AtomicU32::new(0);

static RESTORE_STATE_INTERFACE: LV2_State_Interface = LV2_State_Interface {
    save: None,
    restore: Some(restore_schedules_work),
};

static RESTORE_WORKER_INTERFACE: LV2_Worker_Interface = LV2_Worker_Interface {
    work: Some(restore_responding_work),
    work_response: Some(restore_work_response),
    end_run: None,
};

unsafe extern "C" fn restore_schedules_work(
    _instance: LV2_Handle,
    _retrieve: LV2_State_Retrieve_Function,
    _handle: lv2_sys::LV2_State_Handle,
    _flags: u32,
    _features: *const *const LV2_Feature,
) -> LV2_State_Status {
    let _ = schedule_kept(b"from-restore");
    LV2_STATE_SUCCESS
}

unsafe extern "C" fn restore_responding_work(
    _instance: LV2_Handle,
    respond: LV2_Worker_Respond_Function,
    handle: LV2_Worker_Respond_Handle,
    _size: u32,
    _data: *const c_void,
) -> LV2_Worker_Status {
    let call = RESTORE_WORK_CALLS.fetch_add(1, Ordering::SeqCst);
    if call == 0 {
        // `work` may schedule another request.  The load drain must run it
        // before it returns, still before `run(0)`.
        let _ = schedule_kept(b"follow-up");
    }
    let body: &[u8] = if call == 0 { b"loaded" } else { b"again" };
    match respond {
        Some(respond) => unsafe {
            respond(handle, body.len() as u32, body.as_ptr() as *const c_void)
        },
        None => LV2_WORKER_ERR_UNKNOWN,
    }
}

unsafe extern "C" fn restore_work_response(
    _handle: LV2_Handle,
    size: u32,
    body: *const c_void,
) -> LV2_Worker_Status {
    let bytes = if body.is_null() {
        Vec::new()
    } else {
        // SAFETY: the host promises `size` readable bytes for the call.
        unsafe { std::slice::from_raw_parts(body as *const u8, size as usize) }.to_vec()
    };
    RESTORE_RESPONSES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(bytes);
    LV2_WORKER_SUCCESS
}

unsafe extern "C" fn restore_ordering_run(_handle: LV2_Handle, frames: u32) {
    if frames != 0 {
        return;
    }
    RESPONSES_BEFORE_RUN0.store(
        RESTORE_RESPONSES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len() as u32,
        Ordering::SeqCst,
    );
    let before = RESTORE_WORK_CALLS.load(Ordering::SeqCst);
    let _ = schedule_kept(b"from-run");
    WORK_DURING_RUN0.store(
        RESTORE_WORK_CALLS.load(Ordering::SeqCst) - before,
        Ordering::SeqCst,
    );
}

unsafe extern "C" fn restore_worker_extension(uri: *const std::ffi::c_char) -> *const c_void {
    let uri = unsafe { CStr::from_ptr(uri) };
    if uri == c"http://lv2plug.in/ns/ext/worker#interface" {
        &RESTORE_WORKER_INTERFACE as *const LV2_Worker_Interface as *const c_void
    } else if uri == c"http://lv2plug.in/ns/ext/state#interface" {
        &RESTORE_STATE_INTERFACE as *const LV2_State_Interface as *const c_void
    } else {
        ptr::null()
    }
}

/// A header-only state blob: magic, version 1, no records.  Enough for the
/// host to call `restore`, which is what schedules the request.
fn empty_state_blob() -> Vec<u8> {
    let mut bytes = Vec::with_capacity(16);
    bytes.extend_from_slice(b"RWL2ST01");
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes
}

/// A restore that calls `schedule_work` is served before `run(0)`, and the
/// response is delivered before that run.  A request `run(0)` itself
/// schedules stays queued.
#[test]
fn a_restore_runs_worker_requests_before_the_zero_frame_run() {
    let _serial = worker_tests();
    RESTORE_WORK_CALLS.store(0, Ordering::SeqCst);
    RESPONSES_BEFORE_RUN0.store(u32::MAX, Ordering::SeqCst);
    WORK_DURING_RUN0.store(u32::MAX, Ordering::SeqCst);
    RESTORE_RESPONSES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();

    let descriptor = LV2_Descriptor {
        URI: c"http://example.com/lv2/params".as_ptr(),
        instantiate: Some(schedule_keeping_instantiate),
        connect_port: Some(ignored_connect),
        activate: None,
        run: Some(restore_ordering_run),
        deactivate: None,
        cleanup: None,
        extension_data: Some(restore_worker_extension),
    };
    let pref = PluginPref {
        state: Some(PluginState::capture(&empty_state_blob(), &[])),
        ..PluginPref::default()
    };
    let shared = Arc::new(PluginShared::new(0));
    let loaded = Lv2Instance::load_descriptor(
        &params_bundle(),
        params_table(),
        &descriptor,
        None,
        None,
        48_000.0,
        64,
        &Restore::from_pref(&pref),
        &shared,
    )
    .expect("load the fake plugin");

    assert_eq!(
        RESPONSES_BEFORE_RUN0.load(Ordering::SeqCst),
        2,
        "work_response did not run before run(0)"
    );
    assert_eq!(
        WORK_DURING_RUN0.load(Ordering::SeqCst),
        0,
        "run(0) ran a request it had just scheduled"
    );
    let responses = RESTORE_RESPONSES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(responses, [b"loaded".to_vec(), b"again".to_vec()]);
    drop(loaded);
    assert_eq!(registered_for_test(), 0);
}

/// The buffer slot of the range for `symbol`.
fn table_slot(processor: &Lv2Processor, symbol: &str) -> usize {
    let index = processor
        .ids
        .index_of(symbol_id(symbol))
        .expect("a range for the symbol");
    processor.ranges[index].slot
}

// A fake library descriptor with one plugin, which counts its cleanups.
static LIB_CLEANUPS: AtomicU32 = AtomicU32::new(0);
static LIB_HANDLE: u8 = 0;

struct SyncDescriptor(LV2_Descriptor);
// SAFETY: the fake descriptor is immutable and its strings are static.
unsafe impl Sync for SyncDescriptor {}

static LIB_PLUGIN: SyncDescriptor = SyncDescriptor(LV2_Descriptor {
    URI: c"http://example.com/lv2/from-lib".as_ptr(),
    instantiate: None,
    connect_port: None,
    activate: None,
    run: None,
    deactivate: None,
    cleanup: None,
    extension_data: None,
});

unsafe extern "C" fn lib_get_plugin(
    handle: lv2_sys::LV2_Lib_Handle,
    index: u32,
) -> *const LV2_Descriptor {
    assert_eq!(handle as *const u8, &LIB_HANDLE as *const u8);
    if index == 0 {
        &LIB_PLUGIN.0
    } else {
        ptr::null()
    }
}

unsafe extern "C" fn lib_cleanup(handle: lv2_sys::LV2_Lib_Handle) {
    assert_eq!(handle as *const u8, &LIB_HANDLE as *const u8);
    LIB_CLEANUPS.fetch_add(1, Ordering::SeqCst);
}

struct SyncLib(lv2_sys::LV2_Lib_Descriptor);
// SAFETY: as above; the handle is the address of a static.
unsafe impl Sync for SyncLib {}

static LIB: SyncLib = SyncLib(lv2_sys::LV2_Lib_Descriptor {
    handle: &LIB_HANDLE as *const u8 as lv2_sys::LV2_Lib_Handle,
    size: std::mem::size_of::<lv2_sys::LV2_Lib_Descriptor>() as u32,
    cleanup: Some(lib_cleanup),
    get_plugin: Some(lib_get_plugin),
});

static LIB_BUNDLE: Mutex<Option<String>> = Mutex::new(None);

unsafe extern "C" fn lib_descriptor(
    bundle_path: *const std::ffi::c_char,
    features: *const *const LV2_Feature,
) -> *const lv2_sys::LV2_Lib_Descriptor {
    assert!(
        unsafe { (*features).is_null() },
        "the feature array is not empty"
    );
    *LIB_BUNDLE.lock().expect("the bundle record") = Some(
        unsafe { CStr::from_ptr(bundle_path) }
            .to_string_lossy()
            .into_owned(),
    );
    &LIB.0
}

unsafe extern "C" fn failing_lib_descriptor(
    _bundle_path: *const std::ffi::c_char,
    _features: *const *const LV2_Feature,
) -> *const lv2_sys::LV2_Lib_Descriptor {
    ptr::null()
}

unsafe extern "C" fn plain_descriptor(_index: u32) -> *const LV2_Descriptor {
    ptr::null()
}

/// `lv2.h`: a library may export `lv2_lib_descriptor` instead of
/// `lv2_descriptor`.  It is preferred, its plugins are found through
/// `get_plugin`, and its `cleanup` runs once, when the module goes.
#[test]
fn a_library_descriptor_is_preferred_and_cleaned_up() {
    let features: [*const LV2_Feature; 1] = [ptr::null()];
    let descriptors = resolve(
        Some(lib_descriptor),
        Some(plain_descriptor),
        c"/bundles/lib.lv2/",
        features.as_ptr(),
    )
    .expect("resolve the library descriptor");
    assert!(matches!(descriptors, Descriptors::Library(_)));
    assert_eq!(
        LIB_BUNDLE
            .lock()
            .expect("the bundle record")
            .take()
            .as_deref(),
        Some("/bundles/lib.lv2/")
    );

    let cleanups = LIB_CLEANUPS.load(Ordering::SeqCst);
    let module = Lv2Module::for_test(descriptors);
    let found = module
        .descriptor_for_uri("http://example.com/lv2/from-lib")
        .expect("the library's plugin");
    assert_eq!(found, &LIB_PLUGIN.0 as *const LV2_Descriptor);
    assert!(module
        .descriptor_for_uri("http://example.com/lv2/other")
        .is_none());
    assert_eq!(LIB_CLEANUPS.load(Ordering::SeqCst), cleanups);
    drop(module);
    assert_eq!(LIB_CLEANUPS.load(Ordering::SeqCst) - cleanups, 1);
}

#[test]
fn a_failing_library_descriptor_is_an_error_and_lv2_descriptor_is_the_fallback() {
    let features: [*const LV2_Feature; 1] = [ptr::null()];
    assert!(resolve(
        Some(failing_lib_descriptor),
        Some(plain_descriptor),
        c"/b/",
        features.as_ptr()
    )
    .is_err());
    assert!(matches!(
        resolve(None, Some(plain_descriptor), c"/b/", features.as_ptr()),
        Ok(Descriptors::Function(_))
    ));
    assert!(resolve(None, None, c"/b/", features.as_ptr()).is_err());
}

/// What the default-state fake's `restore` retrieved: key URI to type URI
/// and value bytes.
static DEFAULT_RETRIEVED: Mutex<Vec<(String, String, Vec<u8>)>> = Mutex::new(Vec::new());

const DEFAULT_KEYS: [&CStr; 7] = [
    c"http://example.com/lv2/default-state#sample",
    c"http://example.com/lv2/default-state#gain",
    c"http://example.com/lv2/default-state#count",
    c"http://example.com/lv2/default-state#name",
    c"http://example.com/lv2/default-state#mode",
    c"http://example.com/lv2/default-state#on",
    c"http://example.com/lv2/default-state#nested",
];

unsafe extern "C" fn default_recording_restore(
    _instance: LV2_Handle,
    retrieve: LV2_State_Retrieve_Function,
    handle: *mut c_void,
    _flags: u32,
    _features: *const *const LV2_Feature,
) -> LV2_State_Status {
    let retrieve = retrieve.expect("a retrieve callback");
    let map = urid::global();
    let mut found = Vec::new();
    for key in DEFAULT_KEYS {
        let (mut size, mut type_, mut flags) = (0usize, 0u32, 0u32);
        let value = unsafe { retrieve(handle, map.map(key), &mut size, &mut type_, &mut flags) };
        if value.is_null() {
            continue;
        }
        let bytes = unsafe { std::slice::from_raw_parts(value as *const u8, size) }.to_vec();
        let type_uri = map
            .uri(type_)
            .map(|uri| uri.to_string_lossy().into_owned())
            .unwrap_or_default();
        found.push((key.to_string_lossy().into_owned(), type_uri, bytes));
    }
    *DEFAULT_RETRIEVED.lock().expect("the record") = found;
    LV2_STATE_SUCCESS
}

/// `state:loadDefaultState`: the bundle's `state:state` restores as the atoms
/// lilv makes of it.  A value with no atom form (a nested blank node) is
/// left out, and a plugin that does not ask for its default gets none.
#[test]
fn a_default_state_restores_as_atoms() {
    let bundle = testdata("default-state.lv2");
    let (store, _) = read_bundle(&bundle).expect("read the bundle");
    assert!(state::default_state(&store, "http://example.com/lv2/no-default").is_none());
    let default = state::default_state(&store, "http://example.com/lv2/default-state")
        .expect("a default state");
    let interface = LV2_State_Interface {
        save: None,
        restore: Some(default_recording_restore),
    };
    let host = state_host();
    assert!(state::restore_default(
        ptr::null_mut(),
        &interface,
        &default,
        host.as_ptr()
    ));
    let found = DEFAULT_RETRIEVED.lock().expect("the record").clone();
    let value = |name: &str| {
        found
            .iter()
            .find(|(key, _, _)| key.ends_with(name))
            .map(|(_, type_, bytes)| {
                (
                    type_.rsplit('#').next().unwrap_or("").to_owned(),
                    bytes.clone(),
                )
            })
    };
    let mut path = super::ttl::bundle_root(&bundle)
        .join("click.wav")
        .to_string_lossy()
        .into_owned()
        .into_bytes();
    path.push(0);
    assert_eq!(value("#sample"), Some(("Path".to_owned(), path)));
    assert_eq!(
        value("#gain"),
        Some(("Float".to_owned(), 0.5f32.to_ne_bytes().to_vec()))
    );
    assert_eq!(
        value("#count"),
        Some(("Int".to_owned(), 3i32.to_ne_bytes().to_vec()))
    );
    assert_eq!(
        value("#name"),
        Some(("String".to_owned(), b"hello\0".to_vec()))
    );
    let fast = urid::global().map(c"http://example.com/lv2/default-state#fast");
    assert_eq!(
        value("#mode"),
        Some(("URID".to_owned(), fast.to_ne_bytes().to_vec()))
    );
    assert_eq!(
        value("#on"),
        Some(("Bool".to_owned(), 1i32.to_ne_bytes().to_vec()))
    );
    assert_eq!(value("#nested"), None, "a blank node value was restored");
}

/// `state:loadDefaultState` as a required feature with no `state:state` is a
/// load error.  The same feature listed as optional, with nothing to
/// restore, is omitted and the load continues.
#[test]
fn a_required_load_default_without_state_refuses_the_load() {
    let bundle = testdata("default-state.lv2");
    let shared = Arc::new(PluginShared::new(0));
    let pref = PluginPref::default();
    let error = match Lv2Instance::load(
        &bundle,
        "http://example.com/lv2/required-default",
        48_000,
        64,
        &Restore::from_pref(&pref),
        &shared,
    ) {
        Ok(_) => panic!("a required feature with nothing to restore was instantiated"),
        Err(error) => error,
    };
    assert!(
        error.contains("state:loadDefaultState") && error.contains("state:state"),
        "{error}"
    );

    let optional = match Lv2Instance::load(
        &bundle,
        "http://example.com/lv2/optional-default",
        48_000,
        64,
        &Restore::from_pref(&pref),
        &shared,
    ) {
        Ok(_) => panic!("the optional plugin has no binary"),
        Err(error) => error,
    };
    assert!(
        !optional.contains("loadDefaultState"),
        "an optional feature with no state failed the load: {optional}"
    );
    assert!(optional.contains("no binary"), "{optional}");
}

#[test]
fn a_mapped_path_is_c_heap_that_free_or_free_path_releases() {
    let host = HostFeatures::new(48000.0, 512, 8192, None, false);
    let (map_path, free_path) = host.paths();
    let input = CString::new("/tmp/RigWerk state/sample.wav").expect("a path");
    unsafe {
        let abstract_path = map_path.abstract_path.expect("abstract_path");
        let absolute_path = map_path.absolute_path.expect("absolute_path");
        // A plugin written to `state.h` before 1.18 frees with `free()`.
        let by_free = abstract_path(map_path.handle, input.as_ptr());
        assert!(!by_free.is_null());
        assert_eq!(CStr::from_ptr(by_free), input.as_c_str());
        libc::free(by_free.cast());
        let by_free = absolute_path(map_path.handle, input.as_ptr());
        assert!(!by_free.is_null());
        libc::free(by_free.cast());
        // One written to 1.18 frees with `state:freePath`.
        let by_feature = absolute_path(map_path.handle, input.as_ptr());
        assert!(!by_feature.is_null());
        assert_eq!(CStr::from_ptr(by_feature), input.as_c_str());
        (free_path.free_path.expect("free_path"))(free_path.handle, by_feature);
    }
}

/// What a teardown fake's instance handle points at: the schedule feature
/// its `work_response` answers with, as `eg-sampler` schedules the old
/// sample's free from the response that installs the new one.
struct TeardownPlugin {
    schedule: LV2_Worker_Schedule,
}

/// Every call the teardown fakes saw, in order.
static TEARDOWN_CALLS: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn teardown_log(entry: String) {
    TEARDOWN_CALLS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(entry);
}

unsafe fn teardown_bytes(size: u32, data: *const c_void) -> String {
    if data.is_null() {
        return String::new();
    }
    let bytes = unsafe { std::slice::from_raw_parts(data as *const u8, size as usize) };
    String::from_utf8_lossy(bytes).into_owned()
}

/// `work` that answers "load" with the new sample and frees nothing back.
unsafe extern "C" fn teardown_work(
    _instance: LV2_Handle,
    respond: LV2_Worker_Respond_Function,
    handle: LV2_Worker_Respond_Handle,
    size: u32,
    data: *const c_void,
) -> LV2_Worker_Status {
    let message = unsafe { teardown_bytes(size, data) };
    teardown_log(format!("work {message}"));
    if message == "load" {
        let sample = b"new sample";
        return match respond {
            Some(respond) => unsafe {
                respond(
                    handle,
                    sample.len() as u32,
                    sample.as_ptr() as *const c_void,
                )
            },
            None => LV2_WORKER_ERR_UNKNOWN,
        };
    }
    LV2_WORKER_SUCCESS
}

/// `work_response` that installs the sample and schedules the old one's free.
unsafe extern "C" fn teardown_work_response(
    instance: LV2_Handle,
    size: u32,
    body: *const c_void,
) -> LV2_Worker_Status {
    let message = unsafe { teardown_bytes(size, body) };
    teardown_log(format!("response {message}"));
    let plugin = unsafe { &*(instance as *const TeardownPlugin) };
    let free = b"free old sample";
    let schedule = plugin.schedule.schedule_work.expect("schedule_work");
    unsafe {
        schedule(
            plugin.schedule.handle,
            free.len() as u32,
            free.as_ptr() as *const c_void,
        )
    }
}

unsafe extern "C" fn teardown_end_run(_instance: LV2_Handle) -> LV2_Worker_Status {
    teardown_log("end_run".to_owned());
    LV2_WORKER_SUCCESS
}

/// `worker.h`: a response `respond` accepted must reach `work_response`.
/// The gate keeps the worker thread out, so the load request, its response
/// and the free that response schedules are all finished by `unregister`.
#[test]
fn unregister_delivers_the_responses_and_serves_what_they_schedule() {
    let _serial = worker_tests();
    TEARDOWN_CALLS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
    let rings = Arc::new(WorkerRings::new());
    rings.accept_requests();
    let plugin = Box::new(TeardownPlugin {
        schedule: schedule_struct(&rings),
    });
    let interface = LV2_Worker_Interface {
        work: Some(teardown_work),
        work_response: Some(teardown_work_response),
        end_run: Some(teardown_end_run),
    };
    let handle = &*plugin as *const TeardownPlugin as LV2_Handle;
    let instance = register(handle, &interface, Arc::clone(&rings)).expect("register");

    let gate = hold_gate(&instance);
    assert!(rings.push_request(b"load"));
    let removing = {
        let instance = Arc::clone(&instance);
        std::thread::spawn(move || unregister(&instance))
    };
    while registered_for_test() != 0 {
        std::thread::sleep(Duration::from_millis(1));
    }
    drop(gate);
    removing.join().expect("unregister");

    let calls = TEARDOWN_CALLS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(
        calls,
        [
            "work load",
            "response new sample",
            "end_run",
            "work free old sample"
        ],
        "the teardown did not finish the exchange in order"
    );
    assert!(!rings.request_pending());
    assert!(!rings.response_pending());
}

/// How many `work` calls the endless fake has seen.
static ENDLESS_WORK: AtomicU32 = AtomicU32::new(0);

/// `work` that always answers.
unsafe extern "C" fn endless_work(
    _instance: LV2_Handle,
    respond: LV2_Worker_Respond_Function,
    handle: LV2_Worker_Respond_Handle,
    _size: u32,
    _data: *const c_void,
) -> LV2_Worker_Status {
    ENDLESS_WORK.fetch_add(1, Ordering::SeqCst);
    let again = b"again";
    match respond {
        Some(respond) => unsafe { respond(handle, again.len() as u32, again.as_ptr().cast()) },
        None => LV2_WORKER_ERR_UNKNOWN,
    }
}

/// `work_response` that always schedules another request.
unsafe extern "C" fn endless_work_response(
    instance: LV2_Handle,
    _size: u32,
    _body: *const c_void,
) -> LV2_Worker_Status {
    let plugin = unsafe { &*(instance as *const TeardownPlugin) };
    let schedule = plugin.schedule.schedule_work.expect("schedule_work");
    let again = b"again";
    unsafe {
        schedule(
            plugin.schedule.handle,
            again.len() as u32,
            again.as_ptr().cast(),
        )
    }
}

/// A plugin that answers every response with a new request cannot keep the
/// teardown going forever.
#[test]
fn unregister_stops_a_worker_exchange_that_never_ends() {
    let _serial = worker_tests();
    let rings = Arc::new(WorkerRings::new());
    rings.accept_requests();
    let plugin = Box::new(TeardownPlugin {
        schedule: schedule_struct(&rings),
    });
    let interface = LV2_Worker_Interface {
        work: Some(endless_work),
        work_response: Some(endless_work_response),
        end_run: None,
    };
    let handle = &*plugin as *const TeardownPlugin as LV2_Handle;
    let instance = register(handle, &interface, Arc::clone(&rings)).expect("register");
    let gate = hold_gate(&instance);
    let before = ENDLESS_WORK.load(Ordering::SeqCst);
    assert!(rings.push_request(b"again"));
    let removing = {
        let instance = Arc::clone(&instance);
        std::thread::spawn(move || unregister(&instance))
    };
    while registered_for_test() != 0 {
        std::thread::sleep(Duration::from_millis(1));
    }
    drop(gate);
    removing.join().expect("unregister");
    assert_eq!(
        ENDLESS_WORK.load(Ordering::SeqCst) - before,
        TEARDOWN_ROUNDS as u32 + 1,
        "the teardown did not stop after its rounds"
    );
}
