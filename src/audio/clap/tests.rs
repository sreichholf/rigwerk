use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::Ordering;

use clap_sys::ext::audio_ports::{
    CLAP_AUDIO_PORTS_RESCAN_CHANNEL_COUNT, CLAP_AUDIO_PORTS_RESCAN_IN_PLACE_PAIR,
    CLAP_AUDIO_PORTS_RESCAN_LIST, CLAP_AUDIO_PORTS_RESCAN_NAMES, CLAP_AUDIO_PORTS_RESCAN_PORT_TYPE,
    CLAP_AUDIO_PORT_IS_MAIN,
};
use clap_sys::host::clap_host;

use super::host::{host_audio_ports_flag_supported, host_audio_ports_rescan, HostState, StateSave};
use super::module::{module_key, EntryInit, ModuleRegistry};
use super::plugin::ClapPlugin;
use super::ports::{
    port_layout, PortBuffers, PortInfo, PortLayout, MAX_AUDIO_PORTS, MAX_PORT_CHANNELS,
};
use super::scan::{scan_bundle, scan_module};
use super::state::{StateReader, StateWriter};

#[test]
fn state_save_waits_while_values_are_queued() {
    let mut host = HostState::new();
    // On the main thread with no plugin, a save would report
    // `Unavailable`; only the queued values make it wait.
    host.main_thread = std::thread::current().id();
    assert_eq!(host.save_state(), StateSave::Unavailable);
    host.params_pending.store(true, Ordering::Release);
    assert_eq!(host.save_state(), StateSave::Deferred);
}

#[test]
fn state_writer_collects_every_chunk() {
    let mut writer = StateWriter::default();
    let stream = writer.stream();
    let write = stream.write.expect("write callback");
    let first = [1u8, 2, 3];
    let second = [4u8; 5];
    unsafe {
        assert_eq!(write(&stream, first.as_ptr() as *const c_void, 3), 3);
        assert_eq!(write(&stream, second.as_ptr() as *const c_void, 5), 5);
        assert_eq!(write(&stream, std::ptr::null(), 0), 0);
        assert_eq!(write(&stream, std::ptr::null(), 4), -1);
        assert_eq!(
            write(std::ptr::null(), first.as_ptr() as *const c_void, 3),
            -1
        );
    }
    assert!(!writer.overflowed);
    assert_eq!(writer.bytes, vec![1, 2, 3, 4, 4, 4, 4, 4]);
}

#[test]
fn state_writer_refuses_runaway_sizes() {
    let mut writer = StateWriter::default();
    let stream = writer.stream();
    let byte = [0u8];
    let result = unsafe {
        (stream.write.expect("write"))(&stream, byte.as_ptr() as *const c_void, u64::MAX)
    };
    assert_eq!(result, -1);
    assert!(writer.overflowed);
    assert!(writer.bytes.is_empty());
}

#[test]
fn state_reader_hands_out_the_bytes_in_pieces() {
    let data: Vec<u8> = (0..10).collect();
    let mut reader = StateReader::new(&data);
    let stream = reader.stream();
    let read = stream.read.expect("read callback");
    let mut out = Vec::new();
    let mut chunk = [0u8; 4];
    loop {
        let count = unsafe { read(&stream, chunk.as_mut_ptr() as *mut c_void, 4) };
        assert!(count >= 0);
        if count == 0 {
            break;
        }
        out.extend_from_slice(&chunk[..count as usize]);
    }
    assert_eq!(out, data);
    // At the end it keeps reporting end of stream.
    assert_eq!(
        unsafe { read(&stream, chunk.as_mut_ptr() as *mut c_void, 4) },
        0
    );
    assert_eq!(
        unsafe { read(std::ptr::null(), chunk.as_mut_ptr() as *mut c_void, 4) },
        -1
    );
}

#[test]
fn state_round_trips_through_both_streams() {
    let mut writer = StateWriter::default();
    let ostream = writer.stream();
    let payload = b"plugin state";
    let written =
        unsafe { (ostream.write.expect("write"))(&ostream, payload.as_ptr() as *const c_void, 12) };
    assert_eq!(written, 12);
    let saved = std::mem::take(&mut writer.bytes);
    let mut reader = StateReader::new(&saved);
    let istream = reader.stream();
    let mut back = [0u8; 64];
    let count =
        unsafe { (istream.read.expect("read"))(&istream, back.as_mut_ptr() as *mut c_void, 64) };
    assert_eq!(&back[..count as usize], payload);
}

/// A loaded fake CLAP effect with one stereo main port, for tests outside
/// this module.  With `fail_process`, every block it is given fails.  The
/// fake's state is per thread: process it on the thread that loaded it.
pub(crate) fn fake_plugin_for_test(fail_process: bool) -> ClapPlugin {
    const MAIN: u32 = clap_sys::ext::audio_ports::CLAP_AUDIO_PORT_IS_MAIN;
    fake::reset(if fail_process {
        fake::Fail::Process
    } else {
        fake::Fail::Nothing
    });
    fake::declare_ports(&[(2, MAIN)], &[(2, MAIN)]);
    fake::load("fake").expect("load the fake plugin")
}

