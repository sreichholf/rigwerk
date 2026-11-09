use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::Ordering;

use clap_sys::ext::audio_ports::{
    CLAP_AUDIO_PORTS_RESCAN_CHANNEL_COUNT, CLAP_AUDIO_PORTS_RESCAN_IN_PLACE_PAIR,
    CLAP_AUDIO_PORTS_RESCAN_LIST, CLAP_AUDIO_PORTS_RESCAN_NAMES, CLAP_AUDIO_PORTS_RESCAN_PORT_TYPE,
    CLAP_AUDIO_PORT_IS_MAIN,
};
use clap_sys::host::clap_host;

use clap_sys::ext::gui::{clap_gui_resize_hints, CLAP_WINDOW_API_WAYLAND, CLAP_WINDOW_API_X11};

use super::gui::{
    choose_editor_api, linux_candidates, EditorApi, ParentWindow, ResizeHints, WindowApi,
};
use super::host::{
    host_audio_ports_flag_supported, host_audio_ports_rescan, GuiVisibility, HostState, StateSave,
};
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

/// A loaded fake CLAP effect (as [`fake_plugin_for_test`]) that reports
/// `latency` samples through `clap_plugin_latency`.
pub(crate) fn fake_plugin_with_latency_for_test(latency: u32) -> ClapPlugin {
    const MAIN: u32 = clap_sys::ext::audio_ports::CLAP_AUDIO_PORT_IS_MAIN;
    fake::reset(fake::Fail::Nothing);
    fake::set_latency(latency);
    fake::declare_ports(&[(2, MAIN)], &[(2, MAIN)]);
    fake::load("fake").expect("load the fake plugin")
}

