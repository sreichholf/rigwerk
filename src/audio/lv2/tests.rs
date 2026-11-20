use std::ffi::{c_void, CStr, CString};
use std::path::PathBuf;
use std::ptr;
use std::sync::Arc;

use lv2_sys::{
    LV2_Options_Context_LV2_OPTIONS_INSTANCE as LV2_OPTIONS_INSTANCE, LV2_Worker_Schedule,
    LV2_Worker_Status_LV2_WORKER_ERR_NO_SPACE as LV2_WORKER_ERR_NO_SPACE,
    LV2_Worker_Status_LV2_WORKER_ERR_UNKNOWN as LV2_WORKER_ERR_UNKNOWN,
    LV2_Worker_Status_LV2_WORKER_SUCCESS as LV2_WORKER_SUCCESS,
};

use crate::audio::plugins::AudioThreadScope;

use super::host::{used_uris, HostFeatures, SATISFIED, SUPPORTED};
use super::scan::scan_bundle;
use super::ttl::{file_uri, path_from_file_uri, TtlStore};
use super::urid;
use super::worker::{schedule_struct, WorkerRings, RING_BYTES};

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

/// `lv2:inPlaceBroken`, `lv2:isLive` and `lv2:hardRTCapable` are met by
/// how the host runs a plugin, so requiring them lists the plugin cleanly.
#[test]
fn required_features_the_host_meets_without_data_are_accepted() {
    let found = scan_bundle(&testdata("satisfied.lv2")).expect("scan");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].scan_error, None);
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
    let host = HostFeatures::new(48000.0, 512, 8192, Some(Arc::clone(&rings)));
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

    let without_rings = HostFeatures::new(48000.0, 512, 8192, None);
    assert!(
        !feature_uris(&without_rings)
            .iter()
            .any(|uri| uri.ends_with("worker#schedule")),
        "worker:schedule appeared without rings"
    );
}

#[test]
fn host_features_stay_put_when_the_handle_moves() {
    let host = HostFeatures::new(48000.0, 512, 8192, None);
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
    let host = HostFeatures::new(48000.0, 512, 8192, None);
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
    assert_eq!(unsafe { *(min.value as *const i32) }, 1);

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
    let host = HostFeatures::new(48000.0, 512, 8192, None);
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