/// Makes `plugin` report a channel-count change, as a real plugin does
/// through `clap_host_audio_ports.rescan`.
pub(crate) fn rescan_ports_for_test(plugin: &ClapPlugin) {
    let host: *const clap_sys::host::clap_host = &*plugin._host;
    unsafe {
        host_audio_ports_rescan(
            host,
            clap_sys::ext::audio_ports::CLAP_AUDIO_PORTS_RESCAN_CHANNEL_COUNT,
        )
    };
}

/// A fake CLAP module driven from Rust: every entry and plugin callback
/// logs itself, and `Fail` picks the step that goes wrong.  The callbacks
/// all run on the test thread, so the log and the setting are
/// thread-local and the tests can run in parallel.
mod fake {
    use super::super::module::{ModuleHandle, ModuleRegistry, OpenedModule};
    use super::super::plugin::ClapPlugin;
    use super::super::ports::{DeclaredPorts, PortInfo};
    use crate::audio::plugins::Restore;
    use clap_sys::entry::clap_plugin_entry;
    use clap_sys::ext::audio_ports::{
        clap_audio_port_info, clap_plugin_audio_ports, CLAP_EXT_AUDIO_PORTS,
    };
    use clap_sys::factory::plugin_factory::clap_plugin_factory;
    use clap_sys::host::clap_host;
    use clap_sys::id::CLAP_INVALID_ID;
    use clap_sys::plugin::clap_plugin;
    use clap_sys::process::clap_process;
    use clap_sys::process::{clap_process_status, CLAP_PROCESS_CONTINUE};
    use clap_sys::version::CLAP_VERSION;
    use std::cell::Cell;
    use std::cell::RefCell;
    use std::ffi::{c_char, c_void, CStr};
    use std::path::PathBuf;
    use std::ptr;

    #[derive(Clone, Copy, PartialEq, Debug)]
    pub(super) enum Fail {
        Nothing,
        EntryInit,
        NoFactory,
        NoCreate,
        CreateNull,
        NoPluginInit,
        PluginInit,
        NoAudioPorts,
        Activate,
        /// Loads normally; every `process` call reports an error.
        Process,
    }

    thread_local! {
        static FAIL: Cell<Fail> = const { Cell::new(Fail::Nothing) };
        static LOG: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
        /// The ports the fake declares; `None` leaves out the extension.
        static PORTS_DECLARED: RefCell<Option<DeclaredPorts>> =
            const { RefCell::new(None) };
        static SEEN: RefCell<Option<Seen>> = const { RefCell::new(None) };
    }

    fn log(event: &'static str) {
        LOG.with(|log| log.borrow_mut().push(event));
    }

    fn failing(step: Fail) -> bool {
        FAIL.with(Cell::get) == step
    }

    pub(super) fn reset(step: Fail) {
        FAIL.with(|fail| fail.set(step));
        LOG.with(|log| log.borrow_mut().clear());
        SEEN.with(|seen| seen.borrow_mut().take());
        // `NoAudioPorts` declares the extension with no ports at all.
        PORTS_DECLARED.with(|ports| {
            *ports.borrow_mut() = (step == Fail::NoAudioPorts).then(|| (Vec::new(), Vec::new()));
        });
    }

