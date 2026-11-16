// A Rust stand-in for a single-component VST3 effect, loaded through the
// real load sequence (`Vst3Controller::assemble`) and run through the
// format-neutral `LoadedPlugin` and `PluginHandle`, so the host's restart,
// edit and state handling is tested without a plugin binary.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};

use ::vst3::Steinberg::Vst::{
    BusDirection, BusInfo, IAudioProcessor, IAudioProcessorTrait, IComponent, IComponentHandler,
    IComponentHandlerTrait, IComponentTrait, IEditController, IEditControllerTrait,
    IParamValueQueueTrait, IParameterChanges, IParameterChangesTrait, IoMode, MediaType, ParamID,
    ParamValue, ParameterInfo, ProcessData, ProcessSetup, RestartFlags_, RoutingInfo, SpeakerArr,
    SpeakerArrangement, String128, TChar,
};
use ::vst3::Steinberg::{char16, int16, kResultTrue, IPlugFrame, IPlugViewTrait, ViewRect};
use ::vst3::Steinberg::{
    kInvalidArgument, kResultFalse, kResultOk, tresult, FIDString, FUnknown, IBStream,
    IBStreamTrait, IPlugView, IPluginBaseTrait, TBool, TUID,
};
use ::vst3::{Class, ComPtr, ComRef, ComWrapper};

use super::Vst3Controller;
use crate::audio::plugins::{LoadedPlugin, PluginHandle, PluginPref, PluginShared, Restore, Saved};
use crate::audio::vst3::{IO_CHANGED, LATENCY_CHANGED, PARAM_VALUES_CHANGED, RELOAD_COMPONENT};

/// The stand-in's one control, a gain with default 0.5.
const GAIN: u32 = 5;

/// The stand-in: one audio bus each way, a gain control, a latency it
/// reports, and the restarts it raises on activation.  The controller's
/// value and the processor's gain are kept apart, as in a real plugin: the
/// processor only gets a value through `process`.
#[derive(Default)]
struct Fake {
    /// The controller's value, `f64` bits.
    value: AtomicU64,
    /// The processor's gain, `f64` bits; what `getState` saves.
    gain: AtomicU64,
    /// Main output channels (`0` reads as 2).
    channels: AtomicU32,
    latency: AtomicU32,
    activations: AtomicUsize,
    /// Restart flags raised from inside `setActive(true)`.
    raise_on_activate: AtomicU32,
    handler: Mutex<Option<ComPtr<IComponentHandler>>>,
    terminated: AtomicBool,
    /// The teardown calls, in order, and what the editor view saw.
    log: Arc<Mutex<Vec<&'static str>>>,
    /// Refuses `setActive(true)` unless `setupProcessing` came since the
    /// last `setActive(false)`.
    strict: AtomicBool,
    fresh_setup: AtomicBool,
    /// Extra parameter ids after [`GAIN`] (they share its value).
    extra_ids: Mutex<Vec<u32>>,
    /// Restart flags raised from `setProcessing(true)` (the audio thread).
    raise_on_start: AtomicU32,
    /// A restored state brings control 6 with it, and says so
    /// (`kParamTitlesChanged`), as a plugin whose controls depend on its
    /// state does.
    ids_from_state: AtomicBool,
    /// `process` calls with audio.
    processed: AtomicUsize,
    /// Refuses an empty flush block (`numSamples = 0`), applying nothing.
    refuse_flush: AtomicBool,
    /// Every block with audio fails.
    fail_process: AtomicBool,
    /// While set, `process` waits inside the call after it took the
    /// values, with `inside` set.
    hold: AtomicBool,
    inside: AtomicBool,
    /// Empty blocks (`numSamples = 0`) asked of it.
    flushes: AtomicUsize,
}

impl Fake {
    fn new() -> Self {
        let fake = Self::default();
        fake.value.store(0.5f64.to_bits(), SeqCst);
        fake.gain.store(0.5f64.to_bits(), SeqCst);
        fake
    }

    fn value(&self) -> f64 {
        f64::from_bits(self.value.load(SeqCst))
    }

    fn note(&self, call: &'static str) {
        self.log.lock().unwrap().push(call);
    }

    fn calls(&self) -> Vec<&'static str> {
        self.log.lock().unwrap().clone()
    }

    fn gain(&self) -> f64 {
        f64::from_bits(self.gain.load(SeqCst))
    }

    fn channels(&self) -> u32 {
        match self.channels.load(SeqCst) {
            0 => 2,
            channels => channels,
        }
    }

    /// The handler the host gave the controller.
    fn handler(&self) -> ComPtr<IComponentHandler> {
        self.handler.lock().unwrap().clone().expect("a handler")
    }
}

impl Class for Fake {
    type Interfaces = (IComponent, IAudioProcessor, IEditController);
}

impl IPluginBaseTrait for Fake {
    unsafe fn initialize(&self, _context: *mut FUnknown) -> tresult {
        kResultOk
    }
    unsafe fn terminate(&self) -> tresult {
        self.note("terminate");
        self.terminated.store(true, SeqCst);
        kResultOk
    }
}

