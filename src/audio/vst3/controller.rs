//! The main-thread half of a loaded VST3 plugin, and its load sequence.
//! [`Vst3Controller`] lives behind the plugin's UI-only lock
//! (`PluginHandle`): the component's lifetime and activation, the edit
//! controller (single component or separate, connected through the host
//! proxies), parameters, state and the editor view.  The audio-thread half
//! is `processor.rs`.
//!
//! The load follows the SDK's call sequence (`plugprovider.cpp`): create
//! and initialize the component, find or create the controller, restore
//! the state and the saved values while inactive, negotiate the buses,
//! set up processing, activate, and only then read the latency.
//! Processing is not started here: the first audio block does that.
//! Dropping the controller tears down in reverse, and the module goes
//! last.

use std::path::Path;
use std::ptr;
use std::sync::Arc;

use ::vst3::Steinberg::Vst::{
    BusInfo, IAudioProcessor, IAudioProcessorTrait, IComponent, IComponentHandler, IComponentTrait,
    IEditController, IEditControllerTrait, IProcessContextRequirements,
    IProcessContextRequirementsTrait, ParameterInfo, ParameterInfo_::ParameterFlags_,
    ProcessContext, ProcessSetup, SpeakerArr, SpeakerArrangement, String128,
};
use ::vst3::Steinberg::{kNotImplemented, kResultOk, IBStream, IPlugView, IPluginBaseTrait, TUID};
use ::vst3::{ComPtr, ComWrapper};

use crate::audio::plugins::{
    param_transfer, ParamIds, ParamOut, ParamReceiver, PluginParamView, PluginShared, Restore,
    MAX_BLOCK,
};

use super::connection::Connection;
use super::factory;
use super::handler::ComponentHandler;
use super::host::host_context;
use super::module::{self, Vst3Module};
use super::processor::{BusLayout, Vst3Processor};
use super::stream::{decode_state, encode_state, MemoryStream, StateBlob};
use super::{
    utf16_text, CONT_TIME_VALID, K_AUDIO, K_EVENT, K_INPUT, K_MAIN, K_OUTPUT, K_REALTIME,
    K_SAMPLE32, NEED_TEMPO, NEED_TIME_SIGNATURE, TEMPO_VALID, TIME_SIG_VALID,
};

/// The most audio buses, and channels per bus, a plugin may have.
const MAX_BUSES: usize = 16;
const MAX_BUS_CHANNELS: usize = 32;

/// One parameter as the controller lists it.
#[derive(Clone, Debug, PartialEq)]
struct ParamInfo {
    id: u32,
    title: String,
    units: String,
    step_count: i32,
    default: f64,
    read_only: bool,
}

impl ParamInfo {
    fn from_info(info: &ParameterInfo) -> Self {
        Self {
            id: info.id,
            title: utf16_text(&info.title),
            units: utf16_text(&info.units),
            step_count: info.stepCount.max(0),
            default: info.defaultNormalizedValue.clamp(0.0, 1.0),
            read_only: info.flags & ParameterFlags_::kIsReadOnly != 0,
        }
    }
}

/// The main-thread half.
pub(crate) struct Vst3Controller {
    /// The proxies between a separate controller and the component.
    connection: Option<Connection>,
    /// `None` without a controller (no parameters, no editor).
    controller: Option<ComPtr<IEditController>>,
    /// The component is its own controller.
    single: bool,
    /// A separate controller was initialized, so it is terminated.
    separate: bool,
    component: ComPtr<IComponent>,
    /// UI-thread calls only: setup, latency, and a forced stop.
    processor: ComPtr<IAudioProcessor>,
    handler: Option<ComWrapper<ComponentHandler>>,
    infos: Vec<ParamInfo>,
    ids: Arc<ParamIds>,
    /// The buses the processor's buffers were built for.
    layout: BusLayout,
    shared: Arc<PluginShared>,
    active: bool,
    /// The setup the processor was given, given again on every
    /// reactivation (as the SDK's hosts do between `setActive` calls).
    setup: ProcessSetup,
    /// Last: the binary stays loaded until everything above is released.
    /// `None` only for the tests' Rust stand-ins.
    _module: Option<Arc<Vst3Module>>,
}

