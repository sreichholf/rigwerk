//! The audio-thread half of a loaded LV2 plugin: the port buffers, `run`, and
//! the control values that cross the transfer queues.
//!
//! LV2 has no `start_processing`/`stop_processing`: `activate` and
//! `deactivate` are instantiation-class calls done on the UI thread at load
//! and teardown, so [`Lv2Processor::start`] and [`Lv2Processor::stop`] only
//! exist to fill the format-neutral slot's protocol.  Everything here is an
//! audio-class call or a write into a preallocated buffer.
//!
//! Normalised UI values arrive through the UI→audio transfer; at the start
//! of a block they are denormalised with the shared range table and written
//! into the control input buffers, which the plugin reads during `run`.
//! Nothing is called on the plugin from the UI, and nothing here can fail to
//! apply a value, so there is no retry.  Changed control outputs (meters,
//! gain reduction) go back through the audio→UI transfer, and the latency
//! port's value is published through [`Lv2Core`]'s atomics.
//!
//! Nothing here allocates, locks, logs or opens a file: the buffers, the
//! ranges and the transfer rings are sized at load, and every failure is a
//! static [`Lv2Failure`].

use std::sync::atomic::Ordering;
use std::sync::Arc;

use lv2_sys::LV2_Handle;

use crate::audio::plugins::{Lv2Failure, ParamIds, ParamReceiver, ParamSender};

use super::instance::Lv2Core;
use super::ports::{Lv2Range, PortBuffers};

/// The audio-thread half.  It lives in the chain's slot; the UI only drops
/// it, on teardown, after the slot left every chain.
pub(crate) struct Lv2Processor {
    /// Every declared port's buffer, connected once at load.
    pub(super) buffers: PortBuffers,
    /// The ranges the transfers are indexed by, shared with the instance.
    pub(super) ranges: Arc<[Lv2Range]>,
    pub(super) ids: Arc<ParamIds>,
    pub(super) from_ui: ParamReceiver,
    pub(super) to_ui: ParamSender,
    /// The instance handle, owned by `core`.
    pub(super) handle: LV2_Handle,
    /// `run`, resolved at load.  Never null for a loaded plugin.
    pub(super) run: unsafe extern "C" fn(LV2_Handle, u32),
    /// The last output value reported per range, as `f32` bits, so only a
    /// value that moved crosses back.
    pub(super) reported: Vec<u32>,
    pub(super) max_block: usize,
    /// Last: the handle outlives everything above that points into it.
    pub(super) core: Arc<Lv2Core>,
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

    /// LV2 has no processing stop: `deactivate` is an instantiation-class
    /// call, so a teardown deactivates through the instance, not here.
    pub(crate) fn stop(&mut self) -> Result<(), Lv2Failure> {
        Ok(())
    }

    /// Runs the plugin over one buffer in place, in chunks of at most the
    /// block size it was activated with.  UI values queued for it go into the
    /// control buffers of the first chunk; changed control outputs and the
    /// latency port are read after every chunk.
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
            core,
        } = self;
        // A write into a control buffer always succeeds, so the values count
        // as applied once this drain took them; no retry is needed.
        drain_values(ids, ranges, buffers, from_ui);
        let mut offset = 0;
        while offset < frames {
            let count = (frames - offset).min(*max_block);
            // Auxiliary inputs and outputs stay silent; the plugin may write
            // into any output or CV buffer.
            buffers.silence(count);
            buffers.copy_input(
                &left[offset..offset + count],
                &right[offset..offset + count],
                count,
            );
            buffers.reset_atoms();
            // SAFETY: the instance is active, every declared port is
            // connected, and the buffers hold `count` frames.
            unsafe { run(*handle, count as u32) };
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

    /// For a processing effect the chain skips (bypass, mix at 0): write the
    /// queued UI values into the control buffers so the next processed block
    /// and a later state save see them.  No plugin call is needed: LV2 reads
    /// its control ports during `run`, which this does not make.
    pub(crate) fn flush_if_queued(&mut self) {
        drain_values(
            &self.ids,
            &self.ranges,
            &mut self.buffers,
            &mut self.from_ui,
        );
    }
}

/// Writes every queued UI value into its control buffer and reports the
/// values applied.  A write always succeeds, so only a drain that deferred an
/// overflow value (see [`ParamReceiver::drain`]) leaves the applied count
/// alone; the next block or flush takes it.
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
