use std::sync::atomic::{AtomicI32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use ::vst3::Steinberg::Vst::{
    BusDirection, IAudioProcessor, IAudioProcessorTrait, IParamValueQueueTrait,
    IParameterChangesTrait, ProcessContext, ProcessData, ProcessSetup, SpeakerArrangement,
};
use ::vst3::Steinberg::{kNotImplemented, kResultFalse, kResultOk, tresult, TBool};
use ::vst3::{Class, ComRef, ComWrapper};

use super::{BusBuffers, BusLayout, Vst3Processor};
use crate::audio::plugins::{param_transfer, ParamIds, ParamReceiver, ParamSender, ProcessFailure};

/// Parameter ids of the stand-in: a gain it reads, and a meter it writes.
const GAIN: u32 = 5;
const METER: u32 = 6;

/// A Rust `IAudioProcessor` with one stereo bus each way.  It applies the
/// last gain point it was sent, reports the gain on its meter output
/// parameter, and records each block's length.
struct FakeProcessor {
    gain: AtomicU64,
    blocks: Mutex<Vec<i32>>,
    start_result: AtomicI32,
    starts: AtomicUsize,
}

impl FakeProcessor {
    fn new(start_result: tresult) -> Self {
        Self {
            gain: AtomicU64::new(1.0f64.to_bits()),
            blocks: Mutex::new(Vec::new()),
            start_result: AtomicI32::new(start_result),
            starts: AtomicUsize::new(0),
        }
    }

    fn gain(&self) -> f64 {
        f64::from_bits(self.gain.load(Ordering::SeqCst))
    }
}

impl Class for FakeProcessor {
    type Interfaces = (IAudioProcessor,);
}

impl IAudioProcessorTrait for FakeProcessor {
    unsafe fn setBusArrangements(
        &self,
        _inputs: *mut SpeakerArrangement,
        _ins: i32,
        _outputs: *mut SpeakerArrangement,
        _outs: i32,
    ) -> tresult {
        kResultOk
    }
    unsafe fn getBusArrangement(
        &self,
        _dir: BusDirection,
        _index: i32,
        _arr: *mut SpeakerArrangement,
    ) -> tresult {
        kResultFalse
    }
    unsafe fn canProcessSampleSize(&self, _size: i32) -> tresult {
        kResultOk
    }
    unsafe fn getLatencySamples(&self) -> u32 {
        0
    }
    unsafe fn setupProcessing(&self, _setup: *mut ProcessSetup) -> tresult {
        kResultOk
    }
    unsafe fn setProcessing(&self, state: TBool) -> tresult {
        if state != 0 {
            self.starts.fetch_add(1, Ordering::SeqCst);
            self.start_result.load(Ordering::SeqCst)
        } else {
            kResultOk
        }
    }
    unsafe fn process(&self, data: *mut ProcessData) -> tresult {
        // SAFETY: the host passes a live block.
        let data = unsafe { &mut *data };
        self.blocks.lock().unwrap().push(data.numSamples);
        if data.numSamples == 0 {
            // The SDK's flush: no buses at all.
            assert_eq!((data.numInputs, data.numOutputs), (0, 0));
            assert!(data.inputs.is_null() && data.outputs.is_null());
        }
        // SAFETY: the host's change lists and buses are live for the call.
        unsafe {
            let input = ComRef::from_raw(data.inputParameterChanges).unwrap();
            for at in 0..input.getParameterCount() {
                let queue = ComRef::from_raw(input.getParameterData(at)).unwrap();
                let points = queue.getPointCount();
                if queue.getParameterId() == GAIN && points > 0 {
                    let (mut offset, mut value) = (0, 0.0f64);
                    queue.getPoint(points - 1, &mut offset, &mut value);
                    self.gain.store(value.to_bits(), Ordering::SeqCst);
                }
            }
            let gain = self.gain() as f32;
            let frames = data.numSamples as usize;
            if data.numInputs > 0 {
                let input = &*data.inputs;
                let output = &*data.outputs;
                for channel in 0..2 {
                    let from = *input.__field0.channelBuffers32.add(channel);
                    let to = *output.__field0.channelBuffers32.add(channel);
                    for frame in 0..frames {
                        *to.add(frame) = *from.add(frame) * gain;
                    }
                }
            }
            let mut index = 0;
            let output = ComRef::from_raw(data.outputParameterChanges).unwrap();
            let meter = ComRef::from_raw(output.addParameterData(&METER, &mut index)).unwrap();
            meter.addPoint(0, self.gain(), &mut index);
            // An id the host does not know is accepted and dropped.
            let unknown = ComRef::from_raw(output.addParameterData(&99, &mut index)).unwrap();
            unknown.addPoint(0, 0.5, &mut index);
        }
        kResultOk
    }
    unsafe fn getTailSamples(&self) -> u32 {
        0
    }
}

struct Rig {
    fake: ComWrapper<FakeProcessor>,
    processor: Vst3Processor,
    to_audio: ParamSender,
    from_audio: ParamReceiver,
}

fn rig(start_result: tresult) -> Rig {
    let fake = ComWrapper::new(FakeProcessor::new(start_result));
    let ids = Arc::new(ParamIds::new(vec![GAIN, METER]));
    let (to_audio, from_ui) = param_transfer(ids.len());
    let (to_ui, from_audio) = param_transfer(ids.len());
    let layout = BusLayout {
        inputs: vec![2],
        outputs: vec![2],
        main_in: Some(0),
        main_out: 0,
    };
    // SAFETY: an all-zero process context is a valid value.
    let context: ProcessContext = unsafe { std::mem::zeroed() };
    let processor = Vst3Processor::new(
        fake.to_com_ptr::<IAudioProcessor>().expect("a processor"),
        &ids,
        (from_ui, to_ui),
        &layout,
        context,
        None,
    );
    Rig {
        fake,
        processor,
        to_audio,
        from_audio,
    }
}

fn reported(from_audio: &mut ParamReceiver) -> Vec<(usize, f64)> {
    let mut values = Vec::new();
    from_audio.drain(|index, value, _| values.push((index, value)));
    values
}

#[test]
fn ui_values_reach_the_block_and_output_changes_reach_the_ui_transfer() {
    let mut rig = rig(kResultOk);
    rig.to_audio.push(0, 0.5, 0);
    let mut left = vec![1.0f32; 64];
    let mut right = vec![-1.0f32; 64];
    assert_eq!(rig.processor.process(&mut left, &mut right), Ok(()));
    assert!(left.iter().all(|&sample| sample == 0.5));
    assert!(right.iter().all(|&sample| sample == -0.5));
    // The meter came back for the UI; the unknown id did not.
    assert_eq!(reported(&mut rig.from_audio), vec![(1, 0.5)]);
    // The changes are cleared after the block: the next one reports only
    // what the plugin writes again.
    assert_eq!(rig.processor.process(&mut left, &mut right), Ok(()));
    assert_eq!(reported(&mut rig.from_audio), vec![(1, 0.5)]);
    assert_eq!(*rig.fake.blocks.lock().unwrap(), vec![64, 64]);
}

#[test]
fn a_skipped_effect_flushes_queued_values_with_an_empty_block() {
    let mut rig = rig(kResultOk);
    // Nothing queued: no call.
    rig.processor.flush_if_queued();
    assert!(rig.fake.blocks.lock().unwrap().is_empty());
    rig.to_audio.push(0, 0.25, 40);
    rig.processor.flush_if_queued();
    assert_eq!(*rig.fake.blocks.lock().unwrap(), vec![0]);
    // The flush block had no buses (checked by the stand-in).
    assert_eq!(rig.fake.gain(), 0.25);
    assert!(!rig.to_audio.has_queued());
    assert_eq!(reported(&mut rig.from_audio), vec![(1, 0.25)]);
}

#[test]
fn a_refused_start_is_a_failure_and_not_implemented_is_not() {
    let mut refused = rig(kResultFalse);
    assert_eq!(
        refused.processor.start(),
        Err(ProcessFailure::Vst3StartFailed(kResultFalse))
    );
    let mut quiet = rig(kNotImplemented);
    assert_eq!(quiet.processor.start(), Ok(()));
    quiet.processor.stop();
    assert_eq!(quiet.fake.starts.load(Ordering::SeqCst), 1);
}

#[test]
fn a_mono_main_bus_gets_the_average_and_feeds_both_sides() {
    // One mono input, a silent stereo aux input, one mono output.
    let layout = BusLayout {
        inputs: vec![1, 2],
        outputs: vec![1],
        main_in: Some(0),
        main_out: 0,
    };
    let mut buses = BusBuffers::new(&layout);
    assert_eq!(buses.inputs[0].numChannels, 1);
    assert_eq!(buses.inputs[1].numChannels, 2);
    assert_eq!(buses.outputs[0].numChannels, 1);
    buses.channel(1).fill(9.0);
    buses.fill(&[1.0, 0.5, 0.0], &[0.0, 0.5, -1.0], 3);
    assert_eq!(&buses.channel(0)[..3], &[0.5, 0.5, -0.5]);
    assert!(
        buses.channel(1).iter().all(|&sample| sample == 0.0),
        "aux is silent"
    );
    // The plugin's mono output goes to both sides.
    buses.channel(3)[..3].copy_from_slice(&[0.25, -0.25, 1.0]);
    let (mut left, mut right) = (vec![0.0; 3], vec![0.0; 3]);
    buses.drain(&mut left, &mut right, 3);
    assert_eq!(left, vec![0.25, -0.25, 1.0]);
    assert_eq!(right, left);
}

/// More points for one parameter than its queue holds: the block keeps the
/// newest, sends it again with the next block, and only that block counts
/// the values as applied.
#[test]
fn a_block_whose_queue_dropped_a_point_is_not_counted_applied() {
    let mut rig = rig(kResultOk);
    for offset in 0..6 {
        rig.to_audio
            .push(0, 0.1 * f64::from(offset + 1), offset * 8);
    }
    let mut left = vec![1.0f32; 64];
    let mut right = vec![1.0f32; 64];
    assert_eq!(rig.processor.process(&mut left, &mut right), Ok(()));
    assert_eq!(rig.to_audio.applied(), 0, "a point was dropped");
    assert_eq!(rig.processor.process(&mut left, &mut right), Ok(()));
    assert_eq!(rig.to_audio.applied(), 6);
    assert!((rig.fake.gain() - 0.6).abs() < 1e-9);
}
