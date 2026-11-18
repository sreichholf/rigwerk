//! Keeps extra output streams in step with the live extra output plan.
//! Add/remove/reconnect never stops the clock bridge.
//!
//! [`ExtraOutputSlots`] is the open / close / retry bookkeeping for one kind of
//! stream: cpal here, PipeWire node streams in `linux::native`.  A table
//! only touches the flags of a slot it opened or is retrying.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::super::output_map::MAX_EXTRA_OUTPUTS;
use super::super::shared::{ExtraOutputPlan, Shared};
use super::stream::{open_extra_output, OpenedExtraOutput};

/// How long to wait after a failed open before trying again.
const RETRY_AFTER: Duration = Duration::from_millis(750);

/// Live generic (cpal) extra output streams owned by the bridge thread.
pub(crate) struct ExtraOutputOutputs {
    streams: ExtraOutputSlots<OpenedExtraOutput>,
    buffer_frames: usize,
    sample_rate: u32,
}

impl ExtraOutputOutputs {
    pub(crate) fn new(buffer_frames: usize, sample_rate: u32) -> Self {
        Self {
            streams: ExtraOutputSlots::new(),
            buffer_frames,
            sample_rate,
        }
    }

    /// Opens, closes and reconnects extra outputs to match the plan.  Safe to call
    /// every bridge tick.
    pub(crate) fn sync(&mut self, shared: &Arc<Shared>) {
        // PipeWire duplex fans out in-graph; never open cpal extra outputs there.
        if shared.direct.load(Ordering::Relaxed) {
            self.streams.close_all(shared);
            return;
        }
        let wanted = generic_extra_outputs(&shared.extra_output_plan());
        let (rate, buffer) = (self.sample_rate, self.buffer_frames);
        self.streams.sync(shared, &wanted, |slot, device_id| {
            open_extra_output(slot, device_id, rate, shared, buffer)
        });
    }
}

/// One open stream per extra output slot, keyed by what it was opened for (a
/// device id or a node name), with the retry backoff of a slot that failed.
pub(crate) struct ExtraOutputSlots<T> {
    slots: [Option<(String, T)>; MAX_EXTRA_OUTPUTS],
    /// Next time a missing slot may be retried.
    retry_at: [Option<Instant>; MAX_EXTRA_OUTPUTS],
}

impl<T> ExtraOutputSlots<T> {
    pub(crate) fn new() -> Self {
        Self {
            slots: std::array::from_fn(|_| None),
            retry_at: [None; MAX_EXTRA_OUTPUTS],
        }
    }

    /// Opens, closes and reopens streams so exactly the `wanted` (slot, key)
    /// pairs play.  `open` lays the slot's ring out and starts the stream; the
    /// ring-active flag is off while it runs.  A failed open marks the slot
    /// missing and is retried after [`RETRY_AFTER`]; a stream that set the
    /// missing flag itself is dropped and retried the same way, missing until
    /// it reopens.
    pub(crate) fn sync(
        &mut self,
        shared: &Shared,
        wanted: &[(usize, String)],
        mut open: impl FnMut(usize, &str) -> Result<T, String>,
    ) {
        let wanted: Vec<&(usize, String)> = wanted
            .iter()
            .filter(|(slot, _)| *slot < MAX_EXTRA_OUTPUTS)
            .collect();

        // Close slots that left the plan or changed device.
        for slot in 0..MAX_EXTRA_OUTPUTS {
            let key = wanted.iter().find(|(s, _)| *s == slot).map(|(_, key)| key);
            let keep = key.is_some_and(|key| {
                self.slots[slot]
                    .as_ref()
                    .is_some_and(|(open, _)| open == key)
            });
            if keep {
                continue;
            }
            let owned = self.slots[slot].is_some() || self.retry_at[slot].is_some();
            if self.slots[slot].take().is_some() {
                // Stop the producer; do not reset the ring here — the clock
                // callback may still finish a push.  `open` resets before the
                // next open.
                stop_feeding(shared, slot);
            }
            // A slot still wanted keeps its backoff.  One that left this
            // table's plan clears the missing flag only if it was ours.
            if key.is_none() && owned {
                self.retry_at[slot] = None;
                shared
                    .extra_output_bus(slot)
                    .missing
                    .store(false, Ordering::Relaxed);
            }
        }

        let now = Instant::now();
        for (slot, key) in wanted {
            let slot = *slot;
            if self.slots[slot].is_some() {
                if !shared
                    .extra_output_bus(slot)
                    .missing
                    .load(Ordering::Relaxed)
                {
                    continue;
                }
                // Device reported lost: drop it and retry after the backoff.
                // A PipeWire stream connects asynchronously, so an immediate
                // reopen would succeed and fail again every tick.  The slot
                // stays missing until the reopen.
                self.slots[slot].take();
                stop_feeding(shared, slot);
                self.retry_at[slot] = Some(now + RETRY_AFTER);
                continue;
            }

            if let Some(at) = self.retry_at[slot] {
                if now < at {
                    continue;
                }
            }

            match open(slot, key) {
                Ok(opened) => {
                    shared
                        .extra_output_ring_active(slot)
                        .store(true, Ordering::Release);
                    shared
                        .extra_output_bus(slot)
                        .missing
                        .store(false, Ordering::Relaxed);
                    // Tell the producer the clock's block size once we know the
                    // output rate (open_output already published it).
                    let clock_step = shared.output_step_frames.load(Ordering::Relaxed) as usize;
                    if clock_step > 0 {
                        shared.extra_output_bus(slot).note_input_block(clock_step);
                    }
                    self.slots[slot] = Some((key.clone(), opened));
                    self.retry_at[slot] = None;
                }
                Err(_) => {
                    shared
                        .extra_output_bus(slot)
                        .missing
                        .store(true, Ordering::Relaxed);
                    shared
                        .extra_output_ring_active(slot)
                        .store(false, Ordering::Release);
                    self.retry_at[slot] = Some(now + RETRY_AFTER);
                }
            }
        }
    }

    pub(crate) fn close_all(&mut self, shared: &Shared) {
        for slot in 0..MAX_EXTRA_OUTPUTS {
            if self.slots[slot].take().is_some() {
                stop_feeding(shared, slot);
            }
            self.retry_at[slot] = None;
        }
    }
}

/// The clock side stops pushing into `slot`'s ring.
fn stop_feeding(shared: &Shared, slot: usize) {
    shared
        .extra_output_ring_active(slot)
        .store(false, Ordering::Release);
    shared.extra_output_bus(slot).set_playback_rate(0);
}

/// The plan's slots that play on their own cpal stream, with their device
/// ids.  A PipeWire node extra output is not one: the duplex node links it, and
/// the native two-stream bridge opens a node stream for it.
fn generic_extra_outputs(plan: &ExtraOutputPlan) -> Vec<(usize, String)> {
    plan.slots
        .iter()
        .take(plan.count)
        .enumerate()
        .filter_map(|(slot, link)| {
            if link.device_id.is_empty() || !super::is_generic_extra_output(&link.device_id) {
                return None;
            }
            // PipeWire RigWerk Out is in-graph.  The Windows VB-Cable virtual
            // output is a WASAPI extra output that only plays while it is on.
            if link.virtual_output {
                #[cfg(target_os = "windows")]
                if link.virtual_output_on {
                    return Some((slot, link.device_id.clone()));
                }
                return None;
            }
            Some((slot, link.device_id.clone()))
        })
        .collect()
}

#[cfg(test)]
mod tests;
