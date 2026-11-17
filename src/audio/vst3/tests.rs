// Real VST3 plugins: Steinberg's SDK samples (see `plugins::fixtures`).
// With `RIGWERK_VST3_FIXTURES` unset they return at once.

use std::sync::Arc;

use ::vst3::Steinberg::Vst::{IComponent, IComponentTrait, IEditController, IEditControllerTrait};
use ::vst3::Steinberg::{kResultOk, IBStream, IPluginBaseTrait, TUID};
use ::vst3::{ComPtr, ComWrapper};

use super::connection::Connection;
use super::factory::{self, AUDIO_MODULE_CLASS};
use super::host::host_context;
use super::module::{self, Vst3Module};
use super::scan;
use super::stream::{decode_state, encode_state, MemoryStream, StateBlob};

/// The class ids saved preferences hold for the fixtures, as in
/// `plugins/tests.rs`.
const FIXTURES: [(&str, &str); 3] = [
    ("again", "84E8DE5F92554F5396FAE4133C935A18"),
    ("adelay", "0CDBB66985D548A9BFD8371909D24BB3"),
    ("host-checker", "23FC190E02DD4499A8D2230E50617DA3"),
];

/// Fixture `name`'s module, or `None` when the fixtures are not set up.
fn fixture_module(name: &str) -> Option<Arc<Vst3Module>> {
    let bundle = crate::audio::vst3_fixture(name)?;
    Some(module::acquire(&bundle).unwrap_or_else(|error| panic!("{name}: {error}")))
}

/// A component of class `cid`, initialized with the host context.
fn initialized(module: &Vst3Module, cid: &TUID) -> ComPtr<IComponent> {
    let component = factory::create::<IComponent>(module.factory(), cid).expect("a component");
    // SAFETY: a new component and the static host context.
    assert_eq!(unsafe { component.initialize(host_context()) }, kResultOk);
    component
}

#[test]
fn vst3_fixture_each_bundle_loads_and_lists_its_audio_class() {
    let _main = crate::audio::claim_main_thread();
    for (name, class_id) in FIXTURES {
        let Some(module) = fixture_module(name) else {
            return;
        };
        let cid = factory::parse_class_id(class_id).expect("a class id");
        let classes = factory::classes(module.factory());
        let class = classes
            .iter()
            .find(|class| class.cid == cid)
            .unwrap_or_else(|| panic!("{name} lists no class {class_id}: {classes:?}"));
        assert_eq!(class.category, AUDIO_MODULE_CLASS, "{name}");
        assert!(!class.name.is_empty(), "{name}");
        assert_eq!(
            factory::audio_class(module.factory(), Some(class_id)),
            Ok(cid)
        );
        assert_eq!(factory::audio_class(module.factory(), None), Ok(cid));
        assert!(
            factory::audio_class(module.factory(), Some("00000000000000000000000000000000"))
                .is_err()
        );
        assert!(factory::audio_class(module.factory(), Some("not a class id")).is_err());
    }
}

/// The scan finds each bundle without loading it, and the audio classes
/// `moduleinfo.json` declares are the ones the factory reports, so a
/// scanned class id opens the class it names.
#[test]
fn vst3_fixture_the_scan_lists_what_the_factory_offers() {
    let _main = crate::audio::claim_main_thread();
    let Some(again) = crate::audio::vst3_fixture("again") else {
        return;
    };
    let folder = again.parent().expect("the fixtures folder").to_path_buf();
    let found = scan::find_bundles(std::slice::from_ref(&folder));
    for (name, class_id) in FIXTURES {
        let bundle = folder.join(format!("{name}.vst3"));
        assert!(found.contains(&bundle), "{name} not in {found:?}");
        assert!(scan::has_loadable_binary(&bundle), "{name}");
        let info = scan::read_module_info(&bundle)
            .unwrap_or_else(|error| panic!("{name}: {error}"))
            .unwrap_or_else(|| panic!("{name} has no moduleinfo.json"));
        assert_eq!(info.factory.vendor, "Steinberg Media Technologies");
        let mut declared: Vec<_> = info
            .classes
            .iter()
            .filter(|class| class.category == AUDIO_MODULE_CLASS)
            .map(|class| (class.class_id.clone(), class.name.clone()))
            .collect();
        assert!(
            declared.iter().any(|(id, _)| id == class_id),
            "{name}: {declared:?}"
        );

        let module = fixture_module(name).expect("the same fixture");
        let mut offered: Vec<_> = factory::classes(module.factory())
            .into_iter()
            .filter(|class| class.category == AUDIO_MODULE_CLASS)
            .map(|class| {
                let id = declared
                    .iter()
                    .find(|(id, _)| factory::parse_class_id(id) == Some(class.cid))
                    .map(|(id, _)| id.clone())
                    .unwrap_or_default();
                (id, class.name)
            })
            .collect();
        declared.sort();
        offered.sort();
        assert_eq!(declared, offered, "{name}");
    }
}