/// Makes the fake plugin loaded last on this thread ask for a restart
/// (`clap_host.request_restart`) and report `latency` once reactivated,
/// the way CLAP lets a plugin change its latency while active.
pub(crate) fn restart_fake_with_latency_for_test(latency: u32) {
    fake::set_next_latency(latency);
    fake::request_restart();
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
    use clap_sys::ext::latency::{clap_host_latency, clap_plugin_latency, CLAP_EXT_LATENCY};
    use clap_sys::ext::thread_check::{clap_host_thread_check, CLAP_EXT_THREAD_CHECK};
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
        /// The latency the fake reports; `None` leaves out the extension.
        static LATENCY: Cell<Option<u32>> = const { Cell::new(None) };
        /// A latency the fake switches to at its next `activate`, telling
        /// the host through `latency.changed` as CLAP asks.
        static NEXT_LATENCY: Cell<Option<u32>> = const { Cell::new(None) };
        /// The host of the instance created last, for the helpers that
        /// act as that plugin.
        static HOST: Cell<*const clap_host> = const { Cell::new(ptr::null()) };
    }

    /// One fake instance's lifecycle, kept in `plugin_data` so several
    /// fakes on one thread are checked separately.
    struct Instance {
        host: *const clap_host,
        active: Cell<bool>,
        processing: Cell<bool>,
    }

    fn instance<'a>(plugin: *const clap_plugin) -> &'a Instance {
        unsafe { &*((*plugin).plugin_data as *const Instance) }
    }

    /// Asks the host's `thread_check` whether this is the audio thread.
    fn on_audio_thread(host: *const clap_host) -> bool {
        let host_ref = unsafe { host.as_ref() }.expect("a host");
        let get = host_ref.get_extension.expect("host get_extension");
        let ext =
            unsafe { get(host, CLAP_EXT_THREAD_CHECK.as_ptr()) } as *const clap_host_thread_check;
        let check = unsafe { ext.as_ref() }
            .and_then(|ext| ext.is_audio_thread)
            .expect("the host offers thread_check");
        unsafe { check(host) }
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
        LATENCY.with(|latency| latency.set(None));
        NEXT_LATENCY.with(|latency| latency.set(None));
        gui::reset();
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
        HOST.with(|cell| cell.set(host));
        if failing(Fail::CreateNull) {
            return ptr::null();
        }
        let init = if failing(Fail::NoPluginInit) {
            None
        } else {
            Some(plugin_init as unsafe extern "C" fn(*const clap_plugin) -> bool)
        };
        let state = Box::into_raw(Box::new(Instance {
            host,
            active: Cell::new(false),
            processing: Cell::new(false),
        }));
        Box::into_raw(Box::new(clap_plugin {
            desc: ptr::null(),
            plugin_data: state.cast(),
            init,
            destroy: Some(plugin_destroy),
            activate: Some(plugin_activate),
            deactivate: Some(plugin_deactivate),
            start_processing: Some(plugin_start_processing),
            stop_processing: Some(plugin_stop_processing),
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
        let plugin = unsafe { Box::from_raw(plugin as *mut clap_plugin) };
        drop(unsafe { Box::from_raw(plugin.plugin_data as *mut Instance) });
    }

    unsafe extern "C" fn plugin_activate(
        plugin: *const clap_plugin,
        _rate: f64,
        _min: u32,
        _max: u32,
    ) -> bool {
        log("activate");
        let state = instance(plugin);
        assert!(!state.active.get(), "activate while active");
        if failing(Fail::Activate) {
            return false;
        }
        if let Some(next) = NEXT_LATENCY.with(|latency| latency.take()) {
            LATENCY.with(|latency| latency.set(Some(next)));
            tell_host_latency_changed();
        }
        state.active.set(true);
        true
    }

    unsafe extern "C" fn plugin_deactivate(plugin: *const clap_plugin) {
        log("deactivate");
        let state = instance(plugin);
        // CLAP: [main-thread & active & !processing].
        assert!(state.active.get(), "deactivate while inactive");
        assert!(!state.processing.get(), "deactivate while processing");
        state.active.set(false);
    }

    unsafe extern "C" fn plugin_start_processing(plugin: *const clap_plugin) -> bool {
        log("start_processing");
        let state = instance(plugin);
        // CLAP: [audio-thread & active & !processing].
        assert!(state.active.get(), "start_processing while inactive");
        assert!(!state.processing.get(), "start_processing twice");
        assert!(
            on_audio_thread(state.host),
            "start_processing off the audio thread"
        );
        state.processing.set(true);
        true
    }

    unsafe extern "C" fn plugin_stop_processing(plugin: *const clap_plugin) {
        log("stop_processing");
        let state = instance(plugin);
        // CLAP: [audio-thread & active & processing].
        assert!(state.active.get(), "stop_processing while inactive");
        assert!(state.processing.get(), "stop_processing while stopped");
        assert!(
            on_audio_thread(state.host),
            "stop_processing off the audio thread"
        );
        state.processing.set(false);
    }

    /// Makes the fake report `latency` (with the extension) from now on.
    pub(super) fn set_latency(latency: u32) {
        LATENCY.with(|cell| cell.set(Some(latency)));
    }

    /// Makes the fake switch to `latency` at its next activation.
    pub(super) fn set_next_latency(latency: u32) {
        NEXT_LATENCY.with(|cell| cell.set(Some(latency)));
    }

    /// Calls `clap_host_latency.changed` the way a plugin does: through
    /// the extension the host hands out.
    pub(super) fn tell_host_latency_changed() {
        let host = HOST.with(Cell::get);
        let host_ref = unsafe { host.as_ref() }.expect("the fake was created");
        let get = host_ref.get_extension.expect("host get_extension");
        let ext = unsafe { get(host, CLAP_EXT_LATENCY.as_ptr()) } as *const clap_host_latency;
        let changed = unsafe { ext.as_ref() }
            .and_then(|ext| ext.changed)
            .expect("the host offers latency.changed");
        unsafe { changed(host) };
    }

    /// Calls `clap_host.request_restart` the way a plugin does.
    pub(super) fn request_restart() {
        let host = HOST.with(Cell::get);
        let host_ref = unsafe { host.as_ref() }.expect("the fake was created");
        let request = host_ref.request_restart.expect("host request_restart");
        unsafe { request(host) };
    }

    unsafe extern "C" fn latency_get(plugin: *const clap_plugin) -> u32 {
        log("latency.get");
        // CLAP: [main-thread & active].
        assert!(instance(plugin).active.get(), "latency.get while inactive");
        LATENCY.with(Cell::get).unwrap_or(0)
    }

    static LATENCY_EXT: clap_plugin_latency = clap_plugin_latency {
        get: Some(latency_get),
    };

    unsafe extern "C" fn plugin_get_extension(
        _plugin: *const clap_plugin,
        id: *const c_char,
    ) -> *const c_void {
        let id = unsafe { CStr::from_ptr(id) };
        if id == CLAP_EXT_AUDIO_PORTS && PORTS_DECLARED.with(|ports| ports.borrow().is_some()) {
            return (&PORTS as *const clap_plugin_audio_ports).cast();
        }
        if id == CLAP_EXT_LATENCY && LATENCY.with(Cell::get).is_some() {
            return (&LATENCY_EXT as *const clap_plugin_latency).cast();
        }
        gui::extension(id)
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
        plugin: *const clap_plugin,
        process: *const clap_process,
    ) -> clap_process_status {
        let state = instance(plugin);
        // CLAP: [audio-thread & active & processing].
        assert!(state.active.get(), "process while inactive");
        assert!(state.processing.get(), "process before start_processing");
        assert!(on_audio_thread(state.host), "process off the audio thread");
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

    /// The fake's GUI and `posix-fd-support` extensions, each left out
    /// until a test offers it.
    pub(super) mod gui {
        use super::log;
        use clap_sys::ext::gui::{clap_gui_resize_hints, clap_plugin_gui, CLAP_EXT_GUI};
        use clap_sys::ext::posix_fd_support::{
            clap_plugin_posix_fd_support, CLAP_EXT_POSIX_FD_SUPPORT,
        };
        use clap_sys::plugin::clap_plugin;
        use std::cell::{Cell, RefCell};
        use std::ffi::{c_char, c_void, CStr};
        use std::ptr;

        /// What the fake's GUI agrees to.
        #[derive(Clone)]
        pub(in super::super) struct Gui {
            /// `(api, floating)` pairs `is_api_supported` accepts.
            pub(in super::super) supported: Vec<(&'static CStr, bool)>,
            pub(in super::super) resizable: bool,
            /// `get_resize_hints`; `None` leaves the call failing.
            pub(in super::super) hints: Option<clap_gui_resize_hints>,
            /// `adjust_size` rounds both sides down to this step; 0 refuses.
            pub(in super::super) step: u32,
            pub(in super::super) accept_size: bool,
            pub(in super::super) size: (u32, u32),
        }

        impl Default for Gui {
            fn default() -> Self {
                Self {
                    supported: Vec::new(),
                    resizable: false,
                    hints: None,
                    step: 0,
                    accept_size: true,
                    size: (400, 300),
                }
            }
        }

        thread_local! {
            static GUI: RefCell<Option<Gui>> = const { RefCell::new(None) };
            static FDS: Cell<bool> = const { Cell::new(false) };
            static FD_CALLS: RefCell<Vec<(i32, u32)>> = const { RefCell::new(Vec::new()) };
        }

        pub(in super::super) fn reset() {
            GUI.with(|gui| gui.borrow_mut().take());
            FDS.with(|fds| fds.set(false));
            FD_CALLS.with(|calls| calls.borrow_mut().clear());
        }

        pub(in super::super) fn offer(gui: Gui) {
            GUI.with(|cell| *cell.borrow_mut() = Some(gui));
        }

        #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
        pub(in super::super) fn offer_fd_support() {
            FDS.with(|fds| fds.set(true));
        }

        /// The `(fd, flags)` the plugin's `on_fd` was called with.
        #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
        pub(in super::super) fn fd_calls() -> Vec<(i32, u32)> {
            FD_CALLS.with(|calls| calls.borrow().clone())
        }

        /// The size the fake's view has now.
        pub(in super::super) fn size() -> (u32, u32) {
            with(|gui| gui.size)
        }

        fn with<T>(read: impl FnOnce(&mut Gui) -> T) -> T {
            GUI.with(|gui| read(gui.borrow_mut().as_mut().expect("the GUI is offered")))
        }

        pub(super) fn extension(id: &CStr) -> *const c_void {
            if id == CLAP_EXT_GUI && GUI.with(|gui| gui.borrow().is_some()) {
                return (&GUI_EXT as *const clap_plugin_gui).cast();
            }
            if id == CLAP_EXT_POSIX_FD_SUPPORT && FDS.with(Cell::get) {
                return (&FD_EXT as *const clap_plugin_posix_fd_support).cast();
            }
            ptr::null()
        }

        unsafe extern "C" fn is_api_supported(
            _plugin: *const clap_plugin,
            api: *const c_char,
            floating: bool,
        ) -> bool {
            let api = unsafe { CStr::from_ptr(api) };
            with(|gui| gui.supported.contains(&(api, floating)))
        }

        unsafe extern "C" fn create(
            _plugin: *const clap_plugin,
            _api: *const c_char,
            floating: bool,
        ) -> bool {
            log(if floating {
                "gui.create.floating"
            } else {
                "gui.create.embedded"
            });
            true
        }

        unsafe extern "C" fn destroy(_plugin: *const clap_plugin) {
            log("gui.destroy");
        }

        unsafe extern "C" fn set_scale(_plugin: *const clap_plugin, _scale: f64) -> bool {
            log("gui.set_scale");
            true
        }

        unsafe extern "C" fn get_size(
            _plugin: *const clap_plugin,
            width: *mut u32,
            height: *mut u32,
        ) -> bool {
            let (w, h) = with(|gui| gui.size);
            unsafe {
                *width = w;
                *height = h;
            }
            true
        }

        unsafe extern "C" fn can_resize(_plugin: *const clap_plugin) -> bool {
            with(|gui| gui.resizable)
        }

        unsafe extern "C" fn get_resize_hints(
            _plugin: *const clap_plugin,
            hints: *mut clap_gui_resize_hints,
        ) -> bool {
            match with(|gui| gui.hints) {
                Some(value) => {
                    unsafe { *hints = value };
                    true
                }
                None => false,
            }
        }

        unsafe extern "C" fn adjust_size(
            _plugin: *const clap_plugin,
            width: *mut u32,
            height: *mut u32,
        ) -> bool {
            log("gui.adjust_size");
            let step = with(|gui| gui.step);
            if step == 0 {
                return false;
            }
            unsafe {
                *width -= *width % step;
                *height -= *height % step;
            }
            true
        }

        unsafe extern "C" fn set_size(
            _plugin: *const clap_plugin,
            width: u32,
            height: u32,
        ) -> bool {
            log("gui.set_size");
            with(|gui| {
                if gui.accept_size {
                    gui.size = (width, height);
                }
                gui.accept_size
            })
        }

        unsafe extern "C" fn suggest_title(_plugin: *const clap_plugin, _title: *const c_char) {
            log("gui.suggest_title");
        }

        unsafe extern "C" fn show(_plugin: *const clap_plugin) -> bool {
            log("gui.show");
            true
        }

        unsafe extern "C" fn hide(_plugin: *const clap_plugin) -> bool {
            log("gui.hide");
            true
        }

        static GUI_EXT: clap_plugin_gui = clap_plugin_gui {
            is_api_supported: Some(is_api_supported),
            get_preferred_api: None,
            create: Some(create),
            destroy: Some(destroy),
            set_scale: Some(set_scale),
            get_size: Some(get_size),
            can_resize: Some(can_resize),
            get_resize_hints: Some(get_resize_hints),
            adjust_size: Some(adjust_size),
            set_size: Some(set_size),
            set_parent: None,
            set_transient: None,
            suggest_title: Some(suggest_title),
            show: Some(show),
            hide: Some(hide),
        };

        unsafe extern "C" fn on_fd(_plugin: *const clap_plugin, fd: i32, flags: u32) {
            FD_CALLS.with(|calls| calls.borrow_mut().push((fd, flags)));
        }

        static FD_EXT: clap_plugin_posix_fd_support =
            clap_plugin_posix_fd_support { on_fd: Some(on_fd) };
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

#[test]
fn the_latency_is_read_once_the_plugin_is_active() {
    let plugin = fake_plugin_with_latency_for_test(64);
    assert_eq!(plugin.latency_samples(), 64);
    assert_eq!(plugin.host().latency(), 64);
    // `latency.get` came after `activate` (the fake asserts it is active).
    let events = fake::events();
    let activate = events.iter().position(|event| *event == "activate");
    let get = events.iter().position(|event| *event == "latency.get");
    assert!(activate < get, "{events:?}");

    let plain = fake_plugin_for_test(false);
    assert_eq!(plain.latency_samples(), 0);
    assert_eq!(fake::count("latency.get"), 0);
}

/// CLAP only lets the latency change inside `activate` or through a
/// restart, but some plugins call `latency.changed` while active; the
/// value is read again all the same.
#[test]
fn a_latency_change_while_active_is_read_again() {
    let plugin = fake_plugin_with_latency_for_test(64);
    let host = plugin.host();
    let plugin = std::sync::Mutex::new(plugin);
    // Nothing pending: servicing does not touch the instance.
    assert!(!crate::audio::plugins::service_clap(&host, &plugin));

    fake::set_latency(32);
    fake::tell_host_latency_changed();
    // Only a flag so far; the value is read on the main thread.
    assert_eq!(host.latency(), 64);
    assert!(crate::audio::plugins::service_clap(&host, &plugin));
    assert_eq!(host.latency(), 32);
    assert!(!crate::audio::plugins::service_clap(&host, &plugin));
    assert_eq!(fake::count("deactivate"), 0, "no restart was asked for");
}

#[test]
fn a_restart_stops_on_the_audio_thread_then_reactivates_on_the_main_thread() {
    let plugin = fake_plugin_with_latency_for_test(64);
    let host = plugin.host();
    let plugin = std::sync::Mutex::new(plugin);
    let mut left = vec![0.5f32; 16];
    let mut right = vec![0.5f32; 16];
    let process = |left: &mut [f32], right: &mut [f32]| {
        plugin
            .lock()
            .expect("plugin lock")
            .process(left, right, 16, 1.0)
    };
    assert!(process(&mut left, &mut right).is_ok());
    assert!(fake::seen().is_some());
    assert_eq!(fake::count("start_processing"), 1);

    // A mode switch: the new latency applies at the next activation.
    fake::set_next_latency(128);
    fake::request_restart();

    // Still processing, so the main thread waits for the audio thread.
    assert!(!crate::audio::plugins::service_clap(&host, &plugin));
    assert_eq!(fake::count("deactivate"), 0);
    assert_eq!(host.latency(), 64);

    // The audio thread stops processing and passes the block through.
    left.fill(0.5);
    assert!(process(&mut left, &mut right).is_ok());
    assert!(fake::seen().is_none(), "the plugin was not processed");
    assert!(left.iter().all(|sample| *sample == 0.5));
    assert_eq!(fake::count("stop_processing"), 1);

    // The main thread restarts it and reads the new latency.
    assert!(crate::audio::plugins::service_clap(&host, &plugin));
    assert_eq!(host.latency(), 128);
    assert_eq!(fake::count("deactivate"), 1);
    assert_eq!(fake::count("activate"), 2);
    assert!(!host.lifecycle_pending());

    // Processing starts again.
    assert!(process(&mut left, &mut right).is_ok());
    assert!(fake::seen().is_some());
    assert_eq!(fake::count("start_processing"), 2);
}

#[test]
fn a_restart_before_the_first_block_happens_right_away() {
    let plugin = fake_plugin_with_latency_for_test(16);
    let host = plugin.host();
    let plugin = std::sync::Mutex::new(plugin);
    fake::set_next_latency(48);
    fake::request_restart();
    assert!(crate::audio::plugins::service_clap(&host, &plugin));
    assert_eq!(host.latency(), 48);
    assert_eq!(fake::count("stop_processing"), 0);
    assert_eq!(fake::count("deactivate"), 1);
}

#[test]
fn a_restart_runs_on_the_main_thread_once_the_stream_has_stopped() {
    let plugin = fake_plugin_with_latency_for_test(64);
    let host = plugin.host();
    let plugin = std::sync::Mutex::new(plugin);
    let mut left = vec![0.5f32; 16];
    let mut right = vec![0.5f32; 16];
    // Started, then the stream stops: no audio callback will stop it.
    assert!(plugin
        .lock()
        .expect("plugin lock")
        .process(&mut left, &mut right, 16, 1.0)
        .is_ok());
    fake::set_next_latency(256);
    fake::request_restart();
    assert!(!crate::audio::plugins::service_clap(&host, &plugin));
    assert_eq!(fake::count("deactivate"), 0);
    std::thread::sleep(super::plugin::RESTART_STALL + std::time::Duration::from_millis(50));
    assert!(crate::audio::plugins::service_clap(&host, &plugin));
    assert_eq!(host.latency(), 256);
    // Stopped (as the audio thread, which the fake checks) before the
    // deactivation.
    assert_eq!(fake::count("stop_processing"), 1);
    assert_eq!(fake::count("deactivate"), 1);
    assert!(!host.lifecycle_pending());
}

#[test]
fn a_huge_latency_is_clamped() {
    let plugin = fake_plugin_with_latency_for_test(u32::MAX);
    assert_eq!(plugin.latency_samples(), crate::audio::plugins::MAX_ALIGN);
}

/// Loads the fake with this GUI and its editor API chosen from
/// `candidates`, as a load in that session would.
fn fake_with_gui(gui: fake::gui::Gui, candidates: &[EditorApi]) -> ClapPlugin {
    const MAIN: u32 = CLAP_AUDIO_PORT_IS_MAIN;
    fake::reset(fake::Fail::Nothing);
    fake::gui::offer(gui);
    fake::declare_ports(&[(2, MAIN)], &[(2, MAIN)]);
    let mut plugin = fake::load("fake").expect("load the fake plugin");
    plugin.editor_api = choose_editor_api(plugin.plugin, plugin.gui_ext, candidates);
    plugin
}

fn gui_events() -> Vec<&'static str> {
    fake::events()
        .into_iter()
        .filter(|event| event.starts_with("gui."))
        .collect()
}

#[test]
fn linux_prefers_an_x11_parent_and_floats_on_pure_wayland() {
    let embedded = |api| EditorApi {
        api,
        floating: false,
    };
    let floating = |api| EditorApi {
        api,
        floating: true,
    };
    assert_eq!(
        linux_candidates(true, true),
        vec![
            embedded(WindowApi::X11),
            floating(WindowApi::X11),
            floating(WindowApi::Wayland),
        ]
    );
    assert_eq!(
        linux_candidates(false, true),
        vec![floating(WindowApi::Wayland)]
    );
    assert!(linux_candidates(false, false).is_empty());
}

#[test]
fn the_editor_api_is_the_first_one_the_plugin_supports() {
    let x11_and_wayland = linux_candidates(true, true);
    let gui = |supported| fake::gui::Gui {
        supported,
        ..Default::default()
    };
    let plugin = fake_with_gui(
        gui(vec![
            (CLAP_WINDOW_API_WAYLAND, true),
            (CLAP_WINDOW_API_X11, false),
        ]),
        &x11_and_wayland,
    );
    assert_eq!(
        plugin.editor_api(),
        Some(EditorApi {
            api: WindowApi::X11,
            floating: false
        })
    );
    drop(plugin);

    // A plugin that only floats still has an editor.
    let plugin = fake_with_gui(gui(vec![(CLAP_WINDOW_API_WAYLAND, true)]), &x11_and_wayland);
    assert_eq!(
        plugin.editor_api(),
        Some(EditorApi {
            api: WindowApi::Wayland,
            floating: true
        })
    );
    drop(plugin);

    let plugin = fake_with_gui(
        gui(vec![(CLAP_WINDOW_API_X11, false)]),
        &linux_candidates(false, true),
    );
    assert!(!plugin.has_editor());
}

#[test]
fn a_plugin_without_the_gui_extension_has_no_editor() {
    let plugin = fake_plugin_for_test(false);
    let candidates = linux_candidates(true, true);
    assert_eq!(
        choose_editor_api(plugin.plugin, plugin.gui_ext, &candidates),
        None
    );
}

#[test]
fn a_floating_editor_is_created_titled_shown_and_destroyed() {
    let mut plugin = fake_with_gui(
        fake::gui::Gui {
            supported: vec![(CLAP_WINDOW_API_WAYLAND, true)],
            ..Default::default()
        },
        &linux_candidates(false, true),
    );
    assert_eq!(plugin.prepare_editor(), Ok((400, 300)));
    plugin
        .show_floating_editor("Fake - CLAP", 2.0)
        .expect("show the floating editor");
    // A floating editor cannot take a parent.
    assert!(plugin.attach_editor(ParentWindow::X11(1), 1.0).is_err());
    plugin.close_editor();
    assert_eq!(
        gui_events(),
        vec![
            "gui.create.floating",
            "gui.set_scale",
            "gui.suggest_title",
            "gui.show",
            "gui.hide",
            "gui.destroy",
        ]
    );
}

#[test]
fn a_fixed_editor_keeps_its_size_when_the_window_is_dragged() {
    let mut plugin = fake_with_gui(
        fake::gui::Gui {
            supported: vec![(CLAP_WINDOW_API_X11, false)],
            ..Default::default()
        },
        &linux_candidates(true, false),
    );
    plugin.prepare_editor().expect("create the editor");
    assert_eq!(plugin.editor_resize_hints(), ResizeHints::FIXED);
    assert_eq!(plugin.resize_editor(900, 700), (400, 300));
    assert!(!gui_events().contains(&"gui.set_size"));
}

#[test]
fn a_user_resize_goes_through_adjust_size_first() {
    let mut plugin = fake_with_gui(
        fake::gui::Gui {
            supported: vec![(CLAP_WINDOW_API_X11, false)],
            resizable: true,
            step: 50,
            ..Default::default()
        },
        &linux_candidates(true, false),
    );
    plugin.prepare_editor().expect("create the editor");
    assert!(plugin.editor_resize_hints().resizable());
    assert_eq!(plugin.resize_editor(523, 377), (500, 350));
    assert_eq!(fake::gui::size(), (500, 350));
    let events = gui_events();
    let adjust = events.iter().position(|event| *event == "gui.adjust_size");
    let set = events.iter().position(|event| *event == "gui.set_size");
    assert!(adjust.is_some() && adjust < set, "{events:?}");
}

#[test]
fn a_refused_resize_snaps_the_window_back() {
    let mut plugin = fake_with_gui(
        fake::gui::Gui {
            supported: vec![(CLAP_WINDOW_API_X11, false)],
            resizable: true,
            accept_size: false,
            ..Default::default()
        },
        &linux_candidates(true, false),
    );
    plugin.prepare_editor().expect("create the editor");
    // `adjust_size` refuses (step 0), so the size is passed unchanged and
    // `set_size` refuses that too.
    assert_eq!(plugin.resize_editor(523, 377), (400, 300));
}

#[test]
fn resize_hints_come_from_the_plugin() {
    let plugin = fake_with_gui(
        fake::gui::Gui {
            supported: vec![(CLAP_WINDOW_API_X11, false)],
            resizable: true,
            hints: Some(clap_gui_resize_hints {
                can_resize_horizontally: true,
                can_resize_vertically: false,
                preserve_aspect_ratio: true,
                aspect_ratio_width: 16,
                aspect_ratio_height: 9,
            }),
            ..Default::default()
        },
        &linux_candidates(true, false),
    );
    assert_eq!(
        plugin.editor_resize_hints(),
        ResizeHints {
            horizontal: true,
            vertical: false,
            aspect: Some((16, 9)),
        }
    );
}

#[test]
fn show_hide_and_hint_requests_wait_for_the_window() {
    let shared = std::sync::Arc::new(HostState::new());
    let host = super::plugin::make_host(&shared);
    let host: *const clap_host = &host;
    let gui = &super::host::HOST_GUI;
    assert_eq!(shared.take_gui_visibility(), None);
    unsafe {
        assert!((gui.request_hide.expect("request_hide"))(host));
        assert!((gui.request_show.expect("request_show"))(host));
    }
    // The later request wins, and is taken once.
    assert_eq!(shared.take_gui_visibility(), Some(GuiVisibility::Show));
    assert_eq!(shared.take_gui_visibility(), None);
    assert!(!shared.take_gui_hints_changed());
    unsafe { (gui.resize_hints_changed.expect("resize_hints_changed"))(host) };
    assert!(shared.take_gui_hints_changed());
    assert!(!shared.take_gui_hints_changed());
}

#[cfg(target_os = "linux")]
#[test]
fn posix_fds_are_registered_once_and_polled_for_the_plugin() {
    use clap_sys::ext::posix_fd_support::{CLAP_POSIX_FD_READ, CLAP_POSIX_FD_WRITE};

    let _main = super::claim_main_thread_for_test();
    const MAIN: u32 = CLAP_AUDIO_PORT_IS_MAIN;
    fake::reset(fake::Fail::Nothing);
    fake::gui::offer_fd_support();
    fake::declare_ports(&[(2, MAIN)], &[(2, MAIN)]);
    let plugin = fake::load("fake").expect("load the fake plugin");
    let host: *const clap_host = &*plugin._host;
    let fd_ext = &super::host::HOST_POSIX_FD;
    let register = fd_ext.register_fd.expect("register_fd");
    let modify = fd_ext.modify_fd.expect("modify_fd");
    let unregister = fd_ext.unregister_fd.expect("unregister_fd");

    let mut pipe = [0i32; 2];
    assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
    let [read_end, write_end] = pipe;
    unsafe {
        assert!(register(host, read_end, CLAP_POSIX_FD_WRITE));
        assert!(
            !register(host, read_end, CLAP_POSIX_FD_READ),
            "registered twice"
        );
        assert!(
            !modify(host, write_end, CLAP_POSIX_FD_READ),
            "not registered"
        );
    }
    // Nothing to read and only writes watched: no call.
    plugin.host_state.service_main_thread();
    assert!(fake::gui::fd_calls().is_empty());

    unsafe { assert!(modify(host, read_end, CLAP_POSIX_FD_READ)) };
    assert_eq!(
        unsafe { libc::write(write_end, b"x".as_ptr().cast(), 1) },
        1
    );
    plugin.host_state.service_main_thread();
    assert_eq!(fake::gui::fd_calls(), vec![(read_end, CLAP_POSIX_FD_READ)]);

    unsafe {
        assert!(unregister(host, read_end));
        assert!(!unregister(host, read_end));
    }
    plugin.host_state.service_main_thread();
    assert_eq!(fake::gui::fd_calls().len(), 1);
    drop(plugin);
    unsafe {
        libc::close(read_end);
        libc::close(write_end);
    }
}