/// What a load hands to `LoadedPlugin`: the two halves and the UI side's
/// parts of the parameter transfers.
pub(crate) struct Vst3Loaded {
    pub(crate) controller: Vst3Controller,
    pub(crate) processor: Vst3Processor,
    pub(crate) out: Arc<ParamOut>,
    pub(crate) from_audio: ParamReceiver,
    pub(crate) ids: Arc<ParamIds>,
    pub(crate) views: Vec<PluginParamView>,
    pub(crate) has_editor: bool,
    /// The saved state restored and no newer value had to go on top.
    pub(crate) state_current: bool,
    /// The ids of the restored values still queued for the processor.
    pub(crate) undelivered: Vec<u32>,
}

/// A stream holding `bytes`, as the plugin's `IBStream`.
fn stream(bytes: Vec<u8>) -> (ComWrapper<MemoryStream>, ComPtr<IBStream>) {
    let stream = ComWrapper::new(MemoryStream::new(bytes));
    let pointer = stream
        .to_com_ptr::<IBStream>()
        .expect("MemoryStream implements IBStream");
    (stream, pointer)
}

/// `kResultOk`, or a call the plugin does not implement.
fn ok_or_absent(result: i32) -> bool {
    result == kResultOk || result == kNotImplemented
}

impl Vst3Controller {
    /// Opens the bundle and loads one plugin from it at `sample_rate`,
    /// restored from `restore`.  Main thread.
    pub(crate) fn load(
        path: &Path,
        class_id: Option<&str>,
        sample_rate: u32,
        restore: &Restore<'_>,
        shared: &Arc<PluginShared>,
    ) -> Result<Vst3Loaded, String> {
        let module = module::acquire(path)?;
        let cid = factory::audio_class(module.factory(), class_id)?;
        let component = factory::create::<IComponent>(module.factory(), &cid)
            .ok_or_else(|| "The VST3 plugin could not be created.".to_owned())?;
        Self::assemble(component, Some(module), sample_rate, restore, shared)
    }