/// The header check against real binaries: every fixture's binary passes,
/// and copied out on its own it is a single-file plugin on Windows only.
#[test]
fn vst3_fixture_binaries_pass_the_header_check() {
    let Some(again) = crate::audio::vst3_fixture("again") else {
        return;
    };
    let folder = again.parent().expect("the fixtures folder");
    for (name, _) in FIXTURES {
        let bundle = folder.join(format!("{name}.vst3"));
        module::binary_path(&bundle).unwrap_or_else(|error| panic!("{name}: {error}"));
    }
    let binary = module::binary_path(&again).expect("AGain's binary");
    let dir = std::env::temp_dir().join(format!("rigwerk-vst3-single-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let single = dir.join("AGain Single.vst3");
    std::fs::copy(&binary, &single).expect("copy");
    let listed = scan::has_loadable_binary(&single);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(listed, cfg!(target_os = "windows"));
}

#[test]
fn vst3_fixture_again_component_initializes_with_the_host_context() {
    let _main = crate::audio::claim_main_thread();
    let Some(module) = fixture_module("again") else {
        return;
    };
    let key = module.key().to_path_buf();
    assert!(module::is_loaded(&key));
    let cid = factory::audio_class(module.factory(), Some(FIXTURES[0].1)).expect("AGain");
    let component = initialized(&module, &cid);

    // AGain's controller is a separate class, connected through the proxies.
    let mut controller_cid: TUID = [0; 16];
    // SAFETY: a writable TUID.
    assert_eq!(
        unsafe { component.getControllerClassId(&mut controller_cid) },
        kResultOk
    );
    let controller = factory::create::<IEditController>(module.factory(), &controller_cid)
        .expect("a controller");
    // SAFETY: a new controller and the static host context.
    assert_eq!(unsafe { controller.initialize(host_context()) }, kResultOk);
    let connection = Connection::new(&component, &controller)
        .expect("connected")
        .expect("AGain has connection points");

    // The component state through a host stream, into the envelope and
    // back into the controller.
    let written = ComWrapper::new(MemoryStream::new(Vec::new()));
    let stream = written.to_com_ptr::<IBStream>().expect("a stream");
    // SAFETY: a live stream for the call.
    assert_eq!(unsafe { component.getState(stream.as_ptr()) }, kResultOk);
    let blob = StateBlob {
        component: written.bytes(),
        controller: None,
    };
    assert!(!blob.component.is_empty(), "AGain writes a component state");
    let decoded = decode_state(&encode_state(&blob).expect("encode")).expect("decode");
    // One stream per read: each starts at the beginning.
    let read = ComWrapper::new(MemoryStream::new(decoded.component.clone()));
    let stream = read.to_com_ptr::<IBStream>().expect("a stream");
    let again = ComWrapper::new(MemoryStream::new(decoded.component));
    let again = again.to_com_ptr::<IBStream>().expect("a stream");
    // SAFETY: live streams for the calls.
    unsafe {
        assert_eq!(controller.setComponentState(stream.as_ptr()), kResultOk);
        assert_eq!(component.setState(again.as_ptr()), kResultOk);
    }

    // AGain's component sends its controller a text message from
    // `setActive`, through the component side's proxy.
    let before = connection.forwarded();
    // SAFETY: an initialized component, on the thread that connected it.
    unsafe {
        assert_eq!(component.setActive(1), kResultOk);
        assert_eq!(component.setActive(0), kResultOk);
    }
    assert_eq!(connection.forwarded(), before + 2, "both messages arrived");
    // What the UI does every frame; AGain registers nothing without an
    // editor.
    super::service_run_loop();

    drop(connection);
    // SAFETY: both were initialized above and are not used afterwards.
    unsafe {
        assert_eq!(controller.terminate(), kResultOk);
        assert_eq!(component.terminate(), kResultOk);
    }
    drop((controller, component, stream, again, read, written));
    drop(module);
    assert!(!module::is_loaded(&key), "the last user unloads the module");
    // Not `assert_unloaded`: `again.so` exports a `STB_GNU_UNIQUE` symbol
    // (libstdc++'s `_Sp_make_shared_tag::_S_ti()::__tag`), and glibc never
    // unmaps such a library.  ADelay's binary has none; its test checks.
}

/// The binary at `key` is no longer mapped into the process.
#[cfg(target_os = "linux")]
fn assert_unloaded(key: &std::path::Path) {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(key.as_os_str().as_bytes()).expect("a path");
    // SAFETY: `RTLD_NOLOAD` only looks the library up; a handle it returns
    // is closed again at once.
    let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_NOLOAD) };
    if !handle.is_null() {
        // SAFETY: the handle `dlopen` just returned.
        unsafe { libc::dlclose(handle) };
    }
    assert!(handle.is_null(), "{} is still loaded", key.display());
}

#[cfg(not(target_os = "linux"))]
fn assert_unloaded(_key: &std::path::Path) {}

#[test]
fn vst3_fixture_a_module_loads_once_for_two_instances() {
    let _main = crate::audio::claim_main_thread();
    let Some(first) = fixture_module("adelay") else {
        return;
    };
    let second = fixture_module("adelay").expect("the same fixture");
    assert!(Arc::ptr_eq(&first, &second), "one module per binary");
    let key = first.key().to_path_buf();
    let cid = factory::audio_class(first.factory(), Some(FIXTURES[1].1)).expect("ADelay");
    let one = initialized(&first, &cid);
    let two = initialized(&second, &cid);
    // SAFETY: both were initialized above and are not used afterwards.
    unsafe {
        assert_eq!(one.terminate(), kResultOk);
        assert_eq!(two.terminate(), kResultOk);
    }
    drop((one, two));
    drop(first);
    assert!(module::is_loaded(&key), "still in use");
    drop(second);
    assert!(!module::is_loaded(&key));
    assert_unloaded(&key);
}