impl IComponentTrait for Fake {
    unsafe fn getControllerClassId(&self, _cid: *mut TUID) -> tresult {
        kResultFalse
    }
    unsafe fn setIoMode(&self, _mode: IoMode) -> tresult {
        kResultOk
    }
    unsafe fn getBusCount(&self, media: MediaType, _dir: BusDirection) -> i32 {
        i32::from(media == crate::audio::vst3::K_AUDIO)
    }
    unsafe fn getBusInfo(
        &self,
        _media: MediaType,
        _dir: BusDirection,
        _index: i32,
        bus: *mut BusInfo,
    ) -> tresult {
        // SAFETY: the host passes a writable `BusInfo`.
        unsafe {
            (*bus).channelCount = self.channels() as i32;
            (*bus).busType = crate::audio::vst3::K_MAIN;
        }
        kResultOk
    }
    unsafe fn getRoutingInfo(&self, _in: *mut RoutingInfo, _out: *mut RoutingInfo) -> tresult {
        kResultFalse
    }
    unsafe fn activateBus(
        &self,
        _media: MediaType,
        _dir: BusDirection,
        _index: i32,
        _state: TBool,
    ) -> tresult {
        kResultOk
    }
    unsafe fn setActive(&self, state: TBool) -> tresult {
        if state == 0 {
            self.note("deactivate");
            self.fresh_setup.store(false, SeqCst);
        } else {
            if self.strict.load(SeqCst) && !self.fresh_setup.swap(false, SeqCst) {
                return kResultFalse;
            }
            self.note("activate");
            self.activations.fetch_add(1, SeqCst);
            let flags = self.raise_on_activate.load(SeqCst);
            if flags != 0 {
                // SAFETY: the handler the host set.
                unsafe { self.handler().restartComponent(flags as i32) };
            }
        }
        kResultOk
    }
    unsafe fn setState(&self, state: *mut IBStream) -> tresult {
        let mut bytes = [0u8; 8];
        let mut read = 0;
        // SAFETY: the host's live stream and a writable buffer.
        let stream = unsafe { ComRef::from_raw(state) }.expect("a stream");
        unsafe { stream.read(bytes.as_mut_ptr().cast(), 8, &mut read) };
        if read != 8 {
            return kResultFalse;
        }
        let gain = f64::from_le_bytes(bytes);
        if self.ids_from_state.load(SeqCst) {
            let mut extra = self.extra_ids.lock().unwrap();
            if !extra.contains(&6) {
                extra.push(6);
            }
            drop(extra);
            if let Some(handler) = self.handler.lock().unwrap().clone() {
                // SAFETY: the handler the host set.
                unsafe { handler.restartComponent(RestartFlags_::kParamTitlesChanged) };
            }
        }
        self.gain.store(gain.to_bits(), SeqCst);
        self.value.store(gain.to_bits(), SeqCst);
        kResultOk
    }
    unsafe fn getState(&self, state: *mut IBStream) -> tresult {
        let mut bytes = self.gain().to_le_bytes();
        let mut written = 0;
        // SAFETY: the host's live stream and a readable buffer.
        let stream = unsafe { ComRef::from_raw(state) }.expect("a stream");
        unsafe { stream.write(bytes.as_mut_ptr().cast(), 8, &mut written) };
        kResultOk
    }
}

impl IAudioProcessorTrait for Fake {
    unsafe fn setBusArrangements(
        &self,
        _inputs: *mut SpeakerArrangement,
        _ins: i32,
        _outputs: *mut SpeakerArrangement,
        _outs: i32,
    ) -> tresult {
        // Keeps its own arrangement.
        kResultFalse
    }
    unsafe fn getBusArrangement(
        &self,
        _dir: BusDirection,
        _index: i32,
        arrangement: *mut SpeakerArrangement,
    ) -> tresult {
        let value = if self.channels() == 1 {
            SpeakerArr::kMono
        } else {
            SpeakerArr::kStereo
        };
        // SAFETY: the host passes a writable arrangement.
        unsafe { *arrangement = value };
        kResultOk
    }
    unsafe fn canProcessSampleSize(&self, _size: i32) -> tresult {
        kResultOk
    }
    unsafe fn getLatencySamples(&self) -> u32 {
        self.latency.load(SeqCst)
    }
    unsafe fn setupProcessing(&self, _setup: *mut ProcessSetup) -> tresult {
        self.fresh_setup.store(true, SeqCst);
        kResultOk
    }
    unsafe fn setProcessing(&self, state: TBool) -> tresult {
        self.note(if state != 0 { "start" } else { "stop" });
        let flags = self.raise_on_start.load(SeqCst);
        if state != 0 && flags != 0 {
            // SAFETY: the handler the host set.
            unsafe { self.handler().restartComponent(flags as i32) };
        }
        kResultOk
    }
    unsafe fn process(&self, data: *mut ProcessData) -> tresult {
        // SAFETY: the host's live block and change list.
        unsafe {
            let data = &*data;
            if data.numSamples == 0 {
                self.flushes.fetch_add(1, SeqCst);
            }
            if data.numSamples == 0 && self.refuse_flush.load(SeqCst) {
                return kResultFalse;
            }
            if data.numSamples > 0 {
                self.processed.fetch_add(1, SeqCst);
            }
            let input: ComRef<'_, IParameterChanges> =
                ComRef::from_raw(data.inputParameterChanges).expect("changes");
            for at in 0..input.getParameterCount() {
                let queue = ComRef::from_raw(input.getParameterData(at)).expect("a queue");
                let points = queue.getPointCount();
                if points > 0 {
                    let (mut offset, mut value) = (0, 0.0f64);
                    queue.getPoint(points - 1, &mut offset, &mut value);
                    self.gain.store(value.to_bits(), SeqCst);
                }
            }
        }
        self.inside.store(true, SeqCst);
        while self.hold.load(SeqCst) {
            std::thread::yield_now();
        }
        self.inside.store(false, SeqCst);
        if data_failed(self) {
            return kResultFalse;
        }
        kResultOk
    }
    unsafe fn getTailSamples(&self) -> u32 {
        0
    }
}