    /// The load sequence from a created component on: initialize, the
    /// controller, parameters, state and values, buses, setup, activation.
    /// `module` is `None` only for the tests' Rust stand-ins.
    pub(super) fn assemble(
        component: ComPtr<IComponent>,
        module: Option<Arc<Vst3Module>>,
        sample_rate: u32,
        restore: &Restore<'_>,
        shared: &Arc<PluginShared>,
    ) -> Result<Vst3Loaded, String> {
        // SAFETY: a new component and the static host context.
        if unsafe { component.initialize(host_context()) } != kResultOk {
            // A failed initialize is not paired with terminate.
            return Err("The VST3 plugin refused to start (initialize failed).".to_owned());
        }
        let Some(processor) = component.cast::<IAudioProcessor>() else {
            // SAFETY: initialized above and not used afterwards.
            unsafe { component.terminate() };
            return Err("The VST3 plugin is not an audio processor.".to_owned());
        };
        // From here dropping `this` tears down whatever was set up.
        let mut this = Self {
            connection: None,
            controller: None,
            single: false,
            separate: false,
            component,
            processor,
            handler: None,
            infos: Vec::new(),
            ids: Arc::new(ParamIds::new(Vec::new())),
            layout: BusLayout::default(),
            shared: Arc::clone(shared),
            active: false,
            setup: ProcessSetup {
                processMode: K_REALTIME,
                symbolicSampleSize: K_SAMPLE32,
                maxSamplesPerBlock: MAX_BLOCK as i32,
                sampleRate: 48_000.0,
            },
            _module: module.clone(),
        };
        let rate = if sample_rate == 0 {
            48_000.0
        } else {
            f64::from(sample_rate)
        };
        // SAFETY: every call below is a main-thread call on the live,
        // initialized component and controller, in the SDK's order.
        unsafe {
            if this.processor.canProcessSampleSize(K_SAMPLE32) != kResultOk {
                return Err("The VST3 plugin cannot process 32-bit samples.".to_owned());
            }
            this.open_controller(module.as_deref())?;
            // The state first, with no handler yet: a parameter list or
            // buses that depend on the state are read after it, so a
            // restart the plugin raises from `setState` has nothing left to
            // change.
            let state_loaded = this.restore_state(restore.state.as_deref());
            this.infos = this.read_infos();
            let ids = Arc::new(ParamIds::new(
                this.infos.iter().map(|info| info.id).collect(),
            ));
            this.ids = Arc::clone(&ids);
            let (to_audio, from_ui) = param_transfer(ids.len());
            let (to_ui, from_audio) = param_transfer(ids.len());
            let out = Arc::new(ParamOut::new(Arc::clone(&ids), to_audio));
            let handler =
                ComWrapper::new(ComponentHandler::new(Arc::clone(&out), Arc::clone(shared)));
            if let (Some(controller), Some(pointer)) =
                (&this.controller, handler.as_com_ref::<IComponentHandler>())
            {
                controller.setComponentHandler(pointer.as_ptr());
            }
            this.handler = Some(handler);

            let values = restore.params(state_loaded);
            let undelivered = this.restore_values(&values, &out);

            this.negotiate_buses()?;
            let context = this.process_context(rate);
            this.setup.sampleRate = rate;
            if this.processor.setupProcessing(&mut this.setup) != kResultOk {
                return Err("The VST3 plugin refused its processing setup.".to_owned());
            }
            if this.component.setActive(1) != kResultOk {
                return Err("The VST3 plugin refused to activate.".to_owned());
            }
            this.active = true;
            // Read once active: some plugins settle their buses on
            // activation.
            this.layout = this.read_layout()?;
            this.shared.set_latency(this.processor.getLatencySamples());
            // The first value goes out with the views of the new effect.
            this.shared.take_latency_change();
            // What the plugin raised while it was set up and activated is
            // done already: the buses were read after the arrangement, and
            // the latency after the activation.
            this.settle_restart_flags();

            let processor = Vst3Processor::new(
                this.processor.clone(),
                &ids,
                (from_ui, to_ui),
                &this.layout,
                context,
                module,
            );
            Ok(Vst3Loaded {
                views: this.views(),
                has_editor: this.has_editor(),
                processor,
                out,
                from_audio,
                ids,
                state_current: state_loaded && undelivered.is_empty(),
                undelivered,
                controller: this,
            })
        }
    }

    /// Clears the restart flags the host's own setup and activation made
    /// the plugin raise: `kLatencyChanged` (the latency was just read after
    /// activating), `kIoChanged` when the buses still match the
    /// processor's, `kParamTitlesChanged` when the ids still match, and
    /// `kReloadComponent` (this is the fresh load).  Some plugins raise
    /// these on every activation; acting on them would restart or reload
    /// the plugin forever.  Other flags are kept.
    fn settle_restart_flags(&mut self) {
        let flags = self.shared.take_restart();
        let mut done = super::LATENCY_CHANGED | super::RELOAD_COMPONENT;
        if flags & super::IO_CHANGED != 0 && !self.buses_changed() {
            done |= super::IO_CHANGED;
        }
        if flags & super::PARAM_TITLES_CHANGED != 0 && self.rescan() {
            done |= super::PARAM_TITLES_CHANGED;
        }
        let kept = flags & !done;
        if kept != 0 {
            self.shared.request_restart(kept);
        }
    }

    /// The buses the processor was built for.
    #[cfg(test)]
    pub(crate) fn layout(&self) -> &BusLayout {
        &self.layout
    }

    /// Whether the plugin's audio buses no longer match the layout the
    /// processor was built for (`kIoChanged`).  Main thread.
    pub(crate) fn buses_changed(&self) -> bool {
        // SAFETY: a live component and processor, on the main thread.
        unsafe { self.read_layout() }.map_or(true, |layout| layout != self.layout)
    }

