//! Audio-thread half: `start_processing`, `process`, `stop_processing`,
//! and an active `params.flush` for a skipped effect.  Nothing here
//! allocates, locks, or frees, and it never touches the main-thread
//! instance or its UI lock.
//!
//! A value the event list could not take, or that a failed block took,
//! is kept for the next block.  Values count as applied only once a
//! block or flush carried them all.

use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use clap_sys::events::{
    clap_event_header, clap_event_param_value, clap_input_events, clap_output_events,
    CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_PARAM_VALUE,
};
use clap_sys::ext::params::clap_plugin_params;
use clap_sys::plugin::clap_plugin;
use clap_sys::process::{clap_process, CLAP_PROCESS_ERROR};

use crate::audio::plugins::{
    AudioThreadScope, ClapFailure, ParamIds, ParamReceiver, ParamSender, Retry,
};

use super::instance::ClapCore;
use super::ports::PortBuffers;

/// Immutable, so both halves read it without a lock.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct ClapRange {
    pub(super) id: u32,
    pub(super) min: f64,
    pub(super) max: f64,
    pub(super) stepped: bool,
}

impl ClapRange {
    pub(super) fn normalise(&self, native: f64) -> f64 {
        if self.max > self.min {
            ((native - self.min) / (self.max - self.min)).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }

    pub(super) fn denormalise(&self, value: f64) -> f64 {
        let value = value.clamp(0.0, 1.0);
        let native = self.min + value * (self.max - self.min);
        if self.stepped {
            native.round().clamp(self.min, self.max)
        } else {
            native
        }
    }
}

/// Capacity is reserved at load.  A push past it is dropped, so the
/// audio thread never grows it.
pub(super) struct EventBuffer {
    events: Vec<clap_event_param_value>,
}

impl EventBuffer {
    pub(super) fn with_capacity(capacity: usize) -> Self {
        Self {
            events: Vec::with_capacity(capacity),
        }
    }

    pub(super) fn clear(&mut self) {
        self.events.clear();
    }

    /// Adds one value change.  False, and nothing added, when full.
    pub(super) fn push_value(&mut self, id: u32, value: f64, time: u32) -> bool {
        if self.events.len() == self.events.capacity() {
            return false;
        }
        self.events.push(clap_event_param_value {
            header: clap_event_header {
                size: std::mem::size_of::<clap_event_param_value>() as u32,
                time,
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
        });
        true
    }

    pub(super) fn as_clap(&self) -> clap_input_events {
        clap_input_events {
            ctx: self as *const _ as *mut c_void,
            size: Some(input_events_size),
            get: Some(input_events_get),
        }
    }
}

unsafe extern "C" fn input_events_size(list: *const clap_input_events) -> u32 {
    let Some(list) = (unsafe { list.as_ref() }) else {
        return 0;
    };
    let Some(buffer) = (unsafe { (list.ctx as *const EventBuffer).as_ref() }) else {
        return 0;
    };
    buffer.events.len() as u32
}

unsafe extern "C" fn input_events_get(
    list: *const clap_input_events,
    index: u32,
) -> *const clap_event_header {
    let Some(list) = (unsafe { list.as_ref() }) else {
        return ptr::null();
    };
    let Some(buffer) = (unsafe { (list.ctx as *const EventBuffer).as_ref() }) else {
        return ptr::null();
    };
    buffer
        .events
        .get(index as usize)
        .map_or(ptr::null(), |event| {
            &event.header as *const clap_event_header
        })
}

/// A known `PARAM_VALUE` is normalised and emitted.  Every other event
/// is accepted and dropped.  No allocation, so the audio thread may use it.
pub(super) struct OutputSink<'a> {
    pub(super) ids: &'a ParamIds,
    pub(super) ranges: &'a [ClapRange],
    pub(super) emit: &'a mut dyn FnMut(usize, f64),
}

impl OutputSink<'_> {
    /// Valid while `self` is neither moved nor dropped.
    pub(super) fn as_clap(&mut self) -> clap_output_events {
        clap_output_events {
            ctx: self as *mut Self as *mut c_void,
            try_push: Some(output_try_push),
        }
    }
}