impl IEditControllerTrait for Fake {
    unsafe fn setComponentState(&self, _state: *mut IBStream) -> tresult {
        kResultOk
    }
    unsafe fn setState(&self, _state: *mut IBStream) -> tresult {
        kResultOk
    }
    unsafe fn getState(&self, _state: *mut IBStream) -> tresult {
        kResultFalse
    }
    unsafe fn getParameterCount(&self) -> i32 {
        1 + self.extra_ids.lock().unwrap().len() as i32
    }
    unsafe fn getParameterInfo(&self, index: i32, info: *mut ParameterInfo) -> tresult {
        let id = match index {
            0 => GAIN,
            index => match self.extra_ids.lock().unwrap().get(index as usize - 1) {
                Some(&id) => id,
                None => return kInvalidArgument,
            },
        };
        let title = if id == GAIN { "Gain" } else { "Mix" };
        // SAFETY: the host passes a writable `ParameterInfo`.
        unsafe {
            (*info).id = id;
            for (unit, code) in (*info).title.iter_mut().zip(title.encode_utf16()) {
                *unit = code;
            }
            (*info).defaultNormalizedValue = 0.5;
        }
        kResultOk
    }
    unsafe fn getParamStringByValue(
        &self,
        _id: ParamID,
        value: ParamValue,
        text: *mut String128,
    ) -> tresult {
        let shown = format!("{:.0} %", value * 100.0);
        // SAFETY: the host passes a writable `String128`.
        unsafe {
            for (unit, code) in (*text).iter_mut().zip(shown.encode_utf16()) {
                *unit = code;
            }
        }
        kResultOk
    }
    unsafe fn getParamValueByString(
        &self,
        _id: ParamID,
        _text: *mut TChar,
        _value: *mut ParamValue,
    ) -> tresult {
        kResultFalse
    }
    unsafe fn normalizedParamToPlain(&self, _id: ParamID, value: ParamValue) -> ParamValue {
        value
    }
    unsafe fn plainParamToNormalized(&self, _id: ParamID, value: ParamValue) -> ParamValue {
        value
    }
    unsafe fn getParamNormalized(&self, _id: ParamID) -> ParamValue {
        self.value()
    }
    unsafe fn setParamNormalized(&self, _id: ParamID, value: ParamValue) -> tresult {
        self.value.store(value.to_bits(), SeqCst);
        kResultOk
    }
    unsafe fn setComponentHandler(&self, handler: *mut IComponentHandler) -> tresult {
        // SAFETY: a live handler or null; the reference is ours to keep.
        *self.handler.lock().unwrap() =
            unsafe { ComRef::from_raw(handler) }.map(|handler| handler.to_com_ptr());
        kResultOk
    }
    unsafe fn createView(&self, _name: FIDString) -> *mut IPlugView {
        ComWrapper::new(View {
            log: Arc::clone(&self.log),
        })
        .to_com_ptr::<IPlugView>()
        .map_or(std::ptr::null_mut(), ComPtr::into_raw)
    }
}

/// Whether the stand-in fails its blocks with audio.
fn data_failed(fake: &Fake) -> bool {
    fake.fail_process.load(SeqCst)
}

/// The stand-in's editor view: it attaches anywhere and logs its removal.
struct View {
    log: Arc<Mutex<Vec<&'static str>>>,
}

impl Class for View {
    type Interfaces = (IPlugView,);
}

impl IPlugViewTrait for View {
    unsafe fn isPlatformTypeSupported(&self, _type: FIDString) -> tresult {
        kResultTrue
    }
    unsafe fn attached(&self, _parent: *mut std::ffi::c_void, _type: FIDString) -> tresult {
        self.log.lock().unwrap().push("attached");
        kResultOk
    }
    unsafe fn removed(&self) -> tresult {
        self.log.lock().unwrap().push("removed");
        kResultOk
    }
    unsafe fn onWheel(&self, _distance: f32) -> tresult {
        kResultFalse
    }
    unsafe fn onKeyDown(&self, _key: char16, _code: int16, _modifiers: int16) -> tresult {
        kResultFalse
    }
    unsafe fn onKeyUp(&self, _key: char16, _code: int16, _modifiers: int16) -> tresult {
        kResultFalse
    }
    unsafe fn getSize(&self, size: *mut ViewRect) -> tresult {
        // SAFETY: the host passes a writable rect.
        unsafe {
            *size = ViewRect {
                left: 0,
                top: 0,
                right: 100,
                bottom: 100,
            };
        }
        kResultOk
    }
    unsafe fn onSize(&self, _size: *mut ViewRect) -> tresult {
        kResultOk
    }
    unsafe fn onFocus(&self, _state: TBool) -> tresult {
        kResultOk
    }
    unsafe fn setFrame(&self, _frame: *mut IPlugFrame) -> tresult {
        kResultOk
    }
    unsafe fn canResize(&self) -> tresult {
        kResultFalse
    }
    unsafe fn checkSizeConstraint(&self, _rect: *mut ViewRect) -> tresult {
        kResultOk
    }
}

/// The stand-in, loaded from `pref` the way `load_plugin` loads a binary.
fn load(fake: &ComWrapper<Fake>, pref: &PluginPref) -> LoadedPlugin {
    load_current(fake, pref, 48_000).0
}

/// [`load`], with whether the saved state still describes the instance,
/// as `load_plugin` returns it.
fn load_current(fake: &ComWrapper<Fake>, pref: &PluginPref, rate: u32) -> (LoadedPlugin, bool) {
    let component = fake.to_com_ptr::<IComponent>().expect("a component");
    let shared = Arc::new(PluginShared::new(0));
    let restore = Restore::from_pref(pref);
    let loaded = Vst3Controller::assemble(component, None, rate, &restore, &shared)
        .unwrap_or_else(|error| panic!("{error}"));
    let current = loaded.state_current;
    let plugin = LoadedPlugin::vst3(
        loaded,
        shared,
        Saved {
            params: pref.params.clone(),
            state: pref.state.clone(),
            stale: !current,
        },
    );
    (plugin, current)
}

fn new_fake() -> ComWrapper<Fake> {
    ComWrapper::new(Fake::new())
}

/// Runs `blocks` processed chunks of silence.
fn run(plugin: &mut LoadedPlugin, blocks: usize) {
    for _ in 0..blocks {
        let (mut left, mut right) = (vec![0.0f32; 64], vec![0.0f32; 64]);
        plugin.run_for_test(&mut left, &mut right);
    }
}

fn saved_value(handle: &PluginHandle) -> Option<f64> {
    let (saved, _) = handle.saved();
    saved
        .iter()
        .find(|param| param.id == GAIN)
        .map(|param| param.value)
}