    /// Finds the controller: the component itself, or the class it names,
    /// created, initialized and connected.  A plugin without one loads
    /// with no parameters and no editor.
    unsafe fn open_controller(&mut self, module: Option<&Vst3Module>) -> Result<(), String> {
        if let Some(controller) = self.component.cast::<IEditController>() {
            self.controller = Some(controller);
            self.single = true;
            return Ok(());
        }
        let mut cid: TUID = [0; 16];
        // SAFETY: a writable TUID; the caller's contract for the rest.
        if unsafe { self.component.getControllerClassId(&mut cid) } != kResultOk
            || cid.iter().all(|&byte| byte == 0)
        {
            return Ok(());
        }
        let Some(controller) =
            module.and_then(|module| factory::create::<IEditController>(module.factory(), &cid))
        else {
            return Ok(());
        };
        // SAFETY: a new controller and the static host context.
        if unsafe { controller.initialize(host_context()) } != kResultOk {
            return Err("The VST3 plugin's controller refused to start.".to_owned());
        }
        self.controller = Some(controller.clone());
        self.separate = true;
        self.connection = Connection::new(&self.component, &controller)?;
        Ok(())
    }

    /// The controller's parameter list, in its order.
    fn read_infos(&self) -> Vec<ParamInfo> {
        let Some(controller) = &self.controller else {
            return Vec::new();
        };
        // SAFETY: a live controller; `info` is writable for each call.
        unsafe {
            (0..controller.getParameterCount().max(0))
                .filter_map(|index| {
                    let mut info: ParameterInfo = std::mem::zeroed();
                    (controller.getParameterInfo(index, &mut info) == kResultOk)
                        .then(|| ParamInfo::from_info(&info))
                })
                .collect()
        }
    }

    /// Restores a saved state while inactive: the component, then a
    /// separate controller from the component state and its own.  With no
    /// state a separate controller is synced from the component, so it
    /// shows what the processor really has.  Returns whether the saved
    /// state was restored.
    unsafe fn restore_state(&mut self, bytes: Option<&[u8]>) -> bool {
        let blob = bytes.and_then(|bytes| match decode_state(bytes) {
            Ok(blob) => Some(blob),
            Err(error) => {
                eprintln!("[plugins] not restoring a VST3 plugin's state: {error}");
                None
            }
        });
        // SAFETY: the caller's contract; the streams are live for each call.
        unsafe {
            let Some(blob) = blob else {
                if let (Some(controller), false) = (&self.controller, self.single) {
                    let (written, pointer) = stream(Vec::new());
                    if ok_or_absent(self.component.getState(pointer.as_ptr())) {
                        let (_read, pointer) = stream(written.bytes());
                        controller.setComponentState(pointer.as_ptr());
                    }
                }
                return false;
            };
            let (_component, pointer) = stream(blob.component.clone());
            if !ok_or_absent(self.component.setState(pointer.as_ptr())) {
                return false;
            }
            if let Some(controller) = &self.controller {
                if !self.single {
                    let (_component, pointer) = stream(blob.component);
                    controller.setComponentState(pointer.as_ptr());
                }
                if let Some(state) = blob.controller {
                    let (_controller, pointer) = stream(state);
                    controller.setState(pointer.as_ptr());
                }
            }
            true
        }
    }

    /// Applies saved values (`None` meaning the default) to the controller
    /// and queues them for the processor's first block.  Read-only controls
    /// and ids the plugin no longer lists are skipped.  Returns the ids that
    /// were applied (queued, not yet delivered to the processor).
    unsafe fn restore_values(&mut self, values: &[(u32, Option<f64>)], out: &ParamOut) -> Vec<u32> {
        let mut applied = Vec::new();
        for (id, value) in values {
            let Some(index) = self.ids.index_of(*id) else {
                continue;
            };
            let info = &self.infos[index];
            if info.read_only {
                continue;
            }
            let value = value.unwrap_or(info.default).clamp(0.0, 1.0);
            self.set(*id, value);
            out.push(index, value);
            applied.push(*id);
        }
        applied
    }

