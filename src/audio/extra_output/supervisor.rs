//! Add, remove and reconnect never stop the clock.
//!
//! A table only touches the flags of a slot it opened or is retrying.
//! PipeWire node streams use the same bookkeeping.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::super::output_map::MAX_EXTRA_OUTPUTS;
use super::super::shared::{ExtraOutputPlan, Shared};
use super::stream::{open_extra_output, OpenedExtraOutput};

const RETRY_AFTER: Duration = Duration::from_millis(750);

/// Owned by the bridge thread.
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

    pub(crate) fn sync(&mut self, shared: &Arc<Shared>) {
        // Duplex fans out in the graph.  Do not open cpal streams there.
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

/// Keyed by the device id or node name it was opened for.
pub(crate) struct ExtraOutputSlots<T> {
    slots: [Option<(String, T)>; MAX_EXTRA_OUTPUTS],
    retry_at: [Option<Instant>; MAX_EXTRA_OUTPUTS],
}

impl<T> ExtraOutputSlots<T> {
    pub(crate) fn new() -> Self {
        Self {
            slots: std::array::from_fn(|_| None),
            retry_at: [None; MAX_EXTRA_OUTPUTS],
        }
    }

    /// `open` runs with the ring-active flag off.  A failure stays missing
    /// until [`RETRY_AFTER`] elapses.
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

        // A slot that left the plan, or changed device.
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
                // The clock callback may still finish a push.  The next open
                // resets the ring.
                stop_feeding(shared, slot);
            }
            // A slot still wanted keeps its backoff.  One that left clears
            // `missing` only if this table owned it.
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
                // Lost device.  A PipeWire stream connects asynchronously, so
                // an immediate reopen would fail again every tick.  The slot
                // stays missing until the backoff.
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
                    // The producer sizes its bridge buffer from the clock step.
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

fn stop_feeding(shared: &Shared, slot: usize) {
    shared
        .extra_output_ring_active(slot)
        .store(false, Ordering::Release);
    shared.extra_output_bus(slot).set_playback_rate(0);
}

/// A PipeWire node is not one of these: duplex links it, and the native
/// bridge opens a node stream for it.
fn generic_extra_outputs(plan: &ExtraOutputPlan) -> Vec<(usize, String)> {
    plan.slots
        .iter()
        .take(plan.count)
        .enumerate()
        .filter_map(|(slot, link)| {
            if link.device_id.is_empty() || !super::is_generic_extra_output(&link.device_id) {
                return None;
            }
            // RigWerk Out is in-graph.  The Windows VB-Cable virtual output
            // is a WASAPI extra output and only plays while it is on.
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