#[test]
fn a_latency_change_raised_by_the_hosts_own_activation_does_not_restart_again() {
    let _main = crate::audio::claim_main_thread();
    let fake = new_fake();
    fake.raise_on_activate.store(LATENCY_CHANGED, SeqCst);
    fake.latency.store(64, SeqCst);
    let mut plugin = load(&fake, &PluginPref::default());
    let handle = plugin.handle().clone();
    // The load read the latency after activating: nothing is left to do.
    assert!(!handle.shared().restart_pending());
    assert_eq!(handle.latency(), 64);
    run(&mut plugin, 2);

    // A real change: one restart, then quiet, although the activation
    // raises the flag again every time.
    fake.latency.store(128, SeqCst);
    handle.shared().request_restart(LATENCY_CHANGED);
    for _ in 0..10 {
        handle.service();
        run(&mut plugin, 1);
    }
    assert_eq!(handle.latency(), 128);
    assert_eq!(fake.activations.load(SeqCst), 2, "one load, one restart");
    assert!(!handle.shared().restart_pending());
    assert_eq!(handle.failure(), None);
    assert!(handle.shared().pause.processing(), "processing again");
    drop(handle);
    drop(plugin);
    assert!(fake.terminated.load(SeqCst));
}

#[test]
fn an_io_change_asks_for_a_reload_only_when_its_buses_changed() {
    let _main = crate::audio::claim_main_thread();
    let fake = new_fake();
    // Raised while the host sets the plugin up: the buses still match.
    fake.raise_on_activate.store(IO_CHANGED, SeqCst);
    let mut plugin = load(&fake, &PluginPref::default());
    let handle = plugin.handle().clone();
    assert!(!handle.shared().restart_pending());
    handle.shared().request_restart(IO_CHANGED);
    handle.service();
    assert!(!handle.shared().reload_wanted(), "same buses");
    run(&mut plugin, 1);

    fake.channels.store(1, SeqCst);
    handle.shared().request_restart(IO_CHANGED);
    assert!(handle.service());
    assert!(handle.shared().reload_wanted());
    assert_eq!(handle.failure(), None, "waiting for the reload, not failed");
}

#[test]
fn a_reload_request_raised_by_the_load_itself_is_not_followed() {
    let _main = crate::audio::claim_main_thread();
    let fake = new_fake();
    fake.raise_on_activate.store(RELOAD_COMPONENT, SeqCst);
    let plugin = load(&fake, &PluginPref::default());
    let handle = plugin.handle();
    assert!(!handle.shared().restart_pending());
    handle.service();
    assert!(!handle.shared().reload_wanted());
    // Asked for later, it is.
    handle.shared().request_restart(RELOAD_COMPONENT);
    handle.service();
    assert!(handle.shared().reload_wanted());
}

#[test]
fn a_values_change_reads_the_controls_again_into_the_saved_values() {
    let _main = crate::audio::claim_main_thread();
    let fake = new_fake();
    let plugin = load(&fake, &PluginPref::default());
    let handle = plugin.handle();
    handle.capture_state();
    assert!(!handle.wants_capture());
    // The plugin changed its own value (a preset, say) and says so.
    fake.value.store(0.8f64.to_bits(), SeqCst);
    handle.shared().request_restart(PARAM_VALUES_CHANGED);
    assert!(handle.service());
    assert_eq!(saved_value(handle), Some(0.8));
    assert_eq!(handle.parameters()[0].value, 0.8);
    assert_eq!(handle.parameters()[0].display, "80 %");
    assert!(handle.wants_capture());
}

#[test]
fn an_editor_edit_is_saved_at_once_and_reaches_the_processor() {
    let _main = crate::audio::claim_main_thread();
    let fake = new_fake();
    let mut plugin = load(&fake, &PluginPref::default());
    let handle = plugin.handle().clone();
    handle.capture_state();
    assert!(!handle.wants_capture());
    // The editor moves the control: the controller has it, and tells the
    // host.
    fake.value.store(0.3f64.to_bits(), SeqCst);
    // SAFETY: the handler the host set.
    unsafe { fake.handler().performEdit(GAIN, 0.3) };
    assert!(handle.service());
    assert_eq!(saved_value(&handle), Some(0.3));
    assert_eq!(handle.parameters()[0].value, 0.3);
    assert!(handle.wants_capture());
    run(&mut plugin, 1);
    assert_eq!(fake.gain(), 0.3);
}

#[test]
fn a_capture_with_audio_stopped_after_a_set_keeps_the_value() {
    let _main = crate::audio::claim_main_thread();
    let fake = new_fake();
    fake.value.store(0.9f64.to_bits(), SeqCst);
    fake.gain.store(0.9f64.to_bits(), SeqCst);
    let plugin = load(&fake, &PluginPref::default());
    let handle = plugin.handle();
    handle.set_parameter(GAIN, 0.3).expect("set Gain");
    // No block ran: the processor still has 0.9, and the capture happens
    // anyway instead of waiting for audio that may never come.
    handle.capture_state();
    let (params, state) = handle.saved();
    let state = state.expect("a captured state");
    assert_eq!(fake.gain(), 0.9);
    // The blob is older than the value: its snapshot leaves it out, so a
    // restore applies the saved value.
    assert!(state.params.iter().all(|param| param.id != GAIN));
    assert!(handle.wants_capture(), "captured again once delivered");

    // A restore applies the saved value on top of the older blob.
    let pref = PluginPref {
        params,
        state: Some(state.clone()),
        ..PluginPref::default()
    };
    let restored = new_fake();
    let mut plugin = load(&restored, &pref);
    assert_eq!(restored.value(), 0.3);
    run(&mut plugin, 1);
    assert_eq!(restored.gain(), 0.3);
    // Set back to the default before the next save, still with no audio:
    // the default is not saved any more, and goes on top of the blob's 0.9.
    drop(state);
    handle.reset_parameters().expect("reset");
    handle.capture_state();
    let (params, state) = handle.saved();
    assert!(params.is_empty());
    let pref = PluginPref {
        params,
        state,
        ..PluginPref::default()
    };
    let reset = new_fake();
    let mut plugin = load(&reset, &pref);
    assert_eq!(reset.value(), 0.5);
    run(&mut plugin, 1);
    assert_eq!(reset.gain(), 0.5);
}