    /// The audio bus counts and the main bus of each direction.
    unsafe fn buses(&self) -> Result<(usize, usize, Option<usize>, usize), String> {
        // SAFETY: the caller's contract; every out pointer is writable.
        unsafe {
            let (inputs, outputs) = (
                self.bus_count(K_AUDIO, K_INPUT),
                self.bus_count(K_AUDIO, K_OUTPUT),
            );
            if outputs == 0 {
                return Err("The VST3 plugin has no audio output.".to_owned());
            }
            if inputs > MAX_BUSES || outputs > MAX_BUSES {
                return Err("The VST3 plugin has more audio buses than RigWerk hosts.".to_owned());
            }
            let main = |dir, buses: usize| {
                (0..buses)
                    .find(|&index| {
                        self.bus_info(dir, index)
                            .is_some_and(|info| info.busType == K_MAIN)
                    })
                    .unwrap_or(0)
            };
            Ok((
                inputs,
                outputs,
                (inputs > 0).then(|| main(K_INPUT, inputs)),
                main(K_OUTPUT, outputs),
            ))
        }
    }

    unsafe fn bus_count(&self, media: i32, dir: i32) -> usize {
        // SAFETY: the caller's contract.
        unsafe { self.component.getBusCount(media, dir) }.max(0) as usize
    }

    unsafe fn bus_info(&self, dir: i32, index: usize) -> Option<BusInfo> {
        // SAFETY: the caller's contract; `info` is writable.
        unsafe {
            let mut info: BusInfo = std::mem::zeroed();
            (self
                .component
                .getBusInfo(K_AUDIO, dir, index as i32, &mut info)
                == kResultOk)
                .then_some(info)
        }
    }

    unsafe fn arrangement(&self, dir: i32, index: usize) -> Option<SpeakerArrangement> {
        let mut arrangement: SpeakerArrangement = 0;
        // SAFETY: the caller's contract; `arrangement` is writable.
        (unsafe {
            self.processor
                .getBusArrangement(dir, index as i32, &mut arrangement)
        } == kResultOk)
            .then_some(arrangement)
    }

    /// Activates only the main audio buses and asks for a stereo
    /// arrangement on them where the plugin accepts one.
    unsafe fn negotiate_buses(&self) -> Result<(), String> {
        // SAFETY: the caller's contract; every out pointer is writable.
        unsafe {
            let (inputs, outputs, main_in, main_out) = self.buses()?;
            for (dir, buses, main) in [
                (K_INPUT, inputs, main_in),
                (K_OUTPUT, outputs, Some(main_out)),
            ] {
                for index in 0..buses {
                    self.component.activateBus(
                        K_AUDIO,
                        dir,
                        index as i32,
                        u8::from(main == Some(index)),
                    );
                }
                for index in 0..self.bus_count(K_EVENT, dir) {
                    self.component.activateBus(K_EVENT, dir, index as i32, 0);
                }
            }
            let wanted = |dir, buses: usize, main: Option<usize>| -> Vec<SpeakerArrangement> {
                (0..buses)
                    .map(|index| {
                        if main == Some(index) {
                            SpeakerArr::kStereo
                        } else {
                            self.arrangement(dir, index).unwrap_or(SpeakerArr::kStereo)
                        }
                    })
                    .collect()
            };
            let mut ins = wanted(K_INPUT, inputs, main_in);
            let mut outs = wanted(K_OUTPUT, outputs, Some(main_out));
            // A plugin may refuse and keep its own arrangement; the
            // channel counts are read back either way.
            self.processor.setBusArrangements(
                ins.as_mut_ptr(),
                ins.len() as i32,
                outs.as_mut_ptr(),
                outs.len() as i32,
            );
            Ok(())
        }
    }

    /// Every audio bus's channel count as the plugin has it now (its
    /// arrangement, else its bus info), and the main buses.
    unsafe fn read_layout(&self) -> Result<BusLayout, String> {
        // SAFETY: the caller's contract.
        unsafe {
            let (inputs, outputs, main_in, main_out) = self.buses()?;
            let channels = |dir, buses: usize| -> Result<Vec<usize>, String> {
                (0..buses)
                    .map(|index| {
                        let channels = self
                            .arrangement(dir, index)
                            .map(|arrangement| arrangement.count_ones() as usize)
                            .or_else(|| {
                                self.bus_info(dir, index)
                                    .map(|info| info.channelCount.max(0) as usize)
                            })
                            .unwrap_or(0);
                        if channels > MAX_BUS_CHANNELS {
                            Err("A VST3 bus has more channels than RigWerk hosts.".to_owned())
                        } else {
                            Ok(channels)
                        }
                    })
                    .collect()
            };
            let layout = BusLayout {
                inputs: channels(K_INPUT, inputs)?,
                outputs: channels(K_OUTPUT, outputs)?,
                main_in,
                main_out,
            };
            let usable = |channels: usize| (1..=2).contains(&channels);
            if !usable(layout.outputs[main_out])
                || main_in.is_some_and(|main| !usable(layout.inputs[main]))
            {
                return Err("The VST3 plugin has no mono or stereo main bus.".to_owned());
            }
            Ok(layout)
        }
    }

