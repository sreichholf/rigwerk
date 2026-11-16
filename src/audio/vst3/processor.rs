//! The audio-thread half of a loaded VST3 plugin: `setProcessing` and
//! `process` on its `IAudioProcessor`.
//!
//! UI parameter values arrive through the transfer queue and go into the
//! block's `inputParameterChanges`; what the plugin writes into
//! `outputParameterChanges` goes back to the UI through the other queue,
//! where the controller and the views get it.  A skipped insert with
//! values queued flushes them with a `numSamples = 0` block, so a value set
//! while bypassed still reaches the processor (and its state).  Values a
//! block the plugin refused took are kept and go with the next block; only
//! values a block the plugin accepted carried count as applied (the push
//! count the UI side's saves wait for).
//!
//! Nothing here allocates, locks or frees: the bus buffers, the change
//! lists, the kept values and the process context are built at load, and a
//! failure is a `tresult`.  The processor never touches the controller or its UI lock.

use std::sync::Arc;

use ::vst3::Steinberg::Vst::{
    AudioBusBuffers, IAudioProcessor, IAudioProcessorTrait, IEventList, IParameterChanges,
    ProcessContext, ProcessData,
};
use ::vst3::Steinberg::{kNotImplemented, kResultOk};
use ::vst3::{ComPtr, ComWrapper};

use crate::audio::plugins::{
    ParamIds, ParamReceiver, ParamSender, ProcessFailure, Retry, MAX_BLOCK,
};

use super::module::Vst3Module;
use super::params::{EmptyEvents, ParameterChanges, IN_POINTS, OUT_POINTS};
use super::{K_REALTIME, K_SAMPLE32};

/// The audio buses a plugin was set up with: the channel count of every
/// input and output bus, and which bus is the main one.  The main input,
/// if there is one, and the main output have one or two channels.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct BusLayout {
    pub(crate) inputs: Vec<usize>,
    pub(crate) outputs: Vec<usize>,
    pub(crate) main_in: Option<usize>,
    pub(crate) main_out: usize,
}

/// Every channel of every audio bus, [`MAX_BLOCK`] samples each, and the
/// `AudioBusBuffers` that point into them.
struct BusBuffers {
    samples: Box<[f32]>,
    pointers: Box<[*mut f32]>,
    inputs: Box<[AudioBusBuffers]>,
    outputs: Box<[AudioBusBuffers]>,
    /// Per bus: its first channel's index in `pointers`, and its channels.
    in_start: Box<[(usize, usize)]>,
    out_start: Box<[(usize, usize)]>,
    main_in: Option<usize>,
    main_out: usize,
}

// SAFETY: the raw pointers only ever point into `samples` and `pointers`,
// which the buffers own, and the processor that holds them is used by one
// thread at a time (built on the UI thread, then only the slot's owner).
unsafe impl Send for BusBuffers {}

impl BusBuffers {
    fn new(layout: &BusLayout) -> Self {
        let mut next = 0;
        let mut starts = |channels: &[usize]| -> Box<[(usize, usize)]> {
            channels
                .iter()
                .map(|&count| {
                    let start = next;
                    next += count;
                    (start, count)
                })
                .collect()
        };
        let in_start = starts(&layout.inputs);
        let out_start = starts(&layout.outputs);
        // SAFETY: an all-zero `AudioBusBuffers` is a valid value (integers
        // and a null pointer); `refresh` fills them before every call.
        let empty = || unsafe { std::mem::zeroed::<AudioBusBuffers>() };
        let mut buffers = Self {
            samples: vec![0.0; next * MAX_BLOCK].into(),
            pointers: vec![std::ptr::null_mut(); next].into(),
            inputs: layout.inputs.iter().map(|_| empty()).collect(),
            outputs: layout.outputs.iter().map(|_| empty()).collect(),
            in_start,
            out_start,
            main_in: layout.main_in,
            main_out: layout.main_out,
        };
        buffers.refresh();
        buffers
    }

    /// Points the bus structs at the channels again: a plugin may have
    /// written over them.
    fn refresh(&mut self) {
        let base = self.samples.as_mut_ptr();
        for (channel, pointer) in self.pointers.iter_mut().enumerate() {
            // SAFETY: `samples` holds MAX_BLOCK samples per channel.
            *pointer = unsafe { base.add(channel * MAX_BLOCK) };
        }
        let table = self.pointers.as_mut_ptr();
        for (bus, &(start, count)) in self
            .inputs
            .iter_mut()
            .zip(&self.in_start)
            .chain(self.outputs.iter_mut().zip(&self.out_start))
        {
            bus.numChannels = count as i32;
            bus.silenceFlags = 0;
            // SAFETY: `start` is within `pointers` (or its end for a bus
            // without channels).
            bus.__field0.channelBuffers32 = unsafe { table.add(start) };
        }
    }