#[test]
fn an_open_editor_keeps_the_plugin_active_until_its_view_is_removed() {
    let _main = crate::audio::claim_main_thread();
    let fake = new_fake();
    let mut plugin = load(&fake, &PluginPref::default());
    run(&mut plugin, 1);
    // The editor window: its view, attached, and its handle, dropped after
    // the view (as `PluginGuiWindow` declares them).
    let handle = plugin.handle().clone();
    let mut editor = handle.open_vst3_editor().expect("an editor");
    editor
        .attach(crate::audio::clap::ParentWindow::X11(1))
        .expect("attached");
    fake.log.lock().unwrap().clear();
    // The slot goes (a session replaced, a rate reload) while the editor
    // is open: processing stops, nothing else.
    drop(plugin);
    assert_eq!(fake.calls(), vec!["stop"]);
    drop(editor);
    drop(handle);
    assert_eq!(
        fake.calls(),
        vec!["stop", "removed", "deactivate", "terminate"]
    );
}

#[test]
fn a_latency_restart_sets_the_plugin_up_again_before_it_activates() {
    let _main = crate::audio::claim_main_thread();
    let fake = new_fake();
    fake.strict.store(true, SeqCst);
    let mut plugin = load(&fake, &PluginPref::default());
    let handle = plugin.handle().clone();
    run(&mut plugin, 1);
    fake.latency.store(32, SeqCst);
    handle.shared().request_restart(LATENCY_CHANGED);
    for _ in 0..4 {
        handle.service();
        run(&mut plugin, 1);
    }
    assert_eq!(handle.failure(), None, "not left inactive");
    assert_eq!(handle.latency(), 32);
    assert_eq!(fake.activations.load(SeqCst), 2);
    assert!(handle.shared().pause.processing(), "processing again");
}

/// What the test loader builds stand-ins with, and what it built.
#[derive(Default)]
struct Reloads {
    fakes: std::cell::RefCell<Vec<ComWrapper<Fake>>>,
    channels: std::cell::Cell<u32>,
    extra_ids: std::cell::RefCell<Vec<u32>>,
    fail: std::cell::Cell<bool>,
    loads: std::cell::Cell<usize>,
    raise_on_start: std::cell::Cell<u32>,
    ids_from_state: std::cell::Cell<bool>,
}

impl Reloads {
    fn fake(&self, at: usize) -> ComWrapper<Fake> {
        self.fakes.borrow()[at].clone()
    }
}

/// A path with one stand-in insert, loaded (and reloaded) through
/// `load_plugin` with the settings in the returned [`Reloads`].
fn reloading_path() -> (
    crate::audio::path::PathControls,
    std::rc::Rc<Reloads>,
    crate::audio::plugins::TestLoaderGuard,
) {
    let reloads = std::rc::Rc::new(Reloads::default());
    let settings = std::rc::Rc::clone(&reloads);
    let guard = crate::audio::plugins::set_test_loader(move |pref, rate| {
        settings.loads.set(settings.loads.get() + 1);
        if settings.fail.get() {
            return Err("the stand-in is gone".to_owned());
        }
        let fake = new_fake();
        fake.channels.store(settings.channels.get(), SeqCst);
        *fake.extra_ids.lock().unwrap() = settings.extra_ids.borrow().clone();
        fake.raise_on_start
            .store(settings.raise_on_start.get(), SeqCst);
        fake.ids_from_state
            .store(settings.ids_from_state.get(), SeqCst);
        let loaded = load_current(&fake, pref, rate);
        settings.fakes.borrow_mut().push(fake);
        Ok(loaded)
    });
    let path = crate::audio::path::PathControls::new(48_000);
    path.add_plugin(PluginPref {
        path: format!("{}vst3", crate::audio::plugins::TEST_PATH),
        format: crate::audio::PluginFormat::Vst3,
        name: "Stand-in".to_owned(),
        ..PluginPref::default()
    })
    .expect("load the stand-in");
    (path, reloads, guard)
}

fn run_path(path: &crate::audio::path::PathControls, blocks: usize) {
    for _ in 0..blocks {
        let (mut left, mut right) = (vec![0.0f32; 64], vec![0.0f32; 64]);
        assert!(path.process_plugins(&mut left, &mut right, 64));
    }
}

/// The insert's only view, with no error.
fn view_of(path: &crate::audio::path::PathControls) -> crate::audio::PluginView {
    let view = path.plugin_views()[0].clone();
    assert_eq!(view.error, None);
    view
}

#[test]
fn a_bus_change_reloads_the_plugin_with_the_new_buses_and_its_values() {
    let _main = crate::audio::claim_main_thread();
    let (path, reloads, _loader) = reloading_path();
    path.set_plugin_parameter(0, GAIN, 0.3).expect("set Gain");
    run_path(&path, 2);
    let old = path.plugin_handle(0).expect("loaded");
    let stereo = old.vst3_layout_for_test().expect("a VST3 insert");
    assert_eq!(stereo.outputs, vec![2]);

    // The plugin goes mono and says so.
    reloads.channels.set(1);
    reloads.fake(0).channels.store(1, SeqCst);
    old.shared().request_restart(IO_CHANGED);
    path.service_plugins();
    assert_eq!(reloads.loads.get(), 2, "reloaded once");
    let new = path.plugin_handle(0).expect("reloaded");
    assert_ne!(new.instance_id(), old.instance_id());
    assert_eq!(new.vst3_layout_for_test().expect("VST3").outputs, vec![1]);
    assert_eq!(new.failure(), None);
    let view = view_of(&path);
    assert_eq!(view.instance, Some(new.instance_id()));
    assert_eq!(view.parameters[0].value, 0.3);
    assert_eq!(reloads.fake(1).value(), 0.3);
    run_path(&path, 1);
    assert_eq!(reloads.fake(1).gain(), 0.3);
    // The old instance goes with its last handle.
    drop(old);
    assert!(reloads.fake(0).terminated.load(SeqCst));
    path.service_plugins();
    assert_eq!(reloads.loads.get(), 2, "no second reload");
}