    pub(super) fn events() -> Vec<&'static str> {
        LOG.with(|log| log.borrow().clone())
    }

    pub(super) fn count(event: &str) -> usize {
        events().iter().filter(|logged| **logged == event).count()
    }

    unsafe extern "C" fn entry_init(_path: *const c_char) -> bool {
        log("entry.init");
        !failing(Fail::EntryInit)
    }

    unsafe extern "C" fn entry_deinit() {
        log("entry.deinit");
    }

    unsafe extern "C" fn entry_get_factory(_id: *const c_char) -> *const c_void {
        match FAIL.with(Cell::get) {
            Fail::NoFactory => ptr::null(),
            Fail::NoCreate => (&FACTORY_WITHOUT_CREATE as *const clap_plugin_factory).cast(),
            _ => (&FACTORY as *const clap_plugin_factory).cast(),
        }
    }

    unsafe extern "C" fn factory_create(
        _factory: *const clap_plugin_factory,
        host: *const clap_host,
        _id: *const c_char,
    ) -> *const clap_plugin {
        log("create");
        assert!(!host.is_null());
        if failing(Fail::CreateNull) {
            return ptr::null();
        }
        let init = if failing(Fail::NoPluginInit) {
            None
        } else {
            Some(plugin_init as unsafe extern "C" fn(*const clap_plugin) -> bool)
        };
        Box::into_raw(Box::new(clap_plugin {
            desc: ptr::null(),
            plugin_data: ptr::null_mut(),
            init,
            destroy: Some(plugin_destroy),
            activate: Some(plugin_activate),
            deactivate: Some(plugin_deactivate),
            start_processing: None,
            stop_processing: None,
            reset: None,
            process: Some(plugin_process),
            get_extension: Some(plugin_get_extension),
            on_main_thread: None,
        }))
    }

    unsafe extern "C" fn plugin_init(_plugin: *const clap_plugin) -> bool {
        log("plugin.init");
        !failing(Fail::PluginInit)
    }

    unsafe extern "C" fn plugin_destroy(plugin: *const clap_plugin) {
        log("destroy");
        drop(unsafe { Box::from_raw(plugin as *mut clap_plugin) });
    }

    unsafe extern "C" fn plugin_activate(
        _plugin: *const clap_plugin,
        _rate: f64,
        _min: u32,
        _max: u32,
    ) -> bool {
        log("activate");
        !failing(Fail::Activate)
    }

    unsafe extern "C" fn plugin_deactivate(_plugin: *const clap_plugin) {
        log("deactivate");
    }

    unsafe extern "C" fn plugin_get_extension(
        _plugin: *const clap_plugin,
        id: *const c_char,
    ) -> *const c_void {
        let id = unsafe { CStr::from_ptr(id) };
        if id == CLAP_EXT_AUDIO_PORTS && PORTS_DECLARED.with(|ports| ports.borrow().is_some()) {
            return (&PORTS as *const clap_plugin_audio_ports).cast();
        }
        ptr::null()
    }

    fn declared(is_input: bool) -> Vec<PortInfo> {
        PORTS_DECLARED.with(|ports| {
            let ports = ports.borrow();
            let (inputs, outputs) = ports.as_ref().expect("ports declared");
            if is_input {
                inputs.clone()
            } else {
                outputs.clone()
            }
        })
    }

    unsafe extern "C" fn ports_count(_plugin: *const clap_plugin, is_input: bool) -> u32 {
        declared(is_input).len() as u32
    }

    unsafe extern "C" fn ports_get(
        _plugin: *const clap_plugin,
        index: u32,
        is_input: bool,
        info: *mut clap_audio_port_info,
    ) -> bool {
        let Some(port) = declared(is_input).get(index as usize).copied() else {
            return false;
        };
        unsafe {
            (*info).id = index;
            (*info).channel_count = port.channel_count;
            (*info).flags = port.flags;
            (*info).port_type = ptr::null();
            (*info).in_place_pair = CLAP_INVALID_ID;
        }
        true
    }

    /// Reads every channel of every input port the host passes (so a
    /// port pointing at the wrong memory shows up) and fills each output
    /// channel with `10 * port + channel + 1`.
    unsafe extern "C" fn plugin_process(
        _plugin: *const clap_plugin,
        process: *const clap_process,
    ) -> clap_process_status {
        let process = unsafe { &*process };
        let frames = process.frames_count as usize;
        let inputs = unsafe {
            std::slice::from_raw_parts(process.audio_inputs, process.audio_inputs_count as usize)
        };
        let input_sums = inputs
            .iter()
            .map(|port| {
                (0..port.channel_count as usize)
                    .map(|channel| {
                        let samples = unsafe {
                            std::slice::from_raw_parts(*port.data32.add(channel), frames)
                        };
                        samples.iter().sum::<f32>()
                    })
                    .collect()
            })
            .collect();
        let outputs = unsafe {
            std::slice::from_raw_parts_mut(
                process.audio_outputs,
                process.audio_outputs_count as usize,
            )
        };
        for (index, port) in outputs.iter().enumerate() {
            for channel in 0..port.channel_count as usize {
                let samples =
                    unsafe { std::slice::from_raw_parts_mut(*port.data32.add(channel), frames) };
                samples.fill((10 * index + channel + 1) as f32);
            }
        }
        if failing(Fail::Process) {
            return clap_sys::process::CLAP_PROCESS_ERROR;
        }
        SEEN.with(|seen| {
            *seen.borrow_mut() = Some(Seen {
                inputs_count: process.audio_inputs_count,
                outputs_count: process.audio_outputs_count,
                input_sums,
                output_channels: outputs.iter().map(|port| port.channel_count).collect(),
            });
        });
        CLAP_PROCESS_CONTINUE
    }

    /// What the fake plugin saw in its last `process` call.
    #[derive(Debug, PartialEq)]
    pub(super) struct Seen {
        pub(super) inputs_count: u32,
        pub(super) outputs_count: u32,
        /// Per input port, per channel: the sum of the block.
        pub(super) input_sums: Vec<Vec<f32>>,
        pub(super) output_channels: Vec<u32>,
    }

    /// Makes the fake plugin offer the audio-ports extension with these
    /// `(channel_count, flags)` ports.
    pub(super) fn declare_ports(inputs: &[(u32, u32)], outputs: &[(u32, u32)]) {
        let ports = |list: &[(u32, u32)]| {
            list.iter()
                .map(|&(channel_count, flags)| PortInfo {
                    channel_count,
                    flags,
                })
                .collect()
        };
        PORTS_DECLARED.with(|declared| {
            *declared.borrow_mut() = Some((ports(inputs), ports(outputs)));
        });
    }

    pub(super) fn seen() -> Option<Seen> {
        SEEN.with(|seen| seen.borrow_mut().take())
    }

    static PORTS: clap_plugin_audio_ports = clap_plugin_audio_ports {
        count: Some(ports_count),
        get: Some(ports_get),
    };

    static FACTORY: clap_plugin_factory = clap_plugin_factory {
        get_plugin_count: None,
        get_plugin_descriptor: None,
        create_plugin: Some(factory_create),
    };

    static FACTORY_WITHOUT_CREATE: clap_plugin_factory = clap_plugin_factory {
        get_plugin_count: None,
        get_plugin_descriptor: None,
        create_plugin: None,
    };

    pub(super) static ENTRY: clap_plugin_entry = clap_plugin_entry {
        clap_version: CLAP_VERSION,
        init: Some(entry_init),
        deinit: Some(entry_deinit),
        get_factory: Some(entry_get_factory),
    };

    /// A handle on the test executable itself, standing in for the
    /// plugin library (unloading it only drops a reference).
    pub(super) fn library() -> libloading::Library {
        #[cfg(windows)]
        {
            libloading::os::windows::Library::this()
                .expect("open the test executable")
                .into()
        }
        #[cfg(unix)]
        {
            libloading::os::unix::Library::this().into()
        }
    }

    /// The registry the fake module is loaded through.  Each test thread
    /// uses its own key (the log is per thread too), so parallel tests
    /// never share a fake module.
    pub(super) static REGISTRY: ModuleRegistry = ModuleRegistry::new();

    pub(super) fn key() -> PathBuf {
        PathBuf::from(format!("fake-{:?}.clap", std::thread::current().id()))
    }

    pub(super) fn opened(entry: &'static clap_plugin_entry) -> OpenedModule {
        OpenedModule {
            library: library(),
            entry,
            init_path: c"fake.clap".to_owned(),
        }
    }

    /// Acquires the fake module the way a load or scan does.
    pub(super) fn acquire() -> Result<ModuleHandle, String> {
        REGISTRY.acquire(key(), || Ok(opened(&ENTRY)))
    }

    /// Runs the real load sequence against the fake module.
    pub(super) fn load(plugin_id: &str) -> Result<ClapPlugin, String> {
        let pref = crate::audio::PluginPref::default();
        let restore = Restore::from_pref(&pref);
        let module = acquire()?;
        ClapPlugin::instantiate(module, plugin_id, 48_000, 256, &restore).map(|(plugin, _)| plugin)
    }

    /// A second fake entry that counts `init`/`deinit` across threads
    /// and records whether two of those calls ever overlapped.
    pub(super) mod counted {
        use clap_sys::entry::clap_plugin_entry;
        use clap_sys::version::CLAP_VERSION;
        use std::ffi::{c_char, c_void};
        use std::ptr;
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;

        pub(in super::super) static INITS: AtomicUsize = AtomicUsize::new(0);
        pub(in super::super) static DEINITS: AtomicUsize = AtomicUsize::new(0);
        pub(in super::super) static OVERLAPPED: AtomicBool = AtomicBool::new(false);
        static INSIDE: AtomicBool = AtomicBool::new(false);

        fn enter_and_wait() {
            if INSIDE.swap(true, Ordering::SeqCst) {
                OVERLAPPED.store(true, Ordering::SeqCst);
            }
            // Long enough for a racing caller to arrive.
            std::thread::sleep(Duration::from_millis(20));
            INSIDE.store(false, Ordering::SeqCst);
        }

        unsafe extern "C" fn init(_path: *const c_char) -> bool {
            INITS.fetch_add(1, Ordering::SeqCst);
            enter_and_wait();
            true
        }

        unsafe extern "C" fn deinit() {
            DEINITS.fetch_add(1, Ordering::SeqCst);
            enter_and_wait();
        }

        unsafe extern "C" fn get_factory(_id: *const c_char) -> *const c_void {
            ptr::null()
        }

        pub(in super::super) static ENTRY: clap_plugin_entry = clap_plugin_entry {
            clap_version: CLAP_VERSION,
            init: Some(init),
            deinit: Some(deinit),
            get_factory: Some(get_factory),
        };
    }
}