    fn channel(&mut self, index: usize) -> &mut [f32] {
        &mut self.samples[index * MAX_BLOCK..(index + 1) * MAX_BLOCK]
    }

    /// Fills the main input from `left`/`right` (mono: their average) and
    /// silences every other input channel and every output channel.
    fn fill(&mut self, left: &[f32], right: &[f32], count: usize) {
        self.samples.fill(0.0);
        let Some(main) = self.main_in else {
            return;
        };
        let (start, channels) = self.in_start[main];
        if channels == 1 {
            let mono = self.channel(start);
            for frame in 0..count {
                mono[frame] = 0.5 * (left[frame] + right[frame]);
            }
        } else if channels >= 2 {
            self.channel(start)[..count].copy_from_slice(&left[..count]);
            self.channel(start + 1)[..count].copy_from_slice(&right[..count]);
        }
    }

    /// Copies the main output back (mono: to both sides).
    fn drain(&mut self, left: &mut [f32], right: &mut [f32], count: usize) {
        let (start, channels) = self.out_start[self.main_out];
        if channels == 1 {
            let mono = &self.samples[start * MAX_BLOCK..start * MAX_BLOCK + count];
            left[..count].copy_from_slice(mono);
            right[..count].copy_from_slice(mono);
        } else if channels >= 2 {
            left[..count].copy_from_slice(&self.channel(start)[..count]);
            right[..count].copy_from_slice(&self.channel(start + 1)[..count]);
        }
    }
}

/// The audio-thread half.  It lives in the chain's slot; the UI only drops
/// it, on teardown, after the slot left every chain.
pub(crate) struct Vst3Processor {
    processor: ComPtr<IAudioProcessor>,
    from_ui: ParamReceiver,
    to_ui: ParamSender,
    input_changes: ComWrapper<ParameterChanges>,
    output_changes: ComWrapper<ParameterChanges>,
    events: ComWrapper<EmptyEvents>,
    buses: BusBuffers,
    context: Box<ProcessContext>,
    /// Values a block the plugin refused or failed took from the transfer;
    /// the next block carries them again.
    retry: Retry,
    /// Last: the binary stays loaded until everything above is released.
    /// `None` only for the tests' Rust stand-ins, which live in no module.
    _module: Option<Arc<Vst3Module>>,
}

impl Vst3Processor {
    /// Builds the processor for a set-up plugin (allocates; UI thread).
    /// `context` already holds the rate and what the plugin asked for.
    pub(crate) fn new(
        processor: ComPtr<IAudioProcessor>,
        ids: &Arc<ParamIds>,
        transfers: (ParamReceiver, ParamSender),
        layout: &BusLayout,
        context: ProcessContext,
        module: Option<Arc<Vst3Module>>,
    ) -> Self {
        let (from_ui, to_ui) = transfers;
        Self {
            processor,
            from_ui,
            to_ui,
            input_changes: ComWrapper::new(ParameterChanges::new(Arc::clone(ids), IN_POINTS)),
            output_changes: ComWrapper::new(ParameterChanges::new(Arc::clone(ids), OUT_POINTS)),
            events: ComWrapper::new(EmptyEvents),
            buses: BusBuffers::new(layout),
            context: Box::new(context),
            retry: Retry::new(ids.len()),
            _module: module,
        }
    }

    /// `setProcessing(true)`.  `kNotImplemented` is fine: the call is a
    /// notice some plugins do not need.
    pub(crate) fn start(&mut self) -> Result<(), ProcessFailure> {
        // SAFETY: an active, set-up processor.
        let result = unsafe { self.processor.setProcessing(1) };
        if result == kResultOk || result == kNotImplemented {
            Ok(())
        } else {
            Err(ProcessFailure::Vst3StartFailed(result))
        }
    }

    /// `setProcessing(false)`.
    pub(crate) fn stop(&mut self) {
        // SAFETY: as in `start`.
        unsafe { self.processor.setProcessing(0) };
    }