#[test]
fn a_reload_request_rebuilds_the_plugin_from_its_state() {
    let _main = crate::audio::claim_main_thread();
    let (path, reloads, _loader) = reloading_path();
    path.set_plugin_parameter(0, GAIN, 0.3).expect("set Gain");
    run_path(&path, 2);
    assert_eq!(reloads.fake(0).gain(), 0.3);
    let old = path.plugin_handle(0).expect("loaded");
    old.shared().request_restart(RELOAD_COMPONENT);
    path.service_plugins();
    assert_eq!(reloads.loads.get(), 2);
    let new = path.plugin_handle(0).expect("reloaded");
    assert_ne!(new.instance_id(), old.instance_id());
    // The component state came over.
    assert_eq!(reloads.fake(1).gain(), 0.3);
    assert!(new.saved().1.is_some(), "restored from a captured state");
    view_of(&path);
    // A second request in quick succession (the user changing the
    // layout twice) is honoured too.
    new.shared().request_restart(RELOAD_COMPONENT);
    path.service_plugins();
    assert_eq!(reloads.loads.get(), 3);
    view_of(&path);
}

#[test]
fn a_plugin_that_asks_again_right_after_its_rebuild_is_stopped() {
    let _main = crate::audio::claim_main_thread();
    let (path, reloads, _loader) = reloading_path();
    run_path(&path, 1);
    // From now on every instance asks again as its first block starts.
    reloads.raise_on_start.set(RELOAD_COMPONENT);
    path.plugin_handle(0)
        .expect("loaded")
        .shared()
        .request_restart(RELOAD_COMPONENT);
    for _ in 0..10 {
        path.service_plugins();
        run_path(&path, 1);
    }
    path.service_plugins();
    let limit = crate::audio::plugins::RELOAD_LIMIT as usize;
    assert_eq!(
        reloads.loads.get(),
        1 + limit,
        "rebuilt to the limit, then stopped"
    );
    let view = path.plugin_views()[0].clone();
    let error = view.error.expect("the insert says why");
    assert!(error.contains("keeps asking to be reloaded"), "{error}");
}

#[test]
fn a_plugin_that_asks_again_every_second_is_stopped_at_the_limit() {
    let _main = crate::audio::claim_main_thread();
    let (path, reloads, _loader) = reloading_path();
    let limit = crate::audio::plugins::RELOAD_LIMIT as usize;
    for round in 0..10 {
        if path.plugin_views()[0].error.is_some() {
            break;
        }
        let handle = path.plugin_handle(0).expect("loaded");
        handle.shared().request_restart(RELOAD_COMPONENT);
        path.service_plugins();
        // About 1.1 s pass before the plugin asks again.
        if let Some(link) = path.slot_link(0) {
            link.controls.age_reload_window_for_test(1_100);
        }
        assert!(round <= limit, "still reloading after {round} requests");
    }
    assert_eq!(reloads.loads.get(), 1 + limit);
    let error = path.plugin_views()[0].error.clone().expect("stopped");
    assert!(error.contains("keeps asking to be reloaded"), "{error}");
}

#[test]
fn a_quiet_window_lets_a_plugin_ask_again() {
    let _main = crate::audio::claim_main_thread();
    let (path, reloads, _loader) = reloading_path();
    let limit = crate::audio::plugins::RELOAD_LIMIT as usize;
    for _ in 0..limit {
        path.plugin_handle(0)
            .expect("loaded")
            .shared()
            .request_restart(RELOAD_COMPONENT);
        path.service_plugins();
    }
    // The window passes with no request.
    path.slot_link(0)
        .expect("a slot")
        .controls
        .age_reload_window_for_test(crate::audio::plugins::RELOAD_WINDOW_MS);
    path.plugin_handle(0)
        .expect("loaded")
        .shared()
        .request_restart(RELOAD_COMPONENT);
    path.service_plugins();
    assert_eq!(reloads.loads.get(), 2 + limit);
    view_of(&path);
}

#[test]
fn titles_changed_raised_by_the_activation_with_the_same_ids_is_settled() {
    let _main = crate::audio::claim_main_thread();
    let fake = new_fake();
    fake.raise_on_activate
        .store(crate::audio::vst3::PARAM_TITLES_CHANGED, SeqCst);
    let plugin = load(&fake, &PluginPref::default());
    assert!(
        !plugin.handle().shared().restart_pending(),
        "nothing left to act on"
    );
}

#[test]
fn a_parameter_list_that_depends_on_the_state_is_not_a_layout_change() {
    let _main = crate::audio::claim_main_thread();
    let (path, reloads, _loader) = reloading_path();
    path.set_plugin_parameter(0, GAIN, 0.3).expect("set Gain");
    run_path(&path, 1);
    // Every load from here on finds control 6 in the state it restores.
    reloads.ids_from_state.set(true);
    reloads.fake(0).ids_from_state.store(true, SeqCst);
    path.plugin_handle(0)
        .expect("loaded")
        .shared()
        .request_restart(RELOAD_COMPONENT);
    for _ in 0..5 {
        path.service_plugins();
        run_path(&path, 1);
    }
    assert_eq!(reloads.loads.get(), 2, "no reload loop");
    let view = view_of(&path);
    assert_eq!(
        view.parameters
            .iter()
            .map(|view| view.id)
            .collect::<Vec<_>>(),
        vec![GAIN, 6]
    );
    assert!(!path
        .plugin_handle(0)
        .expect("loaded")
        .shared()
        .reload_wanted());
}

#[test]
fn an_insert_waiting_for_its_reload_is_not_processed() {
    let _main = crate::audio::claim_main_thread();
    let (path, reloads, _loader) = reloading_path();
    run_path(&path, 1);
    let fake = reloads.fake(0);
    let processed = fake.processed.load(SeqCst);
    // The buses changed; the service frame that rebuilds has not run.
    fake.channels.store(1, SeqCst);
    let handle = path.plugin_handle(0).expect("loaded");
    handle.shared().request_restart(IO_CHANGED);
    handle.service();
    assert!(handle.shared().reload_wanted());
    let (mut left, mut right) = (vec![0.5f32; 64], vec![0.5f32; 64]);
    assert!(path.process_plugins(&mut left, &mut right, 64));
    assert_eq!(fake.processed.load(SeqCst), processed, "skipped, not run");
    // The stand-in writes no output when it runs; skipped, its input
    // comes through (declicked in from the last processed block).
    assert!(left[63] > 0.0 && right[63] > 0.0);
    path.service_plugins();
    assert_eq!(reloads.loads.get(), 2);
}