#[test]
fn instances_and_scans_of_one_module_share_one_init() {
    fake::reset(fake::Fail::Nothing);
    let first = fake::load("fake").expect("load the first instance");
    let second = fake::load("fake").expect("load the second instance");
    // A rescan while both are loaded shares the module too.
    let scan = fake::acquire().expect("scan the module");
    assert!(scan_module(std::path::Path::new("fake.clap"), &scan).is_empty());
    drop(scan);
    assert_eq!(fake::count("entry.init"), 1);
    assert_eq!(fake::count("entry.deinit"), 0);
    drop(first);
    assert_eq!(fake::count("destroy"), 1);
    assert_eq!(fake::count("entry.deinit"), 0);
    assert!(fake::REGISTRY.holds(&fake::key()));
    drop(second);
    assert_eq!(fake::count("destroy"), 2);
    assert_eq!(fake::count("entry.deinit"), 1);
    assert!(!fake::REGISTRY.holds(&fake::key()));
    // The last release came after the last instance was destroyed.
    assert_eq!(fake::events().last(), Some(&"entry.deinit"));
    // A load after everything is released starts the module afresh.
    let third = fake::load("fake").expect("load again");
    assert_eq!(fake::count("entry.init"), 2);
    drop(third);
    assert_eq!(fake::count("entry.deinit"), 2);
}