    /// Runs one chunk of at most [`MAX_BLOCK`] frames in place.  The queued
    /// UI values go into the block's input changes; the plugin's output
    /// changes go to the UI.  Only call it between a successful
    /// [`Self::start`] and [`Self::stop`].
    pub(crate) fn process(
        &mut self,
        left: &mut [f32],
        right: &mut [f32],
    ) -> Result<(), ProcessFailure> {
        let count = left.len().min(right.len()).min(MAX_BLOCK);
        self.buses.fill(left, right, count);
        let taken = self.take_ui_values(count);
        let result = self.call(count, true);
        self.applied(taken, result);
        self.context.continousTimeSamples += count as i64;
        if result != kResultOk {
            return Err(ProcessFailure::Vst3(result));
        }
        self.buses.drain(left, right, count);
        Ok(())
    }

    /// For a processing insert the chain skips (bypass, mix at 0): hands
    /// queued UI values to a `numSamples = 0` block with no buses (the
    /// SDK's flush, `FlushParamTest`), so they reach the processor without
    /// audio.  Does nothing without new values: values an earlier refused
    /// flush kept go with the next flush that has new ones, or the next
    /// processed block, so a plugin that refuses empty blocks does not get
    /// one every chunk.
    pub(crate) fn flush_if_queued(&mut self) {
        if self.from_ui.is_empty() {
            return;
        }
        // A plugin that refuses an empty block keeps its old values: they
        // go to `retry` for the next block.
        let taken = self.take_ui_values(1);
        let result = self.call(0, false);
        self.applied(taken, result);
    }

    /// Moves the values a refused block kept, then the transfer's, into
    /// the input changes, offsets clamped to the block: a newer drained
    /// value replaces a kept one at the same offset or follows it.
    /// Returns the push count the drain covers and whether it deferred a
    /// value, for [`Self::applied`].
    fn take_ui_values(&mut self, count: usize) -> (u64, bool) {
        let Self {
            from_ui,
            retry,
            input_changes,
            ..
        } = self;
        let pushed = from_ui.pushed();
        let changes = &**input_changes;
        changes.clear();
        retry.retain(|index, value| !changes.push(index, 0, value));
        let last = count.saturating_sub(1) as i32;
        let deferred = from_ui.drain(|index, value, offset| {
            // A full queue dropped a point: the newest value for the
            // parameter goes again next block.
            if !changes.push(index, offset.clamp(0, last), value) {
                retry.put(index, value);
            }
        });
        (pushed, deferred)
    }

    /// Reports the values up to the drained push count applied, once the
    /// plugin accepted the block they went into, the drain deferred
    /// nothing and no value waits for another try.
    fn applied(&self, (pushed, deferred): (u64, bool), result: i32) {
        // `retry` is not empty after an accepted block when a full change
        // queue dropped a point.
        if result == kResultOk && !deferred && self.retry.is_empty() {
            self.from_ui.mark_applied(pushed);
        }
    }

    /// One `process` call over `count` frames of the bus buffers (none
    /// without `audio`), then the output changes to the UI.
    fn call(&mut self, count: usize, audio: bool) -> i32 {
        self.buses.refresh();
        let buses = |list: &mut [AudioBusBuffers]| {
            if audio && !list.is_empty() {
                (list.len() as i32, list.as_mut_ptr())
            } else {
                (0, std::ptr::null_mut())
            }
        };
        let (num_inputs, inputs) = buses(&mut self.buses.inputs);
        let (num_outputs, outputs) = buses(&mut self.buses.outputs);
        let changes = |list: &ComWrapper<ParameterChanges>| {
            list.as_com_ref::<IParameterChanges>()
                .map_or(std::ptr::null_mut(), |list| list.as_ptr())
        };
        let events = self
            .events
            .as_com_ref::<IEventList>()
            .map_or(std::ptr::null_mut(), |events| events.as_ptr());
        let mut data = ProcessData {
            processMode: K_REALTIME,
            symbolicSampleSize: K_SAMPLE32,
            numSamples: count as i32,
            numInputs: num_inputs,
            numOutputs: num_outputs,
            inputs,
            outputs,
            inputParameterChanges: changes(&self.input_changes),
            outputParameterChanges: changes(&self.output_changes),
            inputEvents: events,
            outputEvents: events,
            processContext: &mut *self.context,
        };
        // SAFETY: every pointer in `data` is live for the call, and the
        // plugin is processing (or this is an empty flush block).
        let result = unsafe { self.processor.process(&mut data) };
        let Self {
            input_changes,
            output_changes,
            to_ui,
            retry,
            ..
        } = self;
        if result != kResultOk {
            // Not taken: the newest value of each goes again next block.
            input_changes.for_each_last(|index, value| retry.put(index, value));
        }
        output_changes.for_each_last(|index, value| {
            to_ui.push(index, value, 0);
        });
        output_changes.clear();
        self.input_changes.clear();
        result
    }
}

#[cfg(test)]
mod tests;