/// Two stand-ins on one path, the second bypassed and the first at half
/// mix.
fn two_slot_path() -> (
    crate::audio::path::PathControls,
    std::rc::Rc<Reloads>,
    crate::audio::plugins::TestLoaderGuard,
) {
    let (path, reloads, loader) = reloading_path();
    path.add_plugin(PluginPref {
        path: format!("{}vst3", crate::audio::plugins::TEST_PATH),
        format: crate::audio::PluginFormat::Vst3,
        name: "Second".to_owned(),
        ..PluginPref::default()
    })
    .expect("load the second stand-in");
    path.set_plugin_mix(0, 0.5);
    path.set_plugin_bypass(1, true);
    (path, reloads, loader)
}

fn instances(path: &crate::audio::path::PathControls) -> Vec<Option<usize>> {
    path.plugin_views()
        .iter()
        .map(|view| view.instance)
        .collect()
}

#[test]
fn only_the_requesting_slot_is_rebuilt_and_keeps_its_bypass_and_mix() {
    let _main = crate::audio::claim_main_thread();
    let (path, reloads, _loader) = two_slot_path();
    let before = instances(&path);
    path.plugin_handle(1)
        .expect("loaded")
        .shared()
        .request_restart(RELOAD_COMPONENT);
    path.service_plugins();
    assert_eq!(reloads.loads.get(), 3, "two loads, one rebuild");
    let after = instances(&path);
    assert_eq!(after[0], before[0], "the other slot stays");
    assert_ne!(after[1], before[1]);
    let views = path.plugin_views();
    assert!((views[0].mix - 0.5).abs() < 1e-6 && !views[0].bypass);
    assert!(views[1].bypass);
}

#[test]
fn two_requests_in_one_frame_rebuild_both_slots() {
    let _main = crate::audio::claim_main_thread();
    let (path, reloads, _loader) = two_slot_path();
    let before = instances(&path);
    for index in 0..2 {
        path.plugin_handle(index)
            .expect("loaded")
            .shared()
            .request_restart(RELOAD_COMPONENT);
    }
    path.service_plugins();
    assert_eq!(reloads.loads.get(), 4);
    let after = instances(&path);
    assert!(after[0] != before[0] && after[1] != before[1]);
    let views = path.plugin_views();
    assert!(views.iter().all(|view| view.error.is_none()));
    assert!((views[0].mix - 0.5).abs() < 1e-6 && !views[0].bypass);
    assert!(views[1].bypass && (views[1].mix - 1.0).abs() < 1e-6);
}

#[test]
fn a_reload_request_opens_no_other_slot() {
    let _main = crate::audio::claim_main_thread();
    let (path, reloads, _loader) = reloading_path();
    // A saved insert that was never opened (the bridge is stopped).
    let mut prefs = path.plugin_prefs();
    prefs.push(PluginPref {
        path: format!("{}vst3", crate::audio::plugins::TEST_PATH),
        format: crate::audio::PluginFormat::Vst3,
        name: "Restored".to_owned(),
        ..PluginPref::default()
    });
    path.restore_plugins(prefs);
    path.ensure_plugin_loaded(0);
    assert_eq!(reloads.loads.get(), 2);
    assert!(path.plugin_handle(1).is_none(), "the second is not open");
    path.plugin_handle(0)
        .expect("loaded")
        .shared()
        .request_restart(RELOAD_COMPONENT);
    path.service_plugins();
    assert_eq!(reloads.loads.get(), 3, "only the requesting slot loaded");
    assert!(path.plugin_handle(1).is_none());
}

#[test]
fn changed_parameter_ids_reload_the_plugin_with_its_new_controls() {
    let _main = crate::audio::claim_main_thread();
    let (path, reloads, _loader) = reloading_path();
    let old = path.plugin_handle(0).expect("loaded");
    assert!(
        path.set_plugin_parameter(0, 6, 0.4).is_err(),
        "no such control yet"
    );
    reloads.extra_ids.borrow_mut().push(6);
    reloads.fake(0).extra_ids.lock().unwrap().push(6);
    old.shared()
        .request_restart(crate::audio::vst3::PARAM_TITLES_CHANGED);
    path.service_plugins();
    assert_eq!(reloads.loads.get(), 2);
    let view = view_of(&path);
    assert_eq!(
        view.parameters
            .iter()
            .map(|view| view.id)
            .collect::<Vec<_>>(),
        vec![GAIN, 6]
    );
    path.set_plugin_parameter(0, 6, 0.4)
        .expect("the new control");
}

#[test]
fn a_reload_that_fails_to_load_shows_why_and_is_not_retried() {
    let _main = crate::audio::claim_main_thread();
    let (path, reloads, _loader) = reloading_path();
    let old = path.plugin_handle(0).expect("loaded");
    reloads.fail.set(true);
    old.shared().request_restart(RELOAD_COMPONENT);
    path.service_plugins();
    assert_eq!(reloads.loads.get(), 2);
    let view = path.plugin_views()[0].clone();
    let error = view.error.expect("the failure is shown");
    assert!(error.contains("asked to be reloaded"), "{error}");
    assert!(error.contains("the stand-in is gone"), "{error}");
    assert!(path.plugin_handle(0).is_none());
    for _ in 0..3 {
        path.service_plugins();
    }
    assert_eq!(reloads.loads.get(), 2, "one attempt per request");
}

/// Whether the snapshot of the insert's saved state lists `id`.
fn snapshot_lists(handle: &PluginHandle, id: u32) -> bool {
    let (_, state) = handle.saved();
    state
        .expect("a captured state")
        .params
        .iter()
        .any(|param| param.id == id)
}

