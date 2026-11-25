use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

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
    host_audio_ports_flag_supported, host_audio_ports_rescan, GuiVisibility, HostState,
};
use super::instance::ClapInstance;
use super::module::{module_key, EntryInit, ModuleRegistry};
use super::ports::{port_layout, PortBuffers, PortInfo, PortLayout, MAX_PORT_CHANNELS};
use super::scan::{
    contained, decode_scan, encode_scan, failure_rows, resolve, scan_bundle, scan_bundles,
    scan_library_isolated, scan_module, wire_candidates, write_scan_file, ClapCandidate,
    ScanFailure, PANIC_WHILE_READ,
};
use super::state::{StateReader, StateWriter};
use crate::audio::plugins::{
    ClapFailure, LoadedPlugin, Outcome, PluginHandle, PluginPref, PluginShared, Restore, Saved,
    MAX_AUDIO_PORTS,
};

/// A save while a value is still queued for the plugin captures anyway
/// (the stream may never deliver it), marks that value undelivered in the
/// snapshot and keeps the slot stale; once a block delivered it, the next
/// save captures a blob that holds it.
#[test]
fn state_save_with_values_queued_marks_them_undelivered() {
    let _main = crate::audio::claim_main_thread();
    fake::reset(fake::Fail::Nothing);
    fake::set_state(b"state");
    fake::offer_params(true);
    fake::declare_ports(&[(2, MAIN)], &[(2, MAIN)]);
    let mut plugin = fake::load("fake").expect("load the fake plugin");
    let handle = plugin.handle().clone();
    let (mut left, mut right) = (vec![0.0f32; 16], vec![0.0f32; 16]);
    // Processing, so a value goes through the transfer to the next block.
    assert_eq!(
        plugin.run_for_test(&mut left, &mut right),
        Outcome::Processed
    );
    handle.set_parameter(7, 0.25).expect("set the gain");
    handle.capture_state();
    assert_eq!(fake::count("state.save"), 1);
    let state = handle.saved().1.expect("captured anyway");
    // The queued value is left out of the snapshot, so a restore applies it.
    assert!(state.params.iter().all(|param| param.id != 7));
    assert!(handle.saved().0.iter().any(|param| param.id == 7));
    assert!(handle.wants_capture(), "captured again once delivered");
    // Once a block delivered it, the next save holds it.
    assert_eq!(
        plugin.run_for_test(&mut left, &mut right),
        Outcome::Processed
    );
    handle.capture_state();
    assert!(!handle.wants_capture());
    let state = handle.saved().1.expect("captured");
    assert_eq!(state.bytes().as_deref(), Some(&b"state"[..]));
    assert_eq!(
        state.params,
        handle.saved().0,
        "the snapshot is the saved values"
    );
}

/// A block that fails does not count the values it took as applied: the
/// next save still leaves them out of the snapshot.
/// A value the full event list could not take is kept and goes with the
/// next block; only after that block are the values applied.
#[test]
fn a_value_a_full_event_list_dropped_arrives_with_the_next_block() {
    let _main = crate::audio::claim_main_thread();
    fake::reset(fake::Fail::Nothing);
    fake::set_state(b"state");
    fake::offer_params(true);
    fake::declare_ports(&[(2, MAIN)], &[(2, MAIN)]);
    let mut plugin = fake::load("fake").expect("load the fake plugin");
    let handle = plugin.handle().clone();
    let (mut left, mut right) = (vec![0.0f32; 16], vec![0.0f32; 16]);
    assert_eq!(
        plugin.run_for_test(&mut left, &mut right),
        Outcome::Processed
    );
    plugin.clap_processor_for_test().limit_events_for_test(1);
    handle.set_parameter(7, 0.25).expect("set the gain");
    handle.set_parameter(9, 1.0).expect("set the mode");
    assert_eq!(
        plugin.run_for_test(&mut left, &mut right),
        Outcome::Processed
    );
    let got = |id: u32| {
        fake::received()
            .iter()
            .any(|&(seen, _, how)| seen == id && how == "process")
    };
    assert!(got(7) != got(9), "one value went, one was kept");
    handle.capture_state();
    let state = handle.saved().1.expect("captured");
    assert!(state
        .params
        .iter()
        .all(|param| param.id != 7 && param.id != 9));
    assert!(handle.wants_capture());
    assert_eq!(
        plugin.run_for_test(&mut left, &mut right),
        Outcome::Processed
    );
    assert!(got(7) && got(9), "the kept value arrived");
    handle.capture_state();
    let state = handle.saved().1.expect("captured");
    assert!(state.params.iter().any(|param| param.id == 7));
    assert!(state.params.iter().any(|param| param.id == 9));
    assert!(!handle.wants_capture());
}

/// Two values a full list kept: with room for one, the first goes with the
/// next block and the second survives it, to arrive with the one after.
#[test]
fn two_values_a_full_event_list_kept_arrive_one_block_at_a_time() {
    let _main = crate::audio::claim_main_thread();
    fake::reset(fake::Fail::Nothing);
    fake::offer_params(true);
    fake::declare_ports(&[(2, MAIN)], &[(2, MAIN)]);
    let mut plugin = fake::load("fake").expect("load the fake plugin");
    let handle = plugin.handle().clone();
    let (mut left, mut right) = (vec![0.0f32; 16], vec![0.0f32; 16]);
    plugin.run_for_test(&mut left, &mut right);
    let received = || {
        fake::received()
            .iter()
            .filter(|&&(_, _, how)| how == "process")
            .count()
    };
    plugin.clap_processor_for_test().limit_events_for_test(0);
    handle.set_parameter(7, 0.25).expect("set the gain");
    handle.set_parameter(9, 1.0).expect("set the mode");
    plugin.run_for_test(&mut left, &mut right);
    assert_eq!(received(), 0, "both kept");
    plugin.clap_processor_for_test().limit_events_for_test(1);
    plugin.run_for_test(&mut left, &mut right);
    assert_eq!(received(), 1);
    plugin.run_for_test(&mut left, &mut right);
    assert_eq!(received(), 2, "the second arrived a block later");
    plugin.run_for_test(&mut left, &mut right);
    assert_eq!(received(), 2, "and only once");
}

/// With the stream stopped the UI hands a value to `params.flush` itself:
/// it is applied at once, so the next capture lists it.
#[test]
fn a_value_flushed_while_stopped_is_in_the_next_capture() {
    let _main = crate::audio::claim_main_thread();
    fake::reset(fake::Fail::Nothing);
    fake::set_state(b"state");
    fake::offer_params(true);
    fake::declare_ports(&[(2, MAIN)], &[(2, MAIN)]);
    let plugin = fake::load("fake").expect("load the fake plugin");
    let handle = plugin.handle().clone();
    handle.set_parameter(7, 0.25).expect("set the gain");
    assert!(fake::received()
        .iter()
        .any(|&(id, _, how)| id == 7 && how == "flush"));
    handle.capture_state();
    let state = handle.saved().1.expect("captured");
    assert!(state.params.iter().any(|param| param.id == 7));
    assert!(!handle.wants_capture());
}

#[test]
fn a_failed_block_does_not_count_its_values_applied() {
    let _main = crate::audio::claim_main_thread();
    fake::reset(fake::Fail::Nothing);
    fake::set_state(b"state");
    fake::offer_params(true);
    fake::declare_ports(&[(2, MAIN)], &[(2, MAIN)]);
    let mut plugin = fake::load("fake").expect("load the fake plugin");
    let handle = plugin.handle().clone();
    let (mut left, mut right) = (vec![0.0f32; 16], vec![0.0f32; 16]);
    assert_eq!(
        plugin.run_for_test(&mut left, &mut right),
        Outcome::Processed
    );
    fake::fail_from_now(fake::Fail::Process);
    handle.set_parameter(7, 0.25).expect("set the gain");
    assert_eq!(plugin.run_for_test(&mut left, &mut right), Outcome::Failed);
    handle.capture_state();
    let state = handle.saved().1.expect("captured");
    assert!(state.params.iter().all(|param| param.id != 7));
    assert!(handle.wants_capture());
}