    /// The process context the processor starts with: the rate and the
    /// continuous time, plus a tempo and time signature if the plugin asks.
    unsafe fn process_context(&self, rate: f64) -> ProcessContext {
        // SAFETY: an all-zero context is a valid value; the caller's
        // contract for the requirements query.
        let mut context: ProcessContext = unsafe { std::mem::zeroed() };
        context.sampleRate = rate;
        context.state = CONT_TIME_VALID;
        let needs = self
            .component
            .cast::<IProcessContextRequirements>()
            // SAFETY: as above.
            .map_or(0, |requirements| unsafe {
                requirements.getProcessContextRequirements()
            });
        if needs & NEED_TEMPO != 0 {
            context.tempo = 120.0;
            context.state |= TEMPO_VALID;
        }
        if needs & NEED_TIME_SIGNATURE != 0 {
            context.timeSigNumerator = 4;
            context.timeSigDenominator = 4;
            context.state |= TIME_SIG_VALID;
        }
        context
    }

    /// `setParamNormalized`.
    pub(crate) fn set(&self, id: u32, value: f64) {
        if let Some(controller) = &self.controller {
            // SAFETY: a live controller, on the main thread.
            unsafe { controller.setParamNormalized(id, value) };
        }
    }

    /// `getParamNormalized`.
    pub(crate) fn value(&self, id: u32) -> Option<f64> {
        let controller = self.controller.as_ref()?;
        // SAFETY: as in `set`.
        Some(unsafe { controller.getParamNormalized(id) })
    }

    /// The plugin's own text for a normalized value of `id`.
    pub(crate) fn format(&self, id: u32, value: f64) -> Option<String> {
        let controller = self.controller.as_ref()?;
        let mut text: String128 = [0; 128];
        // SAFETY: as in `set`; `text` is writable.
        (unsafe { controller.getParamStringByValue(id, value, &mut text) } == kResultOk)
            .then(|| utf16_text(&text))
    }

    fn view(&self, info: &ParamInfo, value: f64) -> PluginParamView {
        PluginParamView {
            id: info.id,
            name: info.title.clone(),
            value,
            default: info.default,
            step_count: info.step_count,
            unit: info.units.clone(),
            display: self
                .format(info.id, value)
                .unwrap_or_else(|| format!("{value:.2}")),
            read_only: info.read_only,
        }
    }

    /// Reads every control from the controller.
    pub(crate) fn views(&self) -> Vec<PluginParamView> {
        self.infos
            .iter()
            .map(|info| self.view(info, self.value(info.id).unwrap_or(info.default)))
            .collect()
    }

    /// The views every control has at its default.
    pub(crate) fn default_views(&self) -> Vec<PluginParamView> {
        self.infos
            .iter()
            .map(|info| self.view(info, info.default))
            .collect()
    }

    /// The writable controls' defaults, `(index, normalized)`.
    pub(crate) fn default_values(&self) -> Vec<(usize, f64)> {
        self.infos
            .iter()
            .enumerate()
            .filter(|(_, info)| !info.read_only)
            .map(|(index, info)| (index, info.default))
            .collect()
    }

    /// Whether `id` is a control the plugin does not let the host set.
    pub(crate) fn is_read_only(&self, id: u32) -> bool {
        self.ids
            .index_of(id)
            .is_some_and(|index| self.infos[index].read_only)
    }

    /// Reads the parameter list again (`kParamTitlesChanged`).  False, and
    /// the old list kept, when the ids changed: the processor and the
    /// transfers were built for that table.
    pub(crate) fn rescan(&mut self) -> bool {
        let fresh = self.read_infos();
        let same = fresh.len() == self.infos.len()
            && fresh
                .iter()
                .enumerate()
                .all(|(index, info)| self.ids.id(index) == Some(info.id));
        if same {
            self.infos = fresh;
        }
        same
    }