#[test]
fn a_flush_the_plugin_refuses_leaves_the_value_undelivered() {
    let _main = crate::audio::claim_main_thread();
    let (path, reloads, _loader) = reloading_path();
    run_path(&path, 1);
    path.set_plugin_bypass(0, true);
    reloads.fake(0).refuse_flush.store(true, SeqCst);
    path.set_plugin_parameter(0, GAIN, 0.3).expect("set Gain");
    run_path(&path, 2);
    assert_eq!(reloads.fake(0).gain(), 0.5, "the flush was refused");
    let handle = path.plugin_handle(0).expect("loaded");
    handle.capture_state();
    assert!(!snapshot_lists(&handle, GAIN), "the blob does not hold it");
    assert!(handle.wants_capture());
    // A restore applies the saved value on top of the older blob.
    let pref = path.plugin_prefs().into_iter().next().expect("one insert");
    let restored = new_fake();
    let mut plugin = load(
        &restored,
        &PluginPref {
            bypass: false,
            ..pref
        },
    );
    run(&mut plugin, 1);
    assert_eq!(restored.gain(), 0.3);
}

#[test]
fn a_reload_after_a_refused_flush_hands_the_new_processor_the_saved_value() {
    let _main = crate::audio::claim_main_thread();
    let (path, reloads, _loader) = reloading_path();
    run_path(&path, 1);
    path.set_plugin_bypass(0, true);
    reloads.fake(0).refuse_flush.store(true, SeqCst);
    path.set_plugin_parameter(0, GAIN, 0.3).expect("set Gain");
    run_path(&path, 2);
    path.plugin_handle(0)
        .expect("loaded")
        .shared()
        .request_restart(RELOAD_COMPONENT);
    path.service_plugins();
    assert_eq!(reloads.loads.get(), 2);
    path.set_plugin_bypass(0, false);
    run_path(&path, 1);
    // What the new processor got, not what its controller shows.
    assert_eq!(reloads.fake(1).gain(), 0.3);
}

#[test]
fn a_save_while_a_block_holds_the_values_inside_process_waits_for_it() {
    let _main = crate::audio::claim_main_thread();
    let (path, reloads, _loader) = reloading_path();
    run_path(&path, 1);
    path.set_plugin_parameter(0, GAIN, 0.3).expect("set Gain");
    let fake = reloads.fake(0);
    let handle = path.plugin_handle(0).expect("loaded");
    /// Lets the held block go however the test ends.
    struct Release<'a>(&'a AtomicBool);
    impl Drop for Release<'_> {
        fn drop(&mut self) {
            self.0.store(false, SeqCst);
        }
    }
    fake.hold.store(true, SeqCst);
    std::thread::scope(|scope| {
        let release = Release(&fake.hold);
        let block = scope.spawn(|| run_path(&path, 1));
        while !fake.inside.load(SeqCst) {
            std::thread::yield_now();
        }
        // Drained, but the block has not returned: not applied yet.
        handle.capture_state();
        let listed = snapshot_lists(&handle, GAIN);
        drop(release);
        block.join().expect("the block");
        assert!(!listed, "captured as applied while still inside process");
    });
    handle.capture_state();
    assert!(
        snapshot_lists(&handle, GAIN),
        "applied once the block returned"
    );
    assert!(!handle.wants_capture());
}

#[test]
fn a_failed_block_does_not_count_its_values_applied() {
    let _main = crate::audio::claim_main_thread();
    let (path, reloads, _loader) = reloading_path();
    run_path(&path, 1);
    reloads.fake(0).fail_process.store(true, SeqCst);
    path.set_plugin_parameter(0, GAIN, 0.3).expect("set Gain");
    run_path(&path, 1);
    let handle = path.plugin_handle(0).expect("loaded");
    assert!(handle.failure().is_some());
    handle.capture_state();
    assert!(!snapshot_lists(&handle, GAIN));
}

#[test]
fn a_value_a_refused_flush_took_reaches_the_processor_with_the_next_block() {
    let _main = crate::audio::claim_main_thread();
    let (path, reloads, _loader) = reloading_path();
    run_path(&path, 1);
    path.set_plugin_bypass(0, true);
    let fake = reloads.fake(0);
    fake.refuse_flush.store(true, SeqCst);
    path.set_plugin_parameter(0, GAIN, 0.3).expect("set Gain");
    run_path(&path, 2);
    let handle = path.plugin_handle(0).expect("loaded");
    handle.capture_state();
    assert!(!snapshot_lists(&handle, GAIN), "not applied yet");
    path.set_plugin_bypass(0, false);
    run_path(&path, 2);
    path.service_plugins();
    assert_eq!(fake.gain(), 0.3, "the next processed block carried it");
    handle.capture_state();
    assert!(snapshot_lists(&handle, GAIN));
    assert!(!handle.wants_capture());
}

#[test]
fn a_newer_value_wins_over_one_a_refused_flush_kept() {
    let _main = crate::audio::claim_main_thread();
    let (path, reloads, _loader) = reloading_path();
    run_path(&path, 1);
    path.set_plugin_bypass(0, true);
    let fake = reloads.fake(0);
    fake.refuse_flush.store(true, SeqCst);
    path.set_plugin_parameter(0, GAIN, 0.3).expect("set Gain");
    run_path(&path, 1);
    // The plugin takes empty blocks again; a newer value is queued.
    fake.refuse_flush.store(false, SeqCst);
    path.set_plugin_parameter(0, GAIN, 0.6).expect("set Gain");
    run_path(&path, 1);
    assert_eq!(fake.gain(), 0.6);
    path.set_plugin_bypass(0, false);
    run_path(&path, 2);
    assert_eq!(fake.gain(), 0.6, "the kept older value did not come back");
}

#[test]
fn a_bypassed_plugin_that_refuses_empty_blocks_is_not_asked_every_chunk() {
    let _main = crate::audio::claim_main_thread();
    let (path, reloads, _loader) = two_slot_path();
    run_path(&path, 1);
    let bypassed = reloads.fake(1);
    bypassed.refuse_flush.store(true, SeqCst);
    path.set_plugin_parameter(1, GAIN, 0.3).expect("set Gain");
    // The active first insert keeps the chain running.
    run_path(&path, 50);
    assert_eq!(
        bypassed.flushes.load(SeqCst),
        1,
        "one try for the new value"
    );
    // Un-bypassed, the first block carries the kept value.
    path.set_plugin_bypass(1, false);
    run_path(&path, 1);
    assert_eq!(bypassed.gain(), 0.3);
}
