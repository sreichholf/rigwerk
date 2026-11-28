//! The audio-thread half: port buffers and `run`.
//!
//! LV2 has no `start_processing`/`stop_processing`.  `activate` and
//! `deactivate` are instantiation-class calls on the UI thread, so
//! [`Lv2Processor::start`] and [`Lv2Processor::stop`] only fill the slot
//! protocol.  Nothing here allocates, locks, logs or opens a file.  A
//! control write always succeeds, so a drained value is applied; there is
//! no retry.
//!
//! Before each `run`, responses are drained into one preallocated buffer
//! and `work_response` is called; `end_run` follows `run` when the plugin
//! has one, response or not.  Both are audio-class.  The load's zero-frame
//! `run` delivers responses on the UI thread before this processor exists,
//! and teardown delivers what is left after it is gone
//! (`worker::unregister`).

use std::ffi::c_void;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use lv2_sys::{LV2_Handle, LV2_Worker_Status};

use crate::audio::plugins::{Lv2Failure, ParamIds, ParamReceiver, ParamSender};

use super::instance::Lv2Core;
use super::ports::{Lv2Range, PortBuffers};
use super::worker::{WorkerRings, RING_BYTES};

/// Audio-class worker calls for one instance.
pub(crate) struct WorkerCallbacks {
    /// The instance's response ring.  The worker thread's `respond` writes
    /// it; while this processor exists, only it reads the ring.
    pub(crate) rings: Arc<WorkerRings>,
    /// The buffer one response is popped into; [`RING_BYTES`], sized at
    /// load so a block never allocates.
    pub(crate) responses: Vec<u8>,
    /// One call per queued response, before `run`.
    pub(crate) work_response:
        Option<unsafe extern "C" fn(LV2_Handle, u32, *const c_void) -> LV2_Worker_Status>,
    /// `end_run`: after every `run` when the plugin has one, response or
    /// not.
    pub(crate) end_run: Option<unsafe extern "C" fn(LV2_Handle) -> LV2_Worker_Status>,
}

/// Lives in the chain slot.  Dropped on the UI thread after the slot left
/// every chain.  Drop hands the buffers to the core, freed after `cleanup`.
pub(crate) struct Lv2Processor {
    /// Connected once at load.  Moved into [`Lv2Core::retired`] on drop.
    pub(super) buffers: PortBuffers,
    /// Shared with the instance, so both halves read one table.
    pub(super) ranges: Arc<[Lv2Range]>,
    pub(super) ids: Arc<ParamIds>,
    pub(super) from_ui: ParamReceiver,
    pub(super) to_ui: ParamSender,
    pub(super) handle: LV2_Handle,
    /// Resolved at load.  Never null for a loaded plugin.
    pub(super) run: unsafe extern "C" fn(LV2_Handle, u32),
    /// The last output value reported per range, as `f32` bits, so only a
    /// value that moved crosses back.
    pub(super) reported: Vec<u32>,
    pub(super) max_block: usize,
    pub(super) worker: Option<WorkerCallbacks>,
    /// Outlives the buffers once they are retired.
    pub(super) core: Arc<Lv2Core>,
}

impl Drop for Lv2Processor {
    /// The plugin points into the buffers until `cleanup`.  The instance may
    /// hold the core longer, so the buffers move there.  UI thread: the
    /// audio side never drops a slot.
    fn drop(&mut self) {
        let _ = self.core.retired.set(std::mem::take(&mut self.buffers));
    }
}

// SAFETY: the processor makes only audio-class calls (`run`) through the
// handle, from whichever thread holds the slot, one at a time; LV2 lets that
// thread change between blocks.  The buffers' raw pointers are only handed to
// the plugin through `connect_port` at load, on the UI thread, and point into
// the processor's own storage.
unsafe impl Send for Lv2Processor {}

impl Lv2Processor {
    /// LV2 has no processing start: `activate` ran at load, on the UI thread.
    pub(crate) fn start(&mut self) -> Result<(), Lv2Failure> {
        Ok(())
    }

    /// LV2 has no processing stop.  `deactivate` is instantiation-class, on
    /// the instance.
    pub(crate) fn stop(&mut self) -> Result<(), Lv2Failure> {
        Ok(())
    }