#[test]
fn a_failed_module_init_is_not_cached() {
    fake::reset(fake::Fail::EntryInit);
    assert!(fake::acquire().is_err());
    assert!(!fake::REGISTRY.holds(&fake::key()));
    fake::reset(fake::Fail::Nothing);
    let plugin = fake::load("fake").expect("the next load retries the init");
    assert_eq!(fake::count("entry.init"), 1);
    drop(plugin);
    assert_eq!(fake::count("entry.deinit"), 1);
}

#[test]
fn a_failed_open_is_not_cached() {
    fake::reset(fake::Fail::Nothing);
    let key = fake::key();
    let error = fake::REGISTRY.acquire(key.clone(), || Err("missing".to_owned()));
    assert!(error.is_err());
    assert!(!fake::REGISTRY.holds(&key));
    let module = fake::acquire().expect("the next acquire opens the module");
    assert_eq!(fake::events(), ["entry.init"]);
    drop(module);
    assert_eq!(fake::events(), ["entry.init", "entry.deinit"]);
}

#[test]
fn concurrent_acquires_init_the_module_once() {
    use fake::counted::{DEINITS, INITS, OVERLAPPED};
    static REGISTRY: ModuleRegistry = ModuleRegistry::new();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let threads: Vec<_> = (0..2)
        .map(|_| {
            let barrier = std::sync::Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                let module = REGISTRY
                    .acquire("counted.clap".into(), || {
                        Ok(fake::opened(&fake::counted::ENTRY))
                    })
                    .expect("acquire the counted module");
                // Both threads hold the module at once before either
                // releases it.
                barrier.wait();
                drop(module);
            })
        })
        .collect();
    for thread in threads {
        thread.join().expect("acquire thread");
    }
    assert_eq!(INITS.load(Ordering::SeqCst), 1);
    assert_eq!(DEINITS.load(Ordering::SeqCst), 1);
    assert!(!OVERLAPPED.load(Ordering::SeqCst));
    assert!(!REGISTRY.holds(std::path::Path::new("counted.clap")));
}

#[test]
fn concurrent_last_releases_still_deinit_the_module() {
    fake::reset(fake::Fail::Nothing);
    static REGISTRY: ModuleRegistry = ModuleRegistry::new();
    // Each drop lingers after unlocking; a drop that still held its
    // reference then would make every concurrent drop see a third
    // reference, and none would release the module.
    REGISTRY.release_pause_ms.store(20, Ordering::SeqCst);
    const THREADS: usize = 4;
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            REGISTRY
                .acquire("released.clap".into(), || Ok(fake::opened(&fake::ENTRY)))
                .expect("acquire the fake module")
        })
        .collect();
    assert_eq!(fake::events(), ["entry.init"]);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(THREADS));
    std::thread::scope(|scope| {
        for handle in handles {
            let barrier = std::sync::Arc::clone(&barrier);
            scope.spawn(move || {
                barrier.wait();
                drop(handle);
            });
        }
    });
    assert!(!REGISTRY.holds(std::path::Path::new("released.clap")));
    // `deinit` ran on whichever thread released last, so this thread's
    // log cannot show it; the empty table is the proof it was released.
}