    /// Whether the controller can create an editor view.
    fn has_editor(&self) -> bool {
        self.create_view().is_some()
    }

    /// `createView("editor")`.
    pub(crate) fn create_view(&self) -> Option<ComPtr<IPlugView>> {
        let controller = self.controller.as_ref()?;
        // SAFETY: as in `set`; the view comes back with a reference.
        unsafe { ComPtr::from_raw(controller.createView(c"editor".as_ptr())) }
    }

    /// The component's state, and a separate controller's, in the saved
    /// envelope.  `getState` is allowed while the audio thread processes.
    pub(crate) fn save_state(&self) -> Result<Vec<u8>, String> {
        // Cleared first, so a change marked during the save is seen again.
        self.shared.take_state_dirty();
        // SAFETY: live objects; the streams are live for each call.
        let blob = unsafe {
            let (component, pointer) = stream(Vec::new());
            if self.component.getState(pointer.as_ptr()) != kResultOk {
                self.shared.mark_state_dirty();
                return Err("The VST3 plugin does not save its state.".to_owned());
            }
            // A single component's controller state is the same blob.
            let controller = match (&self.controller, self.single) {
                (Some(controller), false) => {
                    let (written, pointer) = stream(Vec::new());
                    (controller.getState(pointer.as_ptr()) == kResultOk).then(|| written.bytes())
                }
                _ => None,
            };
            StateBlob {
                component: component.bytes(),
                controller,
            }
        };
        encode_state(&blob)
    }

    /// Whether the component is active (a failed reactivation leaves it
    /// inactive for good).
    pub(crate) fn is_active(&self) -> bool {
        self.active
    }

    /// Deactivates, sets up again and reactivates the component for
    /// `kLatencyChanged`,
    /// then reads the latency, which the plugin may only change while
    /// inactive.  Main thread, with the plugin paused (no block inside,
    /// processing stopped).  False when the reactivation failed.
    pub(crate) fn reactivate(&mut self) -> bool {
        // SAFETY: a live component and processor, on the main thread.
        unsafe {
            self.component.setActive(0);
            // The same setup again while inactive: some plugins only
            // activate after a fresh one (the SDK's audiohost and validator
            // always give it).
            self.active = self.processor.setupProcessing(&mut self.setup) == kResultOk
                && self.component.setActive(1) == kResultOk;
            let latency = if self.active {
                self.processor.getLatencySamples()
            } else {
                0
            };
            self.shared.set_latency(latency);
        }
        // A `kLatencyChanged` raised inside `setActive` is answered by the
        // read above, or this would reactivate every frame.
        let flags = self.shared.take_restart() & !super::LATENCY_CHANGED;
        if flags != 0 {
            self.shared.request_restart(flags);
        }
        self.active
    }

    /// `setProcessing(false)` for a forced pause or a teardown: the caller
    /// knows no block is inside the plugin.
    pub(crate) fn stop_processing(&self) {
        // SAFETY: as in `reactivate`.
        unsafe { self.processor.setProcessing(0) };
    }

    /// `setActive(false)`, if active.  Main thread, with processing stopped.
    pub(crate) fn deactivate(&mut self) {
        if self.active {
            // SAFETY: as in `reactivate`.
            unsafe { self.component.setActive(0) };
            self.active = false;
        }
    }
}

impl Drop for Vst3Controller {
    fn drop(&mut self) {
        // The last handle: `LoadedPlugin` stopped processing, and an editor
        // window removed its view before releasing its handle.
        self.deactivate();
        drop(self.connection.take());
        // SAFETY: live objects, on the main thread, in the SDK's teardown
        // order; nothing uses them afterwards.
        unsafe {
            if let Some(controller) = &self.controller {
                controller.setComponentHandler(ptr::null_mut());
                if self.separate {
                    controller.terminate();
                }
            }
            self.component.terminate();
        }
        // The pointers, the handler and then the module go with the fields.
    }
}

#[cfg(test)]
mod tests;