/// With the stream stopped a value set on a processing CLAP effect never
/// reaches it, but a save still captures, and a restore applies the value
/// on top of the older blob.
#[test]
fn a_capture_with_audio_stopped_after_a_set_keeps_the_value_through_a_restore() {
    let _main = crate::audio::claim_main_thread();
    let stateful = || {
        fake::reset(fake::Fail::Nothing);
        fake::set_state(b"older");
        fake::offer_params(true);
        fake::declare_ports(&[(2, MAIN)], &[(2, MAIN)]);
    };
    stateful();
    let mut plugin = fake::load("fake").expect("load the fake plugin");
    let handle = plugin.handle().clone();
    let (mut left, mut right) = (vec![0.0f32; 16], vec![0.0f32; 16]);
    plugin.run_for_test(&mut left, &mut right);
    handle.set_parameter(7, 0.75).expect("set the gain");
    // The stream stops here: nothing delivers the value.
    handle.capture_state();
    let (params, state) = handle.saved();
    assert!(state.is_some(), "captured with audio stopped");
    let pref = PluginPref {
        params,
        state,
        ..PluginPref::default()
    };
    assert!(pref.state.as_ref().unwrap().params.is_empty());
    drop(handle);
    drop(plugin);
    stateful();
    let restored = fake::load_restoring("fake", &pref).expect("restore the fake");
    assert!(fake::count("state.load") > 0, "the blob was restored");
    let parameters = restored.handle().parameters();
    let gain = parameters
        .iter()
        .find(|view| view.id == 7)
        .expect("the gain");
    assert!((gain.value - 0.75).abs() < 1e-9, "restored {}", gain.value);
    assert!(fake_received_for_test()
        .iter()
        .any(|&(id, value, _)| id == 7 && (value - 1.5).abs() < 1e-9));
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
/// fake's settings are per thread: load it on the thread that sets them.
pub(crate) fn fake_plugin_for_test(fail_process: bool) -> LoadedPlugin {
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
pub(crate) fn fake_plugin_with_latency_for_test(latency: u32) -> LoadedPlugin {
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

/// A loaded fake CLAP effect (as [`fake_plugin_for_test`]) with the state
/// extension: `state.save` writes `state`, until
/// [`set_fake_state_for_test`] changes it.
pub(crate) fn fake_plugin_with_state_for_test(state: &[u8]) -> LoadedPlugin {
    const MAIN: u32 = clap_sys::ext::audio_ports::CLAP_AUDIO_PORT_IS_MAIN;
    fake::reset(fake::Fail::Nothing);
    fake::set_state(state);
    fake::declare_ports(&[(2, MAIN)], &[(2, MAIN)]);
    fake::load("fake").expect("load the fake plugin")
}

/// A fake (as [`fake_plugin_for_test`]) with two controls (see
/// `fake::offer_params`, with `params.flush`), restored from `pref` the way
/// a real load restores it.
pub(crate) fn fake_restoring_for_test(pref: &PluginPref) -> LoadedPlugin {
    fake::reset(fake::Fail::Nothing);
    fake::offer_params(true);
    fake::declare_ports(&[(2, MAIN)], &[(2, MAIN)]);
    fake::load_restoring("fake", pref).expect("load the fake plugin")
}

/// A fake (as [`fake_plugin_for_test`]) with two controls (id 7 "Gain",
/// 0 to 2; id 9 "Mode", stepped 0 to 3) and `params.flush`.  It records
/// every value it receives (see [`fake_received_for_test`]).
pub(crate) fn fake_plugin_with_params_for_test() -> LoadedPlugin {
    fake_restoring_for_test(&PluginPref::default())
}

/// As [`fake_plugin_with_params_for_test`], reporting `value` for control
/// `id` through its output port on every block, the way a CLAP plugin does
/// when its own state changes (see `fake::emit_output`).  `value` is native,
/// as the plugin's control range takes it.
pub(crate) fn fake_plugin_reporting_for_test(id: u32, value: f64) -> LoadedPlugin {
    const MAIN: u32 = clap_sys::ext::audio_ports::CLAP_AUDIO_PORT_IS_MAIN;
    fake::reset(fake::Fail::Nothing);
    fake::offer_params(true);
    fake::emit_output(id, value);
    fake::declare_ports(&[(2, MAIN)], &[(2, MAIN)]);
    fake::load("fake").expect("load the fake plugin")
}

/// The `(id, native value, "process" | "flush")` the fake loaded last on
/// this thread received, in order, from whichever thread.
pub(crate) fn fake_received_for_test() -> Vec<(u32, f64, &'static str)> {
    fake::received()
}

/// The gate of the fake loaded last on this thread: while `hold` is set,
/// its `process` waits inside the call (as a plugin busy with a long
/// block), with `inside` set.  Any thread may use them.
pub(crate) fn fake_process_gate_for_test() -> (Arc<AtomicBool>, Arc<AtomicBool>) {
    fake::gate()
}

/// What the stateful fakes on this thread save from now on.
pub(crate) fn set_fake_state_for_test(state: &[u8]) {
    fake::set_state(state);
}

/// The `clap_host` a loaded CLAP plugin was handed.
fn host_of(handle: &PluginHandle) -> *const clap_host {
    handle
        .with_clap(|plugin| &*plugin.core._host as *const clap_host)
        .expect("a CLAP plugin")
}

/// Runs `body` on a loaded fake's main-thread instance.
fn clap<R>(plugin: &LoadedPlugin, body: impl FnOnce(&mut ClapInstance) -> R) -> R {
    plugin.handle().with_clap(body).expect("a CLAP plugin")
}

/// Makes `plugin` report a channel-count change, as a real plugin does
/// through `clap_host_audio_ports.rescan`.
pub(crate) fn rescan_ports_for_test(handle: &PluginHandle) {
    let host = host_of(handle);
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
    use super::super::instance::ClapInstance;
    use super::super::module::{ModuleHandle, ModuleRegistry, OpenedModule};
    use super::super::ports::{DeclaredPorts, PortInfo};
    use crate::audio::plugins::{LoadedPlugin, PluginPref, PluginShared, Restore, Saved};
    use clap_sys::entry::clap_plugin_entry;
    use clap_sys::events::{
        clap_event_header, clap_event_param_value, clap_input_events, clap_output_events,
        CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_PARAM_VALUE,
    };
    use clap_sys::ext::audio_ports::{
        clap_audio_port_info, clap_plugin_audio_ports, CLAP_EXT_AUDIO_PORTS,
    };
    use clap_sys::ext::latency::{clap_host_latency, clap_plugin_latency, CLAP_EXT_LATENCY};
    use clap_sys::ext::params::{
        clap_param_info, clap_plugin_params, CLAP_EXT_PARAMS, CLAP_PARAM_IS_STEPPED,
    };
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
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

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
        /// Loads normally; `start_processing` refuses.
        StartProcessing,
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
        /// What `state.save` writes; `None` leaves out the extension.
        static STATE: RefCell<Option<Vec<u8>>> = const { RefCell::new(None) };
        /// The params extension: `None` leaves it out, `Some(flush)` offers
        /// it with or without `params.flush`.
        static PARAMS: Cell<Option<bool>> = const { Cell::new(None) };
        /// An output value every `process` call reports.
        static EMIT: Cell<Option<(u32, f64)>> = const { Cell::new(None) };
        /// The instance created last, for the helpers that look at it.
        static LAST: Cell<*const Instance> = const { Cell::new(ptr::null()) };
        /// The control list after a rescan: "Mode" becomes id 11.
        static RENUMBERED: Cell<bool> = const { Cell::new(false) };
    }

    pub(super) fn set_state(state: &[u8]) {
        STATE.with(|saved| *saved.borrow_mut() = Some(state.to_vec()));
    }

    unsafe extern "C" fn state_save(
        _plugin: *const clap_plugin,
        stream: *const clap_sys::stream::clap_ostream,
    ) -> bool {
        log("state.save");
        let bytes = STATE.with(|state| state.borrow().clone().unwrap_or_default());
        let stream = unsafe { &*stream };
        let write = stream.write.expect("ostream write");
        let written = unsafe { write(stream, bytes.as_ptr() as *const c_void, bytes.len() as u64) };
        written == bytes.len() as i64
    }

    /// Accepts any state (its bytes are not the controls' values).
    unsafe extern "C" fn state_load(
        _plugin: *const clap_plugin,
        _stream: *const clap_sys::stream::clap_istream,
    ) -> bool {
        log("state.load");
        true
    }

    static STATE_EXT: clap_sys::ext::state::clap_plugin_state =
        clap_sys::ext::state::clap_plugin_state {
            save: Some(state_save),
            load: Some(state_load),
        };

    /// One fake instance's lifecycle, kept in `plugin_data` so several
    /// fakes on one thread are checked separately.  Atomics and mutexes:
    /// a test may process it on another thread than the one that loaded it.
    struct Instance {
        host: *const clap_host,
        active: AtomicBool,
        processing: AtomicBool,
        fail_start: bool,
        emit: Option<(u32, f64)>,
        /// The controls' current native values.
        values: Mutex<Vec<(u32, f64)>>,
        /// Every value received: `(id, value, "process" | "flush")`.
        received: Mutex<Vec<(u32, f64, &'static str)>>,
        hold: Arc<AtomicBool>,
        inside: Arc<AtomicBool>,
    }

    /// The fake's controls: `(id, name, min, max, default, flags)`.
    const PARAM_INFO: [(u32, &str, f64, f64, f64, u32); 2] = [
        (7, "Gain", 0.0, 2.0, 1.0, 0),
        (9, "Mode", 0.0, 3.0, 0.0, CLAP_PARAM_IS_STEPPED),
    ];

    /// Offers the params extension, with `params.flush` or without.
    pub(super) fn offer_params(flush: bool) {
        PARAMS.with(|params| params.set(Some(flush)));
    }

    /// Makes every `process` call report `value` for `id`.
    pub(super) fn emit_output(id: u32, value: f64) {
        EMIT.with(|emit| emit.set(Some((id, value))));
    }

    fn last<'a>() -> &'a Instance {
        let last = LAST.with(Cell::get);
        assert!(!last.is_null(), "a fake was created on this thread");
        unsafe { &*last }
    }

    pub(super) fn received() -> Vec<(u32, f64, &'static str)> {
        last().received.lock().unwrap().clone()
    }

    pub(super) fn gate() -> (Arc<AtomicBool>, Arc<AtomicBool>) {
        let last = last();
        (Arc::clone(&last.hold), Arc::clone(&last.inside))
    }

    /// Records the values in `events` (CLAP param-value events only).
    fn receive(state: &Instance, events: *const clap_input_events, how: &'static str) {
        let events = unsafe { &*events };
        let size = events.size.expect("events size");
        let get = events.get.expect("events get");
        for index in 0..unsafe { size(events) } {
            let header = unsafe { &*get(events, index) };
            assert_eq!(header.space_id, CLAP_CORE_EVENT_SPACE_ID);
            if header.type_ != CLAP_EVENT_PARAM_VALUE {
                continue;
            }
            let event =
                unsafe { &*(header as *const clap_event_header).cast::<clap_event_param_value>() };
            state
                .received
                .lock()
                .unwrap()
                .push((event.param_id, event.value, how));
            let mut values = state.values.lock().unwrap();
            if let Some(value) = values.iter_mut().find(|(id, _)| *id == event.param_id) {
                value.1 = event.value;
            }
        }
    }

    /// Reports `emit` through `out`, as a plugin reports its own change.
    fn report(state: &Instance, out: *const clap_output_events) {
        let Some((id, value)) = state.emit else {
            return;
        };
        let out = unsafe { &*out };
        let event = clap_event_param_value {
            header: clap_event_header {
                size: std::mem::size_of::<clap_event_param_value>() as u32,
                time: 0,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: CLAP_EVENT_PARAM_VALUE,
                flags: 0,
            },
            param_id: id,
            cookie: ptr::null_mut(),
            note_id: -1,
            port_index: -1,
            channel: -1,
            key: -1,
            value,
        };
        let push = out.try_push.expect("try_push");
        assert!(unsafe { push(out, &event.header) });
    }

    unsafe extern "C" fn params_count(_plugin: *const clap_plugin) -> u32 {
        PARAM_INFO.len() as u32
    }

    unsafe extern "C" fn params_get_info(
        _plugin: *const clap_plugin,
        index: u32,
        info: *mut clap_param_info,
    ) -> bool {
        let Some(&(mut id, name, min, max, default, flags)) = PARAM_INFO.get(index as usize) else {
            return false;
        };
        if id == 9 && RENUMBERED.with(Cell::get) {
            id = 11;
        }
        let info = unsafe { &mut *info };
        info.id = id;
        info.flags = flags;
        info.cookie = ptr::null_mut();
        for (slot, byte) in info.name.iter_mut().zip(name.bytes().chain([0])) {
            *slot = byte as c_char;
        }
        info.module[0] = 0;
        info.min_value = min;
        info.max_value = max;
        info.default_value = default;
        true
    }

    unsafe extern "C" fn params_get_value(
        plugin: *const clap_plugin,
        id: u32,
        value: *mut f64,
    ) -> bool {
        let values = instance(plugin).values.lock().unwrap();
        match values.iter().find(|(known, _)| *known == id) {
            Some((_, current)) => {
                unsafe { *value = *current };
                true
            }
            None => false,
        }
    }

    unsafe extern "C" fn params_value_to_text(
        _plugin: *const clap_plugin,
        _id: u32,
        value: f64,
        buffer: *mut c_char,
        capacity: u32,
    ) -> bool {
        let text = format!("{value:.3}");
        if text.len() >= capacity as usize {
            return false;
        }
        for (index, byte) in text.bytes().chain([0]).enumerate() {
            unsafe { *buffer.add(index) = byte as c_char };
        }
        true
    }

    unsafe extern "C" fn params_flush(
        plugin: *const clap_plugin,
        events: *const clap_input_events,
        out: *const clap_output_events,
    ) {
        log("params.flush");
        let state = instance(plugin);
        // CLAP: [active ? audio-thread : main-thread], never during process.
        assert!(!state.inside.load(Ordering::SeqCst), "flush during process");
        if state.active.load(Ordering::SeqCst) {
            assert!(
                on_audio_thread(state.host),
                "an active flush off the audio thread"
            );
        } else {
            assert!(
                !on_audio_thread(state.host),
                "an inactive flush as the audio thread"
            );
        }
        receive(state, events, "flush");
        report(state, out);
    }

    static PARAMS_EXT: clap_plugin_params = clap_plugin_params {
        count: Some(params_count),
        get_info: Some(params_get_info),
        get_value: Some(params_get_value),
        value_to_text: Some(params_value_to_text),
        text_to_value: None,
        flush: Some(params_flush),
    };

    static PARAMS_EXT_WITHOUT_FLUSH: clap_plugin_params = clap_plugin_params {
        flush: None,
        ..PARAMS_EXT
    };

    fn instance<'a>(plugin: *const clap_plugin) -> &'a Instance {
        unsafe { &*((*plugin).plugin_data as *const Instance) }
    }

    fn thread_check<'a>(host: *const clap_host) -> &'a clap_host_thread_check {
        let host_ref = unsafe { host.as_ref() }.expect("a host");
        let get = host_ref.get_extension.expect("host get_extension");
        let ext =
            unsafe { get(host, CLAP_EXT_THREAD_CHECK.as_ptr()) } as *const clap_host_thread_check;
        unsafe { ext.as_ref() }.expect("the host offers thread_check")
    }

    /// Asks the host's `thread_check` whether this is the audio thread.
    fn on_audio_thread(host: *const clap_host) -> bool {
        let check = thread_check(host).is_audio_thread.expect("is_audio_thread");
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
        STATE.with(|state| state.borrow_mut().take());
        PARAMS.with(|params| params.set(None));
        EMIT.with(|emit| emit.set(None));
        RENUMBERED.with(|renumbered| renumbered.set(false));
        gui::reset();
        // `NoAudioPorts` declares the extension with no ports at all.
        PORTS_DECLARED.with(|ports| {
            *ports.borrow_mut() = (step == Fail::NoAudioPorts).then(|| (Vec::new(), Vec::new()));
        });
    }

    /// Makes the step `step` go wrong from now on, without resetting the
    /// fake (a later activation fails, say).
    pub(super) fn fail_from_now(step: Fail) {
        FAIL.with(|fail| fail.set(step));
    }

    /// The `clap_host` of the instance created last.
    fn last_host<'a>() -> &'a clap_host {
        let host = HOST.with(Cell::get);
        unsafe { host.as_ref() }.expect("the fake was created")
    }

    fn host_params<'a>() -> &'a clap_sys::ext::params::clap_host_params {
        let host = last_host();
        let get = host.get_extension.expect("host get_extension");
        let ext = unsafe { get(host, CLAP_EXT_PARAMS.as_ptr()) }
            as *const clap_sys::ext::params::clap_host_params;
        unsafe { ext.as_ref() }.expect("the host offers params")
    }

    /// Calls `clap_host_params.request_flush`, as a plugin whose own
    /// editor changed a value does.
    pub(super) fn request_flush() {
        let request = host_params().request_flush.expect("request_flush");
        unsafe { request(last_host()) };
    }

    /// Calls `clap_host_params.rescan`; with `renumber`, "Mode" comes back
    /// as id 11.
    pub(super) fn rescan_params(renumber: bool) {
        RENUMBERED.with(|renumbered| renumbered.set(renumber));
        let rescan = host_params().rescan.expect("rescan");
        unsafe { rescan(last_host(), clap_sys::ext::params::CLAP_PARAM_RESCAN_ALL) };
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
            active: AtomicBool::new(false),
            processing: AtomicBool::new(false),
            fail_start: failing(Fail::StartProcessing),
            emit: EMIT.with(Cell::get),
            values: Mutex::new(
                PARAM_INFO
                    .iter()
                    .map(|&(id, _, _, _, default, _)| (id, default))
                    .collect(),
            ),
            received: Mutex::new(Vec::new()),
            hold: Arc::new(AtomicBool::new(false)),
            inside: Arc::new(AtomicBool::new(false)),
        }));
        LAST.with(|last| last.set(state));
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
        LAST.with(|last| {
            if last.get() == plugin.plugin_data as *const Instance {
                last.set(ptr::null());
            }
        });
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
        assert!(
            !state.active.load(Ordering::SeqCst),
            "activate while active"
        );
        assert!(
            !on_audio_thread(state.host),
            "a main-thread call as the audio thread"
        );
        if failing(Fail::Activate) {
            return false;
        }
        if let Some(next) = NEXT_LATENCY.with(|latency| latency.take()) {
            LATENCY.with(|latency| latency.set(Some(next)));
            tell_host_latency_changed();
        }
        state.active.store(true, Ordering::SeqCst);
        true
    }

    unsafe extern "C" fn plugin_deactivate(plugin: *const clap_plugin) {
        log("deactivate");
        let state = instance(plugin);
        // CLAP: [main-thread & active & !processing].
        assert!(
            state.active.load(Ordering::SeqCst),
            "deactivate while inactive"
        );
        assert!(
            !state.processing.load(Ordering::SeqCst),
            "deactivate while processing"
        );
        assert!(
            !on_audio_thread(state.host),
            "a main-thread call as the audio thread"
        );
        state.active.store(false, Ordering::SeqCst);
    }

    unsafe extern "C" fn plugin_start_processing(plugin: *const clap_plugin) -> bool {
        log("start_processing");
        let state = instance(plugin);
        // CLAP: [audio-thread & active & !processing].
        assert!(
            state.active.load(Ordering::SeqCst),
            "start_processing while inactive"
        );
        assert!(
            !state.processing.load(Ordering::SeqCst),
            "start_processing twice"
        );
        assert!(
            on_audio_thread(state.host),
            "start_processing off the audio thread"
        );
        if state.fail_start {
            return false;
        }
        state.processing.store(true, Ordering::SeqCst);
        true
    }

    unsafe extern "C" fn plugin_stop_processing(plugin: *const clap_plugin) {
        log("stop_processing");
        let state = instance(plugin);
        // CLAP: [audio-thread & active & processing].
        assert!(
            state.active.load(Ordering::SeqCst),
            "stop_processing while inactive"
        );
        assert!(
            state.processing.load(Ordering::SeqCst),
            "stop_processing while stopped"
        );
        assert!(
            on_audio_thread(state.host),
            "stop_processing off the audio thread"
        );
        state.processing.store(false, Ordering::SeqCst);
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
        assert!(
            instance(plugin).active.load(Ordering::SeqCst),
            "latency.get while inactive"
        );
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
        if id == CLAP_EXT_PARAMS {
            match PARAMS.with(Cell::get) {
                Some(true) => return (&PARAMS_EXT as *const clap_plugin_params).cast(),
                Some(false) => {
                    return (&PARAMS_EXT_WITHOUT_FLUSH as *const clap_plugin_params).cast()
                }
                None => {}
            }
        }
        if id == clap_sys::ext::state::CLAP_EXT_STATE
            && STATE.with(|state| state.borrow().is_some())
        {
            return (&STATE_EXT as *const clap_sys::ext::state::clap_plugin_state).cast();
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
        assert!(
            state.active.load(Ordering::SeqCst),
            "process while inactive"
        );
        assert!(
            state.processing.load(Ordering::SeqCst),
            "process before start_processing"
        );
        assert!(on_audio_thread(state.host), "process off the audio thread");
        let process = unsafe { &*process };
        state.inside.store(true, Ordering::SeqCst);
        while state.hold.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        receive(state, process.in_events, "process");
        report(state, process.out_events);
        state.inside.store(false, Ordering::SeqCst);
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
    pub(super) fn load(plugin_id: &str) -> Result<LoadedPlugin, String> {
        load_restoring(plugin_id, &PluginPref::default())
    }

    /// As [`load`], restoring `pref` the way a real load does.
    pub(super) fn load_restoring(
        plugin_id: &str,
        pref: &PluginPref,
    ) -> Result<LoadedPlugin, String> {
        let restore = Restore::from_pref(pref);
        let module = acquire()?;
        let shared = Arc::new(PluginShared::new(0));
        let loaded = ClapInstance::instantiate(module, plugin_id, 48_000, 256, &restore, &shared)?;
        let current = loaded.state_current;
        Ok(LoadedPlugin::clap(
            loaded,
            shared,
            Saved {
                params: pref.params.clone(),
                state: pref.state.clone(),
                stale: !current,
            },
        ))
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
fn a_bundle_that_cannot_be_opened_becomes_a_row_and_the_scan_goes_on() {
    // Two empty bundles: real paths, real `dlopen`, the same `Err` a broken
    // bundle takes (on macOS a `.clap` is a folder, so a folder works there).
    let dir = std::env::temp_dir().join(format!("rigwerk-clap-scan-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the test folder");
    let first = dir.join("First.clap");
    let second = dir.join("Second.clap");
    std::fs::create_dir_all(&first).expect("create the first bundle");
    std::fs::create_dir_all(&second).expect("create the second bundle");
    let rows = scan_bundles(vec![second.clone(), first.clone()]);
    let names: Vec<_> = rows.iter().map(|row| row.name.as_str()).collect();
    assert_eq!(
        names,
        ["First", "Second"],
        "the scan continued past the first"
    );
    for row in &rows {
        assert!(row.plugin_id.is_empty(), "there is no id to load");
        let error = row.scan_error.as_deref().expect("why the bundle failed");
        assert!(error.contains("CLAP load failed"), "{error}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_panic_while_a_bundle_is_read_becomes_a_scan_error() {
    // A panic in the bundle's own `extern "C" init` aborts where it happens,
    // so this cannot be reached through a fake entry: it exercises the
    // unwinding `contained` does catch, which is the host-side panic that
    // would otherwise abort the whole scan.
    let read: Result<(), String> = contained(|| panic!("a bundle panicked"));
    let error = read.expect_err("the panic becomes a scan error");
    assert_eq!(error, format!("{PANIC_WHILE_READ} a bundle panicked"));
    // A payload without text still reports, with the sentence alone.
    let silent: Result<(), String> = contained(|| std::panic::panic_any(7u8));
    assert_eq!(
        silent.expect_err("a payload without text still reports"),
        PANIC_WHILE_READ
    );
}

/// The `--clap-scan` file a child writes and a parent reads: every field the
/// browser uses survives the trip, including a path with a space and a
/// non-ASCII character, and re-encoding the decoded rows gives the same text.
#[test]
fn the_scan_file_round_trips_every_row() {
    let rows = vec![
        ClapCandidate {
            path: PathBuf::from("/usr/lib/clap/Gain.clap"),
            plugin_id: "com.example.gain".to_owned(),
            name: "Gain".to_owned(),
            vendor: "Example".to_owned(),
            category: "audio-effect".to_owned(),
            features: vec!["audio-effect".to_owned()],
            scan_error: None,
        },
        ClapCandidate {
            path: PathBuf::from("Plug-ins/Nice Amp Ünïcode.clap"),
            plugin_id: "org.nice.amp".to_owned(),
            name: "Nice Amp".to_owned(),
            vendor: String::new(),
            category: "audio-effect · stereo".to_owned(),
            features: vec!["audio-effect".to_owned(), "stereo".to_owned()],
            scan_error: None,
        },
        ClapCandidate {
            path: PathBuf::from("/usr/lib/clap/Broken.clap"),
            plugin_id: String::new(),
            name: "Broken".to_owned(),
            vendor: String::new(),
            category: "CLAP effect".to_owned(),
            features: Vec::new(),
            scan_error: Some("CLAP load failed: not a library".to_owned()),
        },
    ];
    let text = encode_scan(rows.clone()).expect("encode the scan");
    let back = decode_scan(&text).expect("decode the scan");
    assert_eq!(back.len(), rows.len());
    for (before, after) in rows.iter().zip(&back) {
        assert_eq!(after.path, before.path, "path");
        assert_eq!(after.plugin_id, before.plugin_id, "plugin id");
        assert_eq!(after.name, before.name, "name");
        assert_eq!(after.vendor, before.vendor, "vendor");
        assert_eq!(after.category, before.category, "category");
        assert_eq!(after.features, before.features, "features");
        assert_eq!(after.scan_error, before.scan_error, "scan error");
    }
    assert_eq!(
        encode_scan(back).expect("encode again"),
        text,
        "the whole row survives, not only the fields checked above"
    );
    assert!(text.contains("Ünïcode"), "{text}");
}

/// A file tagged with another version is refused whole: the parent never reads
/// its fields with this build's meanings.
#[test]
fn a_scan_file_from_another_version_is_rejected() {
    let row = "(path:\"x.clap\",plugin_id:\"id\",name:\"X\",vendor:\"\",\
               category:\"CLAP effect\",features:[],scan_error:None)";
    let current = format!("(version:1,candidates:[{row}])");
    assert_eq!(
        decode_scan(&current).expect("version 1 decodes").len(),
        1,
        "the fixture is valid, so the refusals below are about the version"
    );
    assert!(decode_scan(&format!("(version:2,candidates:[{row}])")).is_none());
    assert!(decode_scan(&format!("(version:0,candidates:[{row}])")).is_none());
    // Text that is not the wire format at all is no result either.
    assert!(decode_scan("not ron").is_none());
    assert!(decode_scan("").is_none());
}

/// A path with no UTF-8 encoding: a raw byte on Unix, an unpaired surrogate on
/// Windows.
#[cfg(unix)]
fn odd_bundle_path() -> PathBuf {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    PathBuf::from(OsString::from_vec(b"/usr/lib/clap/\xff.clap".to_vec()))
}

#[cfg(windows)]
fn odd_bundle_path() -> PathBuf {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    // A lone high surrogate: `Path::to_str` rejects it, and no RON string can
    // carry it.
    PathBuf::from(OsString::from_wide(&[
        0xD800, 0x002E, 0x0063, 0x006C, 0x0061, 0x0070,
    ]))
}

/// RON carries a path as text.  A bundle whose path is not valid UTF-8 cannot
/// cross as itself, so it becomes one error row; failing the whole file would
/// hide every other bundle in the scan.
#[test]
fn a_bundle_path_that_is_not_utf8_becomes_a_row_not_a_failed_scan() {
    let rows = wire_candidates(vec![ClapCandidate {
        path: odd_bundle_path(),
        plugin_id: "com.example.odd".to_owned(),
        name: "Odd".to_owned(),
        vendor: String::new(),
        category: "audio-effect".to_owned(),
        features: vec!["audio-effect".to_owned()],
        scan_error: None,
    }]);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, "Odd", "the row still says which bundle");
    assert!(rows[0].plugin_id.is_empty(), "there is no id to load");
    assert!(rows[0].features.is_empty());
    assert!(rows[0].path.to_str().is_some(), "the path can be encoded");
    let error = rows[0].scan_error.as_deref().expect("a reason");
    assert!(error.contains("UTF-8"), "{error}");
    assert!(encode_scan(rows).is_ok(), "the rest of the scan survives");
}

/// The child creates its result with `create_new`, so it can never clobber a
/// path, and what it writes is exactly what the parent decodes.
#[test]
fn write_scan_file_creates_once_and_writes_a_decodable_scan() {
    let dir = std::env::temp_dir().join(format!("rigwerk-clap-wire-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the test folder");
    let path = dir.join("result.ron");
    let _ = std::fs::remove_file(&path);
    let row = || ClapCandidate {
        path: PathBuf::from("Plug-ins/Write Test.clap"),
        plugin_id: "com.example.write".to_owned(),
        name: "Write Test".to_owned(),
        vendor: "Example".to_owned(),
        category: "audio-effect".to_owned(),
        features: vec!["audio-effect".to_owned()],
        scan_error: None,
    };
    write_scan_file(&path, vec![row()]).expect("write the result");
    let text = std::fs::read_to_string(&path).expect("read the result");
    let back = decode_scan(&text).expect("the parent decodes it");
    assert_eq!(
        encode_scan(back).expect("encode the decoded rows"),
        encode_scan(wire_candidates(vec![row()])).expect("encode the expected rows"),
        "the file holds exactly the rows the child meant to send"
    );
    // A second child must not replace a file the parent is about to read.
    assert!(
        write_scan_file(&path, vec![row()]).is_err(),
        "create_new refuses an existing path"
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("still there"),
        text,
        "the refused write left the file alone"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// The parent's spawn is not tested here: under `cargo test` the running binary
// is the test harness, and `--clap-scan` would be taken by libtest rather than
// by `main`.  Only a real `rigwerk` launch exercises the child; the row mapping
// it falls back to is pure and is what this test covers.

/// A scan that gives nothing back reports one row per root, with no plugin id
/// and the cause, so the browser can say where CLAP was looked for.
#[test]
fn a_failed_isolated_scan_reports_one_row_per_root() {
    let roots = vec![
        PathBuf::from("/usr/lib/clap"),
        PathBuf::from("AppData/Local/Programs/Common/CLAP"),
    ];
    let cases = [
        (ScanFailure::TimedOut, "did not finish in time"),
        (
            ScanFailure::Abandoned,
            "abandoned a plugin that would not return",
        ),
        (ScanFailure::Unwritable, "could not write its result"),
        (
            ScanFailure::Exited("exit code: 7".to_owned()),
            "ended unexpectedly (exit code: 7)",
        ),
        (ScanFailure::Spawn, "could not be started"),
        (ScanFailure::Unreadable, "produced no readable result"),
        (ScanFailure::NoTime, "ran out of time before it could start"),
    ];
    for (cause, fragment) in cases {
        let rows = failure_rows(&roots, &cause);
        assert_eq!(rows.len(), roots.len(), "{cause:?}");
        for (row, root) in rows.iter().zip(&roots) {
            assert_eq!(row.path.as_path(), root.as_path(), "{cause:?}");
            assert_eq!(row.name, root.display().to_string(), "{cause:?}");
            assert!(row.plugin_id.is_empty(), "{cause:?}");
            assert!(row.features.is_empty(), "{cause:?}");
            let error = row.scan_error.as_deref().expect("a reason");
            assert!(error.contains(fragment), "{cause:?}: {error}");
        }
    }
}

/// A result passes through untouched and a failure becomes the rows, so the
/// two arms of `scan_library_isolated` cannot be swapped unnoticed.
#[test]
fn resolve_passes_a_scan_through_and_turns_a_failure_into_rows() {
    let roots = vec![PathBuf::from("/usr/lib/clap")];
    let found = vec![ClapCandidate {
        path: PathBuf::from("/usr/lib/clap/Gain.clap"),
        plugin_id: "com.example.gain".to_owned(),
        name: "Gain".to_owned(),
        vendor: String::new(),
        category: "audio-effect".to_owned(),
        features: vec!["audio-effect".to_owned()],
        scan_error: None,
    }];
    let passed = resolve(Ok(found), &roots);
    assert_eq!(passed.len(), 1, "the scan's own rows, not the roots");
    assert_eq!(passed[0].plugin_id, "com.example.gain");
    assert!(passed[0].scan_error.is_none());
    let failed = resolve(Err(ScanFailure::Spawn), &roots);
    assert_eq!(failed.len(), roots.len());
    assert_eq!(failed[0].path, roots[0], "the root stands in for the scan");
    assert!(failed[0].plugin_id.is_empty());
    let error = failed[0].scan_error.as_deref().expect("the cause");
    assert!(error.contains("could not be started"), "{error}");
}

/// A zero budget is the one arm that must not reach the spawn: the rows name
/// every scan root with the "ran out of time" cause, which is a different
/// sentence from any a child that was actually started can produce.
#[test]
fn a_zero_budget_reports_the_roots_without_starting_a_child() {
    let roots = super::scan_roots();
    let rows = scan_library_isolated(Duration::ZERO);
    assert_eq!(rows.len(), roots.len());
    for (row, root) in rows.iter().zip(&roots) {
        assert_eq!(&row.path, root);
        assert!(row.plugin_id.is_empty());
        assert!(row.features.is_empty());
        let error = row.scan_error.as_deref().expect("a reason");
        assert!(
            error.contains("ran out of time before it could start"),
            "{error}"
        );
    }
}

/// The child's result folder is this scan's alone: on Unix its mode has no
/// group or other bits, so a predictable file name inside it cannot be read
/// or pre-created by another user.
#[cfg(unix)]
#[test]
fn a_scan_result_folder_is_private_to_this_user() {
    use super::scan::scan_result_dir;
    use std::os::unix::fs::PermissionsExt;

    let dir = scan_result_dir().expect("a folder for the result");
    let mode = std::fs::metadata(&dir)
        .expect("stat the folder")
        .permissions()
        .mode();
    assert_eq!(mode & 0o077, 0, "mode {mode:o}");
    std::fs::remove_dir(&dir).expect("remove the folder");
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
    assert_eq!(
        plugin.run_for_test(&mut left, &mut right),
        Outcome::Processed
    );
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
    assert_eq!(
        plugin.run_for_test(&mut left, &mut right),
        Outcome::Processed
    );
    // A misbehaving plugin can overwrite the buffer descriptions it was
    // handed.  `refresh` must restore everything before the next call.
    let processor = plugin.clap_processor_for_test();
    for port in &mut processor.inputs.ports {
        port.data32 = std::ptr::null_mut();
        port.data64 = std::ptr::null_mut();
        port.constant_mask = u64::MAX;
    }
    for port in &mut processor.outputs.ports {
        port.data32 = std::ptr::null_mut();
        port.channel_count = 0;
        port.constant_mask = u64::MAX;
    }
    left.fill(0.5);
    right.fill(-0.25);
    assert_eq!(
        plugin.run_for_test(&mut left, &mut right),
        Outcome::Processed
    );
    let seen = fake::seen().expect("the plugin processed");
    assert_eq!(seen.inputs_count, 1);
    assert_eq!(seen.input_sums, [vec![16.0, -8.0]]);
    assert_eq!(seen.output_channels, [2]);
    assert!(left.iter().all(|sample| *sample == 1.0));
    assert!(right.iter().all(|sample| *sample == 2.0));
    let processor = plugin.clap_processor_for_test();
    for port in processor
        .inputs
        .ports
        .iter()
        .chain(&processor.outputs.ports)
    {
        assert!(port.data64.is_null());
        assert_eq!(port.constant_mask, 0);
    }
    assert_eq!(processor.outputs.ports[0].channel_count, 2);
}

#[test]
fn process_refreshes_every_sub_block_of_a_long_buffer() {
    fake::reset(fake::Fail::Nothing);
    fake::declare_ports(&[(2, MAIN), (2, 0)], &[(2, MAIN)]);
    let mut plugin = fake::load("fake").expect("load the fake plugin");
    // 600 frames are three calls (256, 256, 88) at the fake's block size.
    let mut left = vec![0.5f32; 600];
    let mut right = vec![-0.25f32; 600];
    assert_eq!(
        plugin.run_for_test(&mut left, &mut right),
        Outcome::Processed
    );
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
    let host = host_of(plugin.handle());
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
        assert!(!plugin.shared().reload_wanted());
        assert_eq!(
            plugin.run_for_test(&mut left, &mut right),
            Outcome::Processed
        );
        host_audio_ports_rescan(
            host,
            CLAP_AUDIO_PORTS_RESCAN_PORT_TYPE | CLAP_AUDIO_PORTS_RESCAN_IN_PLACE_PAIR,
        );
        assert!(!plugin.shared().reload_wanted());
        assert_eq!(
            plugin.run_for_test(&mut left, &mut right),
            Outcome::Processed
        );
    }
    assert!(fake::seen().is_some());
    unsafe { host_audio_ports_rescan(host, CLAP_AUDIO_PORTS_RESCAN_CHANNEL_COUNT) };
    // A real change asks for a reload: the old buffers are not used again.
    assert!(plugin.shared().reload_wanted());
    assert_eq!(plugin.run_for_test(&mut left, &mut right), Outcome::Skipped);
    assert_eq!(plugin.shared().failure(), None);
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
    let pref = PluginPref::default();
    let shared = Arc::new(PluginShared::new(0));
    let loaded = ClapInstance::load(
        &path,
        &candidate.plugin_id,
        48_000,
        512,
        &Restore::from_pref(&pref),
        &shared,
    )
    .expect("load the fixture");
    let mut plugin = LoadedPlugin::clap(
        loaded,
        shared,
        Saved {
            params: Vec::new(),
            state: None,
            stale: true,
        },
    );
    let handle = plugin.handle().clone();
    assert!(!handle.parameters().is_empty());
    if handle.has_editor() {
        let (width, height) =
            clap(&plugin, |plugin| plugin.prepare_editor()).expect("create the fixture interface");
        assert!(width > 0 && height > 0);
        clap(&plugin, ClapInstance::close_editor);
    }
    handle.set_parameter(1, 0.5).expect("set the fixture gain");
    let mut left = vec![0.25f32; 128];
    let mut right = vec![-0.25f32; 128];
    assert_eq!(
        plugin.run_for_test(&mut left, &mut right),
        Outcome::Processed
    );
    assert!(left.iter().any(|sample| (*sample - 0.125).abs() < 0.001));
}

#[test]
fn the_latency_is_read_once_the_plugin_is_active() {
    let plugin = fake_plugin_with_latency_for_test(64);
    assert_eq!(plugin.latency(), 64);
    assert_eq!(plugin.handle().latency(), 64);
    // `latency.get` came after `activate` (the fake asserts it is active).
    let events = fake::events();
    let activate = events.iter().position(|event| *event == "activate");
    let get = events.iter().position(|event| *event == "latency.get");
    assert!(activate < get, "{events:?}");
    // The load's own read is not a change to republish.
    assert!(!plugin.shared().take_latency_change());

    let plain = fake_plugin_for_test(false);
    assert_eq!(plain.latency(), 0);
    assert_eq!(fake::count("latency.get"), 0);
}

/// CLAP only lets the latency change inside `activate` or through a
/// restart, but some plugins call `latency.changed` while active; the
/// value is read again all the same.
#[test]
fn a_latency_change_while_active_is_read_again() {
    let plugin = fake_plugin_with_latency_for_test(64);
    let handle = plugin.handle().clone();
    // Nothing pending: servicing changes nothing.
    assert!(!handle.service());

    fake::set_latency(32);
    fake::tell_host_latency_changed();
    // Only a flag so far; the value is read on the main thread.
    assert_eq!(handle.latency(), 64);
    assert!(handle.service());
    assert_eq!(handle.latency(), 32);
    assert!(!handle.service());
    assert_eq!(fake::count("deactivate"), 0, "no restart was asked for");
}

/// The restart is a pause: the UI asks, the audio thread stops processing
/// on its next block, the UI deactivates, reactivates, reads the latency
/// and resumes, and the next block starts processing again.
#[test]
fn a_restart_stops_on_the_audio_thread_then_reactivates_on_the_main_thread() {
    let mut plugin = fake_plugin_with_latency_for_test(64);
    let handle = plugin.handle().clone();
    let mut left = vec![0.5f32; 16];
    let mut right = vec![0.5f32; 16];
    assert_eq!(
        plugin.run_for_test(&mut left, &mut right),
        Outcome::Processed
    );
    assert!(fake::seen().is_some());
    assert_eq!(fake::count("start_processing"), 1);

    // A mode switch: the new latency applies at the next activation.
    fake::set_next_latency(128);
    fake::request_restart();

    // Still processing, so the main thread asks and waits for the audio
    // thread.
    assert!(!handle.service());
    assert_eq!(fake::count("deactivate"), 0);
    assert_eq!(handle.latency(), 64);

    // The audio thread stops processing and passes the block through.
    left.fill(0.5);
    assert_eq!(plugin.run_for_test(&mut left, &mut right), Outcome::Skipped);
    assert!(fake::seen().is_none(), "the plugin was not processed");
    assert!(left.iter().all(|sample| *sample == 0.5));
    assert_eq!(fake::count("stop_processing"), 1);

    // The main thread restarts it, reads the new latency and republishes.
    assert!(handle.service());
    assert_eq!(handle.latency(), 128);
    assert_eq!(fake::count("deactivate"), 1);
    assert_eq!(fake::count("activate"), 2);
    assert!(!plugin.shared().restart_pending());
    let events = fake::events();
    let stop = events.iter().position(|event| *event == "stop_processing");
    let deactivate = events.iter().position(|event| *event == "deactivate");
    let get = events.iter().rposition(|event| *event == "latency.get");
    let activate = events.iter().rposition(|event| *event == "activate");
    assert!(stop < deactivate && activate < get, "{events:?}");

    // Processing starts again.
    assert_eq!(
        plugin.run_for_test(&mut left, &mut right),
        Outcome::Processed
    );
    assert!(fake::seen().is_some());
    assert_eq!(fake::count("start_processing"), 2);
}

#[test]
fn a_restart_before_the_first_block_happens_right_away() {
    let plugin = fake_plugin_with_latency_for_test(16);
    let handle = plugin.handle().clone();
    fake::set_next_latency(48);
    fake::request_restart();
    assert!(handle.service());
    assert_eq!(handle.latency(), 48);
    assert_eq!(fake::count("stop_processing"), 0);
    assert_eq!(fake::count("deactivate"), 1);
    // Resumed: the next block starts processing.
    assert_eq!(fake::count("start_processing"), 0);
    drop(handle);
    drop(plugin);
}

#[test]
fn a_restart_runs_on_the_main_thread_once_the_stream_has_stopped() {
    let mut plugin = fake_plugin_with_latency_for_test(64);
    let handle = plugin.handle().clone();
    let mut left = vec![0.5f32; 16];
    let mut right = vec![0.5f32; 16];
    // Started, then the stream stops: no audio callback will stop it.
    assert_eq!(
        plugin.run_for_test(&mut left, &mut right),
        Outcome::Processed
    );
    fake::set_next_latency(256);
    fake::request_restart();
    assert!(!handle.service());
    assert_eq!(fake::count("deactivate"), 0);
    std::thread::sleep(crate::audio::plugins::PAUSE_STALL + std::time::Duration::from_millis(50));
    assert!(handle.service());
    assert_eq!(handle.latency(), 256);
    // Stopped (as the audio thread, which the fake checks) before the
    // deactivation.
    assert_eq!(fake::count("stop_processing"), 1);
    assert_eq!(fake::count("deactivate"), 1);
    assert!(!plugin.shared().restart_pending());
    // The next block starts it again.
    assert_eq!(
        plugin.run_for_test(&mut left, &mut right),
        Outcome::Processed
    );
    assert_eq!(fake::count("start_processing"), 2);
}

#[test]
fn a_huge_latency_is_clamped() {
    let plugin = fake_plugin_with_latency_for_test(u32::MAX);
    assert_eq!(plugin.latency(), crate::audio::plugins::MAX_ALIGN);
}

#[test]
fn a_parameter_set_reaches_process_as_an_input_event() {
    let mut plugin = fake_plugin_with_params_for_test();
    let handle = plugin.handle().clone();
    let (mut left, mut right) = (vec![0.0f32; 16], vec![0.0f32; 16]);
    assert_eq!(
        plugin.run_for_test(&mut left, &mut right),
        Outcome::Processed
    );
    // Processing: the value waits in the transfer for the next block.
    let display = handle.set_parameter(7, 0.25).expect("set the gain");
    assert_eq!(display.as_deref(), Some("0.500"));
    handle.set_parameter(9, 1.0).expect("set the mode");
    assert!(fake_received_for_test().is_empty());
    assert_eq!(
        plugin.run_for_test(&mut left, &mut right),
        Outcome::Processed
    );
    // Native values (gain 0 to 2, mode stepped 0 to 3), in the order set.
    assert_eq!(
        fake_received_for_test(),
        vec![(7, 0.5, "process"), (9, 3.0, "process")]
    );
    assert_eq!(fake::count("params.flush"), 0);
    // Delivered once.
    assert_eq!(
        plugin.run_for_test(&mut left, &mut right),
        Outcome::Processed
    );
    assert_eq!(fake_received_for_test().len(), 2);
}

#[test]
fn a_set_while_not_processing_is_flushed_on_the_main_thread() {
    let mut plugin = fake_plugin_with_params_for_test();
    let handle = plugin.handle().clone();
    // No block has run: the plugin is active but not processing.  The fake
    // checks that this active flush is made as its audio thread.
    handle.set_parameter(7, 0.75).expect("set the gain");
    assert_eq!(fake_received_for_test(), vec![(7, 1.5, "flush")]);
    // The plugin has it: a full read sees it, nothing is queued.
    handle.sync_full().expect("nothing queued");
    assert_eq!(handle.parameters()[0].value, 0.75);
    // The pause it took is over: the first block starts and processes,
    // with nothing left to deliver.
    let (mut left, mut right) = (vec![0.0f32; 16], vec![0.0f32; 16]);
    assert_eq!(
        plugin.run_for_test(&mut left, &mut right),
        Outcome::Processed
    );
    assert_eq!(fake::count("start_processing"), 1);
    assert_eq!(fake_received_for_test().len(), 1);
    assert_eq!(plugin.shared().pause.paused_blocks(), 0);

    // Without `params.flush` the value waits for the first block.
    fake::reset(fake::Fail::Nothing);
    fake::offer_params(false);
    fake::declare_ports(&[(2, MAIN)], &[(2, MAIN)]);
    let mut plugin = fake::load("fake").expect("load the fake plugin");
    let handle = plugin.handle().clone();
    handle.set_parameter(7, 0.75).expect("set the gain");
    assert!(fake_received_for_test().is_empty());
    assert!(handle.sync_full().is_none(), "a value is still queued");
    assert_eq!(
        plugin.run_for_test(&mut left, &mut right),
        Outcome::Processed
    );
    assert_eq!(fake_received_for_test(), vec![(7, 1.5, "process")]);
}

#[test]
fn a_skipped_effect_flushes_queued_values() {
    let mut plugin = fake_plugin_with_params_for_test();
    let handle = plugin.handle().clone();
    let (mut left, mut right) = (vec![0.0f32; 16], vec![0.0f32; 16]);
    assert_eq!(
        plugin.run_for_test(&mut left, &mut right),
        Outcome::Processed
    );
    handle.set_parameter(7, 1.0).expect("set the gain");
    // Bypassed: the block skips the plugin, but hands it the value.
    let controls = crate::audio::plugins::SlotControls::new(&PluginPref::default());
    assert_eq!(
        plugin.run(&controls, &mut left, &mut right, false),
        Outcome::Skipped
    );
    assert_eq!(fake_received_for_test(), vec![(7, 2.0, "flush")]);
    // Nothing queued: a state capture need not wait.
    assert!(handle.sync_full().is_some());
    // An empty skip does not flush again.
    plugin.run(&controls, &mut left, &mut right, false);
    assert_eq!(fake::count("params.flush"), 1);
}

#[test]
fn output_param_events_reach_the_view() {
    fake::reset(fake::Fail::Nothing);
    fake::offer_params(true);
    fake::emit_output(9, 2.0);
    fake::declare_ports(&[(2, MAIN)], &[(2, MAIN)]);
    let mut plugin = fake::load("fake").expect("load the fake plugin");
    let handle = plugin.handle().clone();
    assert_eq!(handle.parameters()[1].value, 0.0);
    let (mut left, mut right) = (vec![0.0f32; 16], vec![0.0f32; 16]);
    assert_eq!(
        plugin.run_for_test(&mut left, &mut right),
        Outcome::Processed
    );
    // The UI drains it on its next service and asks for a republish.
    assert!(handle.service());
    let mode = &handle.parameters()[1];
    assert_eq!(mode.id, 9);
    assert!((mode.value - 2.0 / 3.0).abs() < 1e-9, "{}", mode.value);
    assert_eq!(mode.display, "2.000");
    // View only: the plugin's own report is not a saved setting.
    assert!(handle.saved().0.is_empty());
    assert!(!handle.service());
}

#[test]
fn a_failed_start_pauses_instead_of_processing() {
    fake::reset(fake::Fail::StartProcessing);
    fake::declare_ports(&[(2, MAIN)], &[(2, MAIN)]);
    let mut plugin = fake::load("fake").expect("load the fake plugin");
    let (mut left, mut right) = (vec![0.5f32; 16], vec![0.5f32; 16]);
    assert_eq!(plugin.run_for_test(&mut left, &mut right), Outcome::Failed);
    assert!(fake::seen().is_none(), "process was not called");
    assert_eq!(
        plugin.shared().failure(),
        Some(crate::audio::plugins::ProcessFailure::Clap(
            ClapFailure::StartFailed
        ))
    );
    assert!(plugin.shared().pause.is_paused());
    assert!(!plugin.shared().pause.processing());
    // Later blocks skip: no second start, no process.
    assert_eq!(plugin.run_for_test(&mut left, &mut right), Outcome::Skipped);
    assert_eq!(fake::count("start_processing"), 1);
    assert!(fake::seen().is_none());
    assert!(left.iter().all(|sample| *sample == 0.5));
    // Teardown does not stop a plugin that never started.
    drop(plugin);
    assert_eq!(fake::count("stop_processing"), 0);
    assert_eq!(fake::count("deactivate"), 1);
}

/// A restart whose reactivation fails leaves the plugin inactive, failed
/// and paused for good: neither a knob move nor a flush the plugin asks
/// for resumes it, and the audio side never starts it again.
#[test]
fn a_failed_reactivation_stays_paused_through_later_controls() {
    let mut plugin = fake_plugin_with_params_for_test();
    let handle = plugin.handle().clone();
    // Not processing yet: the restart happens at once, and fails.
    fake::request_restart();
    fake::fail_from_now(fake::Fail::Activate);
    assert!(handle.service());
    assert_eq!(
        plugin.shared().failure(),
        Some(crate::audio::plugins::ProcessFailure::Clap(
            ClapFailure::NotActive
        ))
    );
    assert!(plugin.shared().pause.is_paused());
    // A knob move reaches the inactive plugin through a main-thread flush
    // (the fake checks it is not made as the audio thread) and resumes
    // nothing.
    handle.set_parameter(7, 0.5).expect("set the gain");
    assert!(plugin.shared().pause.is_paused());
    assert_eq!(fake_received_for_test(), vec![(7, 1.0, "flush")]);
    fake::request_flush();
    handle.service();
    assert!(plugin.shared().pause.is_paused());
    assert_eq!(fake::count("params.flush"), 2);
    let (mut left, mut right) = (vec![0.0f32; 16], vec![0.0f32; 16]);
    assert_eq!(plugin.run_for_test(&mut left, &mut right), Outcome::Skipped);
    assert_eq!(fake::count("start_processing"), 0);
}

/// A value still queued for the audio side keeps a later one from taking
/// the direct flush: delivered in order, the newer one wins.
#[test]
fn a_direct_flush_waits_behind_values_already_queued() {
    let mut plugin = fake_plugin_with_params_for_test();
    let handle = plugin.handle().clone();
    let (mut left, mut right) = (vec![0.0f32; 16], vec![0.0f32; 16]);
    assert_eq!(
        plugin.run_for_test(&mut left, &mut right),
        Outcome::Processed
    );
    // Queued, then a restart stops the plugin before a block takes it.
    handle.set_parameter(7, 0.25).expect("set the gain");
    fake::request_restart();
    handle.service();
    assert_eq!(plugin.run_for_test(&mut left, &mut right), Outcome::Skipped);
    handle.service();
    assert!(!plugin.shared().pause.processing());
    // Not processing, but 0.25 is still queued: 0.75 queues behind it.
    handle.set_parameter(7, 0.75).expect("set the gain");
    assert_eq!(
        plugin.run_for_test(&mut left, &mut right),
        Outcome::Processed
    );
    assert_eq!(
        plugin.run_for_test(&mut left, &mut right),
        Outcome::Processed
    );
    let received = fake_received_for_test();
    assert_eq!(
        received.last().map(|value| value.1),
        Some(1.5),
        "{received:?}"
    );
    assert_eq!(fake::count("params.flush"), 0);
}

/// A flush the plugin asks for while it is not processing is made by the
/// main-thread service under a pause, as its audio thread; its output
/// reaches the views, and the plugin resumes.
#[test]
fn a_requested_flush_while_not_processing_is_made_by_the_service() {
    fake::reset(fake::Fail::Nothing);
    fake::offer_params(true);
    fake::emit_output(9, 3.0);
    fake::declare_ports(&[(2, MAIN)], &[(2, MAIN)]);
    let mut plugin = fake::load("fake").expect("load the fake plugin");
    let handle = plugin.handle().clone();
    fake::request_flush();
    assert!(handle.service());
    assert_eq!(fake::count("params.flush"), 1);
    assert_eq!(handle.parameters()[1].value, 1.0);
    // Taken: the next frame does not flush again, and the plugin starts.
    assert!(!handle.service());
    assert_eq!(fake::count("params.flush"), 1);
    let (mut left, mut right) = (vec![0.0f32; 16], vec![0.0f32; 16]);
    assert_eq!(
        plugin.run_for_test(&mut left, &mut right),
        Outcome::Processed
    );
    assert_eq!(fake::count("start_processing"), 1);
}

/// While processing, a requested flush is delivered by the next block: a
/// processed one clears it, a skipped one flushes on the audio thread.
#[test]
fn a_requested_flush_while_processing_goes_with_the_next_block() {
    let mut plugin = fake_plugin_with_params_for_test();
    let controls = crate::audio::plugins::SlotControls::new(&PluginPref::default());
    let (mut left, mut right) = (vec![0.0f32; 16], vec![0.0f32; 16]);
    assert_eq!(
        plugin.run_for_test(&mut left, &mut right),
        Outcome::Processed
    );
    // A processed block delivers it: no flush afterwards.
    fake::request_flush();
    assert_eq!(
        plugin.run_for_test(&mut left, &mut right),
        Outcome::Processed
    );
    plugin.run(&controls, &mut left, &mut right, false);
    assert_eq!(fake::count("params.flush"), 0);
    // A skipped block flushes it, once.
    fake::request_flush();
    plugin.run(&controls, &mut left, &mut right, false);
    assert_eq!(fake::count("params.flush"), 1);
    plugin.run(&controls, &mut left, &mut right, false);
    assert_eq!(fake::count("params.flush"), 1);
}

/// A rescan that keeps the controls refreshes the views; one that changes
/// an id cannot be followed by the processor's table, so the effect asks
/// for a reload and keeps the views and values of that table meanwhile.
#[test]
fn a_rescan_that_changes_the_controls_asks_for_a_reload() {
    let plugin = fake_plugin_with_params_for_test();
    let handle = plugin.handle().clone();
    fake::rescan_params(false);
    assert!(handle.service());
    assert!(!plugin.shared().reload_wanted());

    fake::rescan_params(true);
    assert!(handle.service());
    assert_eq!(plugin.shared().failure(), None);
    assert!(plugin.shared().reload_wanted());
    let ids: Vec<u32> = handle.parameters().iter().map(|view| view.id).collect();
    assert_eq!(ids, [7, 9], "the views keep the table the values use");
    assert!(handle.set_parameter(11, 0.5).is_err());
    assert!(handle.set_parameter(9, 1.0).is_ok());
}

/// The path rebuilds a fake whose controls changed: the new instance has
/// the new ids, keeps the values set before, and the new control is
/// settable.
#[test]
fn changed_controls_reload_the_plugin_with_its_new_controls() {
    let _main = crate::audio::claim_main_thread();
    fake::reset(fake::Fail::Nothing);
    fake::offer_params(true);
    fake::declare_ports(&[(2, MAIN)], &[(2, MAIN)]);
    let loads = std::rc::Rc::new(std::cell::Cell::new(0));
    let counted = std::rc::Rc::clone(&loads);
    let _loader = crate::audio::plugins::set_test_loader(move |pref, _rate| {
        counted.set(counted.get() + 1);
        let plugin = fake::load_restoring("fake", pref)?;
        Ok((plugin, false))
    });
    let path = crate::audio::path::PathControls::new(48_000);
    path.add_plugin(PluginPref {
        path: format!("{}clap", crate::audio::plugins::TEST_PATH),
        format: crate::audio::PluginFormat::Clap,
        name: "Fake".to_owned(),
        ..PluginPref::default()
    })
    .expect("load the fake");
    path.set_plugin_parameter(0, 7, 0.25).expect("set the gain");
    let old = path.plugin_handle(0).expect("loaded");
    fake::rescan_params(true);
    path.service_plugins();
    assert_eq!(loads.get(), 2, "reloaded once");
    let new = path.plugin_handle(0).expect("reloaded");
    assert_ne!(new.instance_id(), old.instance_id());
    let view = path.plugin_views()[0].clone();
    assert_eq!(view.error, None);
    let ids: Vec<u32> = view.parameters.iter().map(|view| view.id).collect();
    assert_eq!(ids, [7, 11]);
    assert!(
        (view.parameters[0].value - 0.25).abs() < 1e-9,
        "the value came over"
    );
    path.set_plugin_parameter(0, 11, 1.0)
        .expect("the new control");
    path.service_plugins();
    assert_eq!(loads.get(), 2, "no second reload");
    drop(old);
}

/// Loads the fake with this GUI and its editor API chosen from
/// `candidates`, as a load in that session would.
fn fake_with_gui(gui: fake::gui::Gui, candidates: &[EditorApi]) -> LoadedPlugin {
    fake::reset(fake::Fail::Nothing);
    fake::gui::offer(gui);
    fake::declare_ports(&[(2, MAIN)], &[(2, MAIN)]);
    let plugin = fake::load("fake").expect("load the fake plugin");
    clap(&plugin, |plugin| {
        plugin.editor_api = choose_editor_api(plugin.plugin, plugin.gui_ext, candidates);
    });
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
        clap(&plugin, |plugin| plugin.editor_api()),
        Some(EditorApi {
            api: WindowApi::X11,
            floating: false
        })
    );
    drop(plugin);

    // A plugin that only floats still has an editor.
    let plugin = fake_with_gui(gui(vec![(CLAP_WINDOW_API_WAYLAND, true)]), &x11_and_wayland);
    assert_eq!(
        clap(&plugin, |plugin| plugin.editor_api()),
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
    assert!(!clap(&plugin, |plugin| plugin.has_editor()));
}

#[test]
fn a_plugin_without_the_gui_extension_has_no_editor() {
    let plugin = fake_plugin_for_test(false);
    let candidates = linux_candidates(true, true);
    assert_eq!(
        clap(&plugin, |plugin| choose_editor_api(
            plugin.plugin,
            plugin.gui_ext,
            &candidates
        )),
        None
    );
}

#[test]
fn a_floating_editor_is_created_titled_shown_and_destroyed() {
    let plugin = fake_with_gui(
        fake::gui::Gui {
            supported: vec![(CLAP_WINDOW_API_WAYLAND, true)],
            ..Default::default()
        },
        &linux_candidates(false, true),
    );
    clap(&plugin, |plugin| {
        assert_eq!(plugin.prepare_editor(), Ok((400, 300)));
        plugin
            .show_floating_editor("Fake - CLAP", 2.0)
            .expect("show the floating editor");
        // A floating editor cannot take a parent.
        assert!(plugin.attach_editor(ParentWindow::X11(1), 1.0).is_err());
        plugin.close_editor();
    });
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
    let plugin = fake_with_gui(
        fake::gui::Gui {
            supported: vec![(CLAP_WINDOW_API_X11, false)],
            ..Default::default()
        },
        &linux_candidates(true, false),
    );
    clap(&plugin, |plugin| {
        plugin.prepare_editor().expect("create the editor");
        assert_eq!(plugin.editor_resize_hints(), ResizeHints::FIXED);
        assert_eq!(plugin.resize_editor(900, 700), (400, 300));
    });
    assert!(!gui_events().contains(&"gui.set_size"));
}

#[test]
fn a_user_resize_goes_through_adjust_size_first() {
    let plugin = fake_with_gui(
        fake::gui::Gui {
            supported: vec![(CLAP_WINDOW_API_X11, false)],
            resizable: true,
            step: 50,
            ..Default::default()
        },
        &linux_candidates(true, false),
    );
    clap(&plugin, |plugin| {
        plugin.prepare_editor().expect("create the editor");
        assert!(plugin.editor_resize_hints().resizable());
        assert_eq!(plugin.resize_editor(523, 377), (500, 350));
    });
    assert_eq!(fake::gui::size(), (500, 350));
    let events = gui_events();
    let adjust = events.iter().position(|event| *event == "gui.adjust_size");
    let set = events.iter().position(|event| *event == "gui.set_size");
    assert!(adjust.is_some() && adjust < set, "{events:?}");
}

#[test]
fn a_refused_resize_snaps_the_window_back() {
    let plugin = fake_with_gui(
        fake::gui::Gui {
            supported: vec![(CLAP_WINDOW_API_X11, false)],
            resizable: true,
            accept_size: false,
            ..Default::default()
        },
        &linux_candidates(true, false),
    );
    clap(&plugin, |plugin| {
        plugin.prepare_editor().expect("create the editor");
        // `adjust_size` refuses (step 0), so the size is passed unchanged
        // and `set_size` refuses that too.
        assert_eq!(plugin.resize_editor(523, 377), (400, 300));
    });
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
        clap(&plugin, |plugin| plugin.editor_resize_hints()),
        ResizeHints {
            horizontal: true,
            vertical: false,
            aspect: Some((16, 9)),
        }
    );
}

#[test]
fn show_hide_and_hint_requests_wait_for_the_window() {
    let shared = Arc::new(HostState::new(Arc::new(PluginShared::new(0))));
    let host = super::instance::make_host(&shared);
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

    let _main = crate::audio::claim_main_thread();
    fake::reset(fake::Fail::Nothing);
    fake::gui::offer_fd_support();
    fake::declare_ports(&[(2, MAIN)], &[(2, MAIN)]);
    let plugin = fake::load("fake").expect("load the fake plugin");
    let host = host_of(plugin.handle());
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
    clap(&plugin, ClapInstance::service_callbacks);
    assert!(fake::gui::fd_calls().is_empty());

    unsafe { assert!(modify(host, read_end, CLAP_POSIX_FD_READ)) };
    assert_eq!(
        unsafe { libc::write(write_end, b"x".as_ptr().cast(), 1) },
        1
    );
    clap(&plugin, ClapInstance::service_callbacks);
    assert_eq!(fake::gui::fd_calls(), vec![(read_end, CLAP_POSIX_FD_READ)]);

    unsafe {
        assert!(unregister(host, read_end));
        assert!(!unregister(host, read_end));
    }
    clap(&plugin, ClapInstance::service_callbacks);
    assert_eq!(fake::gui::fd_calls().len(), 1);
    drop(plugin);
    unsafe {
        libc::close(read_end);
        libc::close(write_end);
    }
}