#[test]
fn module_keys_resolve_different_spellings_of_one_file() {
    let dir = std::env::temp_dir().join(format!("rigwerk-clap-key-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the test folder");
    let file = dir.join("module.clap");
    std::fs::write(&file, b"").expect("create the test file");
    let dotted = dir.join(".").join("module.clap");
    assert_eq!(module_key(&file), module_key(&dotted));
    // A path that cannot be canonicalised still gets an absolute key.
    let missing = std::path::Path::new("no-such-dir/missing.clap");
    assert!(module_key(missing).is_absolute());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_refused_entry_init_is_not_deinitialised() {
    fake::reset(fake::Fail::EntryInit);
    assert!(fake::load("fake").is_err());
    assert_eq!(fake::events(), ["entry.init"]);
}

#[test]
fn failures_before_create_deinit_the_entry_once() {
    for step in [fake::Fail::NoFactory, fake::Fail::NoCreate] {
        fake::reset(step);
        assert!(fake::load("fake").is_err(), "{step:?}");
        assert_eq!(fake::events(), ["entry.init", "entry.deinit"], "{step:?}");
    }
    // A plugin id with a NUL byte fails after `init` too.
    fake::reset(fake::Fail::Nothing);
    assert!(fake::load("fa\0ke").is_err());
    assert_eq!(fake::events(), ["entry.init", "entry.deinit"]);
    // So does a factory that creates nothing.
    fake::reset(fake::Fail::CreateNull);
    assert!(fake::load("fake").is_err());
    assert_eq!(fake::events(), ["entry.init", "create", "entry.deinit"]);
}

#[test]
fn failures_after_create_destroy_then_deinit_once() {
    let cases: [(fake::Fail, &[&str]); 4] = [
        (
            fake::Fail::NoPluginInit,
            &["entry.init", "create", "destroy", "entry.deinit"],
        ),
        (
            fake::Fail::PluginInit,
            &[
                "entry.init",
                "create",
                "plugin.init",
                "destroy",
                "entry.deinit",
            ],
        ),
        (
            fake::Fail::NoAudioPorts,
            &[
                "entry.init",
                "create",
                "plugin.init",
                "destroy",
                "entry.deinit",
            ],
        ),
        (
            fake::Fail::Activate,
            &[
                "entry.init",
                "create",
                "plugin.init",
                "activate",
                "destroy",
                "entry.deinit",
            ],
        ),
    ];
    for (step, expected) in cases {
        fake::reset(step);
        assert!(fake::load("fake").is_err(), "{step:?}");
        assert_eq!(fake::events(), expected, "{step:?}");
    }
}

#[test]
fn a_loaded_plugin_tears_down_once_in_order() {
    fake::reset(fake::Fail::Nothing);
    let plugin = fake::load("fake").expect("load the fake plugin");
    assert_eq!(fake::count("destroy"), 0);
    assert_eq!(fake::count("entry.deinit"), 0);
    drop(plugin);
    assert_eq!(
        fake::events(),
        [
            "entry.init",
            "create",
            "plugin.init",
            "activate",
            "deactivate",
            "destroy",
            "entry.deinit",
        ]
    );
}

#[test]
fn the_entry_guard_deinits_exactly_once() {
    fake::reset(fake::Fail::Nothing);
    let guard =
        unsafe { EntryInit::init(&fake::ENTRY, c"fake.clap") }.expect("the fake entry initialises");
    assert!(!guard.plugin_factory().is_null());
    assert_eq!(fake::count("entry.deinit"), 0);
    drop(guard);
    assert_eq!(fake::events(), ["entry.init", "entry.deinit"]);
}

fn port(channel_count: u32, flags: u32) -> PortInfo {
    PortInfo {
        channel_count,
        flags,
    }
}

fn layout(
    inputs: &[usize],
    outputs: &[usize],
    main_input: usize,
    main_output: usize,
) -> PortLayout {
    PortLayout {
        inputs: inputs.to_vec(),
        outputs: outputs.to_vec(),
        main_input,
        main_output,
    }
}

const MAIN: u32 = CLAP_AUDIO_PORT_IS_MAIN;

/// `(channel_count, flags)` per input and per output port.
type FakePorts<'a> = (&'a [(u32, u32)], &'a [(u32, u32)]);

#[test]
fn port_layout_follows_the_declared_ports() {
    use port_layout;
    let stereo = [port(2, MAIN)];
    // Main at index 0.
    assert_eq!(
        port_layout(Some((&stereo, &stereo))),
        Ok(layout(&[2], &[2], 0, 0))
    );
    // Main at index 1: the flag wins, every port keeps its buffer.
    let late = [port(1, 0), port(2, MAIN)];
    assert_eq!(
        port_layout(Some((&late, &late))),
        Ok(layout(&[1, 2], &[1, 2], 1, 1))
    );
    // No main flag: index 0 stands in.
    let unflagged = [port(2, 0), port(1, 0)];
    assert_eq!(
        port_layout(Some((&unflagged, &stereo))),
        Ok(layout(&[2, 1], &[2], 0, 0))
    );
    // A sidechain input gets its own buffer.
    let sidechain = [port(2, MAIN), port(2, 0)];
    assert_eq!(
        port_layout(Some((&sidechain, &stereo))),
        Ok(layout(&[2, 2], &[2], 0, 0))
    );
    // Mono main ports.
    let mono = [port(1, MAIN)];
    assert_eq!(
        port_layout(Some((&mono, &mono))),
        Ok(layout(&[1], &[1], 0, 0))
    );
    // More than two channels on the main port are set up in full.
    let surround = [port(6, MAIN)];
    assert_eq!(
        port_layout(Some((&surround, &surround))),
        Ok(layout(&[6], &[6], 0, 0))
    );
    // No extension: one stereo input and output, as before.
    assert_eq!(port_layout(None), Ok(layout(&[2], &[2], 0, 0)));
    // An auxiliary port without channels is fine.
    let empty_aux = [port(2, MAIN), port(0, 0)];
    assert_eq!(
        port_layout(Some((&empty_aux, &stereo))),
        Ok(layout(&[2, 0], &[2], 0, 0))
    );
}

#[test]
fn port_layout_rejects_unusable_ports() {
    use port_layout;
    let stereo = [port(2, MAIN)];
    // Zero ports in either direction.
    assert!(port_layout(Some((&[], &stereo))).is_err());
    assert!(port_layout(Some((&stereo, &[]))).is_err());
    assert!(port_layout(Some((&[], &[]))).is_err());
    // A main port without channels.
    assert!(port_layout(Some((&[port(0, MAIN), port(2, 0)], &stereo))).is_err());
    // Nonsense sizes.
    let wide = [port(MAX_PORT_CHANNELS as u32 + 1, MAIN)];
    assert!(port_layout(Some((&wide, &stereo))).is_err());
    let many = vec![port(2, 0); MAX_AUDIO_PORTS + 1];
    assert!(port_layout(Some((&stereo, &many))).is_err());
}

/// Loads the fake with `(inputs, outputs)` ports (`None`: no extension)
/// and runs one block of left 0.5, right -0.25 through it.
fn process_fake(ports: Option<FakePorts<'_>>) -> (fake::Seen, Vec<f32>, Vec<f32>) {
    fake::reset(fake::Fail::Nothing);
    if let Some((inputs, outputs)) = ports {
        fake::declare_ports(inputs, outputs);
    }
    let mut plugin = fake::load("fake").expect("load the fake plugin");
    let mut left = vec![0.5f32; 64];
    let mut right = vec![-0.25f32; 64];
    plugin
        .process(&mut left, &mut right, 64, 1.0)
        .expect("process a block");
    let seen = fake::seen().expect("the plugin processed");
    (seen, left, right)
}

#[test]
fn process_passes_every_declared_port() {
    // A sidechain compressor: main stereo input plus a stereo sidechain.
    let (seen, left, right) = process_fake(Some((&[(2, MAIN), (2, 0)], &[(2, MAIN)])));
    assert_eq!(seen.inputs_count, 2);
    assert_eq!(seen.outputs_count, 1);
    assert_eq!(seen.input_sums, [vec![32.0, -16.0], vec![0.0, 0.0]]);
    assert_eq!(seen.output_channels, [2]);
    assert!(left.iter().all(|sample| *sample == 1.0));
    assert!(right.iter().all(|sample| *sample == 2.0));
}

#[test]
fn process_feeds_and_reads_the_flagged_main_port() {
    // Main ports at index 1: a 6-channel input after a mono aux, a mono
    // output after a stereo aux.
    let (seen, left, right) = process_fake(Some((&[(1, 0), (6, MAIN)], &[(2, 0), (1, MAIN)])));
    assert_eq!(seen.inputs_count, 2);
    assert_eq!(seen.outputs_count, 2);
    assert_eq!(
        seen.input_sums,
        [vec![0.0], vec![32.0, -16.0, 0.0, 0.0, 0.0, 0.0]]
    );
    assert_eq!(seen.output_channels, [2, 1]);
    // Output port 1, channel 0, on both sides.
    assert!(left.iter().all(|sample| *sample == 11.0));
    assert!(right.iter().all(|sample| *sample == 11.0));
}

#[test]
fn process_mixes_a_mono_main_port_down_and_back_up() {
    let (seen, left, right) = process_fake(Some((&[(1, MAIN)], &[(1, MAIN)])));
    assert_eq!(seen.input_sums, [vec![8.0]]);
    assert!(left.iter().chain(&right).all(|sample| *sample == 1.0));
}

#[test]
fn process_without_the_extension_uses_one_stereo_pair() {
    let (seen, left, right) = process_fake(None);
    assert_eq!(seen.inputs_count, 1);
    assert_eq!(seen.outputs_count, 1);
    assert_eq!(seen.input_sums, [vec![32.0, -16.0]]);
    assert_eq!(seen.output_channels, [2]);
    assert!(left.iter().all(|sample| *sample == 1.0));
    assert!(right.iter().all(|sample| *sample == 2.0));
}

#[test]
fn a_zero_channel_port_gets_a_null_pointer() {
    use PortBuffers;
    let buffers = PortBuffers::new(&[2, 0], 0, 8);
    assert_eq!(buffers.ports[0].channel_count, 2);
    assert!(!buffers.ports[0].data32.is_null());
    assert_eq!(buffers.ports[1].channel_count, 0);
    assert!(buffers.ports[1].data32.is_null());
    // The empty default is only a placeholder, but asking for its main
    // port must not panic.
    let mut empty = PortBuffers::default();
    assert!(empty.main_channels().is_empty());
}

#[test]
fn process_passes_a_zero_channel_aux_port_as_empty() {
    let (seen, left, right) = process_fake(Some((&[(2, MAIN), (0, 0)], &[(2, MAIN), (0, 0)])));
    assert_eq!(seen.inputs_count, 2);
    assert_eq!(seen.input_sums, [vec![32.0, -16.0], vec![]]);
    assert_eq!(seen.outputs_count, 2);
    assert_eq!(seen.output_channels, [2, 0]);
    assert!(left.iter().all(|sample| *sample == 1.0));
    assert!(right.iter().all(|sample| *sample == 2.0));
}

#[test]
fn process_restores_pointers_a_plugin_changed() {
    fake::reset(fake::Fail::Nothing);
    fake::declare_ports(&[(2, MAIN)], &[(2, MAIN)]);
    let mut plugin = fake::load("fake").expect("load the fake plugin");
    let mut left = vec![0.5f32; 32];
    let mut right = vec![-0.25f32; 32];
    plugin
        .process(&mut left, &mut right, 32, 1.0)
        .expect("process the first block");
    // A misbehaving plugin can overwrite the buffer descriptions it was
    // handed.  `refresh` must restore everything before the next call.
    for port in &mut plugin.inputs.ports {
        port.data32 = std::ptr::null_mut();
        port.data64 = std::ptr::null_mut();
        port.constant_mask = u64::MAX;
    }
    for port in &mut plugin.outputs.ports {
        port.data32 = std::ptr::null_mut();
        port.channel_count = 0;
        port.constant_mask = u64::MAX;
    }
    left.fill(0.5);
    right.fill(-0.25);
    plugin
        .process(&mut left, &mut right, 32, 1.0)
        .expect("process the second block");
    let seen = fake::seen().expect("the plugin processed");
    assert_eq!(seen.inputs_count, 1);
    assert_eq!(seen.input_sums, [vec![16.0, -8.0]]);
    assert_eq!(seen.output_channels, [2]);
    assert!(left.iter().all(|sample| *sample == 1.0));
    assert!(right.iter().all(|sample| *sample == 2.0));
    for port in plugin.inputs.ports.iter().chain(&plugin.outputs.ports) {
        assert!(port.data64.is_null());
        assert_eq!(port.constant_mask, 0);
    }
    assert_eq!(plugin.outputs.ports[0].channel_count, 2);
}

#[test]
fn process_refreshes_every_sub_block_of_a_long_buffer() {
    fake::reset(fake::Fail::Nothing);
    fake::declare_ports(&[(2, MAIN), (2, 0)], &[(2, MAIN)]);
    let mut plugin = fake::load("fake").expect("load the fake plugin");
    // 600 frames are three calls (256, 256, 88) at the fake's block size.
    let mut left = vec![0.5f32; 600];
    let mut right = vec![-0.25f32; 600];
    plugin
        .process(&mut left, &mut right, 600, 1.0)
        .expect("process a long buffer");
    let seen = fake::seen().expect("the plugin processed");
    assert_eq!(seen.inputs_count, 2);
    assert_eq!(seen.input_sums, [vec![44.0, -22.0], vec![0.0, 0.0]]);
    assert_eq!(seen.output_channels, [2]);
    assert!(left.iter().all(|sample| *sample == 1.0));
    assert!(right.iter().all(|sample| *sample == 2.0));
}

#[test]
fn a_port_rescan_stops_processing_until_reloaded() {
    fake::reset(fake::Fail::Nothing);
    fake::declare_ports(&[(2, MAIN)], &[(2, MAIN)]);
    let mut plugin = fake::load("fake").expect("load the fake plugin");
    let host: *const clap_host = &*plugin._host;
    let mut left = vec![0.5f32; 16];
    let mut right = vec![0.5f32; 16];
    unsafe {
        assert!(host_audio_ports_flag_supported(
            host,
            CLAP_AUDIO_PORTS_RESCAN_LIST
        ));
        assert!(!host_audio_ports_flag_supported(host, 1 << 31));
        // Names and the two metadata flags do not touch the buffers.
        host_audio_ports_rescan(host, CLAP_AUDIO_PORTS_RESCAN_NAMES);
        assert!(!plugin.host_state.ports_changed());
        assert!(plugin.process(&mut left, &mut right, 16, 1.0).is_ok());
        host_audio_ports_rescan(
            host,
            CLAP_AUDIO_PORTS_RESCAN_PORT_TYPE | CLAP_AUDIO_PORTS_RESCAN_IN_PLACE_PAIR,
        );
        assert!(!plugin.host_state.ports_changed());
        assert!(plugin.process(&mut left, &mut right, 16, 1.0).is_ok());
    }
    assert!(fake::seen().is_some());
    unsafe { host_audio_ports_rescan(host, CLAP_AUDIO_PORTS_RESCAN_CHANNEL_COUNT) };
    assert!(plugin.host_state.ports_changed());
    assert!(plugin.process(&mut left, &mut right, 16, 1.0).is_err());
    assert!(fake::seen().is_none(), "the plugin was not called");
}

/// Optional smoke test used while developing the host: point
/// `RIGWERK_CLAP_FIXTURE` at a `.clap` file, then run `cargo test`.
#[test]
fn loads_a_real_clap_effect() {
    let Ok(path) = std::env::var("RIGWERK_CLAP_FIXTURE") else {
        return;
    };
    let path = PathBuf::from(path);
    let candidates = scan_bundle(&path).expect("scan the fixture");
    assert!(!candidates.is_empty(), "fixture exposed no effects");
    let candidate = &candidates[0];
    let mut plugin =
        ClapPlugin::load_fresh(&path, &candidate.plugin_id, 48_000, 512).expect("load the fixture");
    assert!(!plugin.parameters().is_empty());
    if plugin.has_editor() {
        let (width, height) = plugin
            .prepare_editor()
            .expect("create the fixture interface");
        assert!(width > 0 && height > 0);
        plugin.close_editor();
    }
    plugin.set_parameter(1, 0.5).expect("set the fixture gain");
    let mut left = vec![0.25f32; 128];
    let mut right = vec![-0.25f32; 128];
    plugin
        .process(&mut left, &mut right, 128, 1.0)
        .expect("process a block");
    assert!(left.iter().any(|sample| (*sample - 0.125).abs() < 0.001));
}
