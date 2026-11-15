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
    ParamValue, ParameterInfo, ProcessData, ProcessSetup, RoutingInfo, SpeakerArr,
    SpeakerArrangement, String128, TChar,
};
use ::vst3::Steinberg::{char16, int16, kResultTrue, IPlugFrame, IPlugViewTrait, ViewRect};
use ::vst3::Steinberg::{
    kInvalidArgument, kResultFalse, kResultOk, tresult, FIDString, FUnknown, IBStream,
    IBStreamTrait, IPlugView, IPluginBaseTrait, TBool, TUID,
};
use ::vst3::{Class, ComPtr, ComRef, ComWrapper};

use super::Vst3Controller;
use crate::audio::plugins::{
    LoadedPlugin, PluginHandle, PluginPref, PluginShared, ProcessFailure, Restore, Saved,
};
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
        kResultOk
    }
    unsafe fn process(&self, data: *mut ProcessData) -> tresult {
        // SAFETY: the host's live block and change list.
        unsafe {
            let data = &*data;
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
        1
    }
    unsafe fn getParameterInfo(&self, index: i32, info: *mut ParameterInfo) -> tresult {
        if index != 0 {
            return kInvalidArgument;
        }
        // SAFETY: the host passes a writable `ParameterInfo`.
        unsafe {
            (*info).id = GAIN;
            for (unit, code) in (*info).title.iter_mut().zip("Gain".encode_utf16()) {
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
    let component = fake.to_com_ptr::<IComponent>().expect("a component");
    let shared = Arc::new(PluginShared::new(0));
    let restore = Restore::from_pref(pref);
    let loaded = Vst3Controller::assemble(component, None, 48_000, &restore, &shared)
        .unwrap_or_else(|error| panic!("{error}"));
    let current = loaded.state_current;
    LoadedPlugin::vst3(
        loaded,
        shared,
        Saved {
            params: pref.params.clone(),
            state: pref.state.clone(),
            stale: !current,
        },
    )
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
fn an_io_change_fails_the_insert_only_when_its_buses_changed() {
    let _main = crate::audio::claim_main_thread();
    let fake = new_fake();
    // Raised while the host sets the plugin up: the buses still match.
    fake.raise_on_activate.store(IO_CHANGED, SeqCst);
    let mut plugin = load(&fake, &PluginPref::default());
    let handle = plugin.handle().clone();
    assert!(!handle.shared().restart_pending());
    handle.shared().request_restart(IO_CHANGED);
    handle.service();
    assert_eq!(handle.failure(), None, "same buses");
    run(&mut plugin, 1);

    fake.channels.store(1, SeqCst);
    handle.shared().request_restart(IO_CHANGED);
    assert!(handle.service());
    assert_eq!(handle.failure(), Some(ProcessFailure::Vst3BusesChanged));
}

#[test]
fn a_reload_request_fails_the_insert_with_its_own_text() {
    let _main = crate::audio::claim_main_thread();
    let fake = new_fake();
    let plugin = load(&fake, &PluginPref::default());
    let handle = plugin.handle();
    handle.shared().request_restart(RELOAD_COMPONENT);
    assert!(handle.service());
    let failure = handle.failure().expect("a failure");
    assert_eq!(failure, ProcessFailure::Vst3Reload);
    assert!(failure.message().contains("asked to be reloaded"));
    assert_ne!(
        failure.message(),
        ProcessFailure::Vst3BusesChanged.message()
    );
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
    assert!(state
        .params
        .iter()
        .any(|param| param.id == GAIN && param.value < 0.0));
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
    // Set back to the default (no saved value) before that save: the
    // default goes on top of the blob's 0.9.
    let pref = PluginPref {
        params: Vec::new(),
        state: Some(state),
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