    /// Chunks of at most the block size the plugin was activated with.
    pub(crate) fn process(
        &mut self,
        left: &mut [f32],
        right: &mut [f32],
    ) -> Result<(), Lv2Failure> {
        if !self.core.active.load(Ordering::Acquire) {
            return Err(Lv2Failure::NotActive);
        }
        let frames = left.len().min(right.len());
        let Self {
            buffers,
            ranges,
            ids,
            from_ui,
            to_ui,
            handle,
            run,
            reported,
            max_block,
            worker,
            core,
        } = self;
        // A control write always succeeds, so a drained value counts as
        // applied; no retry.
        drain_values(ids, ranges, buffers, from_ui);
        let mut offset = 0;
        while offset < frames {
            let count = (frames - offset).min(*max_block);
            // Auxiliary inputs stay silent.  The plugin may write any output
            // or CV buffer.
            buffers.silence(count);
            buffers.copy_input(
                &left[offset..offset + count],
                &right[offset..offset + count],
                count,
            );
            buffers.reset_atoms();
            // Responses are audio-class, so they go in just before `run`.
            if let Some(worker) = worker.as_mut() {
                deliver_responses(
                    *handle,
                    &worker.rings,
                    &mut worker.responses,
                    worker.work_response,
                );
            }
            // SAFETY: the instance is active, every declared port is
            // connected, and the buffers hold `count` frames.
            unsafe { run(*handle, count as u32) };
            // `end_run` closes the cycle, with or without responses.
            if let Some(worker) = worker.as_ref() {
                // SAFETY: `end_run` is audio-class and belongs to a plugin
                // that is active; this is the run context it expects.
                if let Some(end_run) = worker.end_run {
                    unsafe { end_run(*handle) };
                }
            }
            buffers.copy_output(
                &mut left[offset..offset + count],
                &mut right[offset..offset + count],
                count,
            );
            for (index, range) in ranges.iter().enumerate() {
                if !range.read_only {
                    continue;
                }
                let bits = buffers.control(range.slot).to_bits();
                if bits != reported[index] {
                    reported[index] = bits;
                    to_ui.push(index, range.normalise(f64::from(f32::from_bits(bits))), 0);
                }
            }
            let bits = buffers.latency().to_bits();
            if bits != core.latency_raw.swap(bits, Ordering::AcqRel) {
                core.latency_changed.store(true, Ordering::Release);
            }
            offset += count;
        }
        Ok(())
    }

    /// Bypass or mix at 0: write queued UI values so the next block and a
    /// later save see them.  No plugin call: LV2 reads controls during
    /// `run`.  The response ring waits the same way.
    pub(crate) fn flush_if_queued(&mut self) {
        drain_values(
            &self.ids,
            &self.ranges,
            &mut self.buffers,
            &mut self.from_ui,
        );
    }
}

/// Oldest first, in the run context.  A plugin without `work_response`
/// drops the response: leaving it would stall later ones.  Called before
/// each `run`, before the load's `run(handle, 0)`, and by
/// `worker::unregister` before `deactivate`.
pub(super) fn deliver_responses(
    handle: LV2_Handle,
    rings: &WorkerRings,
    responses: &mut [u8],
    work_response: Option<
        unsafe extern "C" fn(LV2_Handle, u32, *const c_void) -> LV2_Worker_Status,
    >,
) {
    debug_assert!(
        responses.len() >= RING_BYTES,
        "the response buffer must hold a whole ring"
    );
    while rings.response_pending() {
        let Some(size) = rings.pop_response(responses) else {
            break;
        };
        if let Some(work_response) = work_response {
            // SAFETY: `work_response` is audio-class, called here from the
            // run context, and `size` bytes of the popped message are live in
            // the buffer for the call.
            unsafe {
                work_response(handle, size as u32, responses.as_ptr() as *const c_void);
            }
        }
    }
}

/// A write always succeeds.  Only a drain that deferred an overflow (see
/// [`ParamReceiver::drain`]) leaves the applied count alone.
fn drain_values(
    ids: &ParamIds,
    ranges: &[Lv2Range],
    buffers: &mut PortBuffers,
    from_ui: &mut ParamReceiver,
) {
    let pushed = from_ui.pushed();
    let deferred = from_ui.drain(|index, value, _| {
        if ids.id(index).is_none() {
            return;
        }
        if let Some(range) = ranges.get(index) {
            if !range.read_only {
                buffers.set_control(range.slot, range.denormalise(value) as f32);
            }
        }
    });
    if !deferred {
        from_ui.mark_applied(pushed);
    }
}