unsafe extern "C" fn output_try_push(
    list: *const clap_output_events,
    event: *const clap_event_header,
) -> bool {
    let (Some(list), Some(header)) = (unsafe { list.as_ref() }, unsafe { event.as_ref() }) else {
        return false;
    };
    let Some(sink) = (unsafe { (list.ctx as *mut OutputSink<'_>).as_mut() }) else {
        return false;
    };
    if header.space_id == CLAP_CORE_EVENT_SPACE_ID
        && header.type_ == CLAP_EVENT_PARAM_VALUE
        && header.size as usize >= std::mem::size_of::<clap_event_param_value>()
    {
        let value = unsafe { &*(event as *const clap_event_param_value) };
        if let Some(index) = sink.ids.index_of(value.param_id) {
            if let Some(range) = sink.ranges.get(index) {
                (sink.emit)(index, range.normalise(value.value));
            }
        }
    }
    true
}

/// As the audio thread.  A forced stop on the UI thread uses it only
/// when no block is inside the plugin.
pub(super) fn stop_processing(plugin: *const clap_plugin) {
    let _audio_thread = AudioThreadScope::enter();
    if let Some(stop) = unsafe { (*plugin).stop_processing } {
        unsafe { stop(plugin) };
    }
}

/// Lives in the chain's slot.  The UI drops it on teardown, after the
/// slot has left every chain.
pub(crate) struct ClapProcessor {
    /// The instance's pointer, owned by `core`.
    pub(super) plugin: *const clap_plugin,
    pub(super) params_ext: *const clap_plugin_params,
    pub(super) ranges: Arc<[ClapRange]>,
    pub(super) ids: Arc<ParamIds>,
    pub(super) from_ui: ParamReceiver,
    pub(super) to_ui: ParamSender,
    /// One buffer per declared audio port, sized at load.
    pub(super) inputs: PortBuffers,
    pub(super) outputs: PortBuffers,
    pub(super) events: EventBuffer,
    /// Values a block took from the transfer but the plugin did not get
    /// (a full event list, a failed block); the next block carries them.
    pub(super) retry: Retry,
    pub(super) steady_time: i64,
    pub(super) max_block: usize,
    /// Last: the instance outlives everything above that points into it.
    pub(super) core: Arc<ClapCore>,
}

// SAFETY: the processor calls the plugin only through its audio-thread
// functions (`start_processing`, `process`, `stop_processing`, an active
// `params.flush`), from whichever thread holds the slot, one at a time;
// CLAP lets the audio thread change between blocks.  The port buffers'
// raw pointers point into buffers the processor owns.
unsafe impl Send for ClapProcessor {}

impl ClapProcessor {
    /// As the audio thread.
    pub(crate) fn start(&mut self) -> Result<(), ClapFailure> {
        let _audio_thread = AudioThreadScope::enter();
        match unsafe { (*self.plugin).start_processing } {
            Some(start) if !unsafe { start(self.plugin) } => Err(ClapFailure::StartFailed),
            _ => Ok(()),
        }
    }

    pub(crate) fn stop(&mut self) {
        stop_processing(self.plugin);
    }

    /// In place, in sub-blocks of at most the activation block size.
    /// Only between a successful [`Self::start`] and [`Self::stop`].
    pub(crate) fn process(
        &mut self,
        left: &mut [f32],
        right: &mut [f32],
    ) -> Result<(), ClapFailure> {
        let host = &*self.core.host;
        let _audio_thread = AudioThreadScope::enter();
        // A processed block delivers whatever the plugin asked to flush.
        host.flush_requested.store(false, Ordering::Relaxed);
        let frames = left.len().min(right.len());
        let Self {
            plugin,
            ranges,
            ids,
            from_ui,
            to_ui,
            inputs,
            outputs,
            events,
            retry,
            steady_time,
            max_block,
            ..
        } = self;
        let plugin = *plugin;
        // Every value this push count covers is in the first sub-block's
        // events; they count as applied once every sub-block succeeded.
        let pushed = from_ui.pushed();
        let mut deferred = false;
        let mut offset = 0;
        while offset < frames {
            let count = (frames - offset).min(*max_block);
            // Auxiliary inputs and main channels past the second stay
            // silent; the plugin may have written into any of them.
            inputs.silence(count);
            let main = inputs.main_channels();
            if let [mono] = main {
                for sample in 0..count {
                    mono[sample] = 0.5 * (left[offset + sample] + right[offset + sample]);
                }
            } else {
                main[0][..count].copy_from_slice(&left[offset..offset + count]);
                main[1][..count].copy_from_slice(&right[offset..offset + count]);
            }
            outputs.silence(count);

            events.clear();
            if offset == 0 {
                let last = count.saturating_sub(1) as i32;
                deferred = fill_events(retry, from_ui, ranges, events, last);
            }
            inputs.refresh();
            outputs.refresh();
            let input = events.as_clap();
            let mut emit = |index: usize, value: f64| {
                to_ui.push(index, value, 0);
            };
            let mut sink = OutputSink {
                ids,
                ranges,
                emit: &mut emit,
            };
            let output = sink.as_clap();
            // One buffer per declared port, in the plugin's order, so the
            // counts match what it reported through `clap_plugin_audio_ports`.
            let process = clap_process {
                steady_time: *steady_time,
                frames_count: count as u32,
                transport: ptr::null(),
                audio_inputs: inputs.ports.as_ptr(),
                audio_outputs: outputs.ports.as_mut_ptr(),
                audio_inputs_count: inputs.ports.len() as u32,
                audio_outputs_count: outputs.ports.len() as u32,
                in_events: &input,
                out_events: &output,
            };
            let status = match unsafe { (*plugin).process } {
                Some(process_fn) => unsafe { process_fn(plugin, &process) },
                None => CLAP_PROCESS_ERROR,
            };
            *steady_time = steady_time.saturating_add(count as i64);
            if status == CLAP_PROCESS_ERROR {
                // The effect fails for good: nothing is applied, and
                // nothing is kept for a block that will not come.
                return Err(ClapFailure::ProcessError);
            }

            let main = outputs.main_channels();
            if let [mono] = main {
                left[offset..offset + count].copy_from_slice(&mono[..count]);
                right[offset..offset + count].copy_from_slice(&mono[..count]);
            } else {
                left[offset..offset + count].copy_from_slice(&main[0][..count]);
                right[offset..offset + count].copy_from_slice(&main[1][..count]);
            }
            offset += count;
        }
        if !deferred && retry.is_empty() {
            from_ui.mark_applied(pushed);
        }
        Ok(())
    }

    /// Bypass or mix at 0.  Queued values, or a flush the plugin asked
    /// for, go to `params.flush` on the audio thread.  Without `flush`
    /// they stay queued for the next processed block.
    pub(crate) fn flush_if_queued(&mut self) {
        let host = &*self.core.host;
        let requested = host.flush_requested.load(Ordering::Relaxed);
        if !requested && self.from_ui.is_empty() && self.retry.is_empty() {
            return;
        }
        let Some(flush) = (unsafe { self.params_ext.as_ref() }).and_then(|params| params.flush)
        else {
            return;
        };
        host.flush_requested.store(false, Ordering::Relaxed);
        let _audio_thread = AudioThreadScope::enter();
        let Self {
            plugin,
            ranges,
            ids,
            from_ui,
            to_ui,
            events,
            retry,
            ..
        } = self;
        events.clear();
        let pushed = from_ui.pushed();
        let deferred = fill_events(retry, from_ui, ranges, events, 0);
        let input = events.as_clap();
        let mut emit = |index: usize, value: f64| {
            to_ui.push(index, value, 0);
        };
        let mut sink = OutputSink {
            ids,
            ranges,
            emit: &mut emit,
        };
        let output = sink.as_clap();
        unsafe { flush(*plugin, &input, &output) };
        // `flush` reports nothing back: handed over is applied.
        if !deferred && retry.is_empty() {
            from_ui.mark_applied(pushed);
        }
    }

    #[cfg(test)]
    pub(super) fn limit_events_for_test(&mut self, capacity: usize) {
        self.events = EventBuffer::with_capacity(capacity);
    }
}

/// Kept values first, then the transfer: a newer one for the same
/// parameter follows, so it wins.  A value the full list cannot take
/// stays in `retry`, and nothing newer gets in after that, so a kept
/// value is never older than one that went.  True when the drain
/// deferred a value.
fn fill_events(
    retry: &mut Retry,
    from_ui: &mut ParamReceiver,
    ranges: &[ClapRange],
    events: &mut EventBuffer,
    last: i32,
) -> bool {
    retry.retain(|index, value| match ranges.get(index) {
        Some(range) => !events.push_value(range.id, range.denormalise(value), 0),
        None => false,
    });
    from_ui.drain(|index, value, at| {
        let Some(range) = ranges.get(index) else {
            return;
        };
        if !events.push_value(range.id, range.denormalise(value), at.clamp(0, last) as u32) {
            retry.put(index, value);
        }
    })
}
