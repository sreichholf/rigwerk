//! Keeps generic follower streams in step with the live follower plan.
//! Add/remove/reconnect never stops the clock bridge.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::super::shared::Shared;
use super::stream::{open_follower, OpenedFollower};

/// How long to wait after a failed open before trying again.
const RETRY_AFTER: Duration = Duration::from_millis(750);

/// Live follower streams owned by the bridge thread.
pub(crate) struct FollowerOutputs {
    slots: [Option<OpenedFollower>; super::super::output_map::MAX_FOLLOWERS],
    /// Next time a missing slot may be retried.
    retry_at: [Option<Instant>; super::super::output_map::MAX_FOLLOWERS],
    buffer_frames: usize,
    sample_rate: u32,
}

impl FollowerOutputs {
    pub(crate) fn new(buffer_frames: usize, sample_rate: u32) -> Self {
        Self {
            slots: std::array::from_fn(|_| None),
            retry_at: [None; super::super::output_map::MAX_FOLLOWERS],
            buffer_frames,
            sample_rate,
        }
    }

    /// Opens, closes and reconnects followers to match the plan.  Safe to call
    /// every bridge tick.
    pub(crate) fn sync(&mut self, shared: &Arc<Shared>) {
        // PipeWire duplex fans out in-graph; never open cpal followers there.
        if shared.direct.load(Ordering::Relaxed) {
            self.close_all(shared);
            return;
        }

        let plan = shared.follower_plan();
        let wanted: Vec<(usize, String)> = (0..plan.count)
            .filter_map(|slot| {
                let link = &plan.slots[slot];
                if link.virtual_out || link.device_id.is_empty() {
                    return None;
                }
                if !super::is_generic_follower(&link.device_id) {
                    return None;
                }
                Some((slot, link.device_id.clone()))
            })
            .collect();

        // Close slots that left the plan or changed device.
        for slot in 0..self.slots.len() {
            let keep = wanted.iter().any(|(s, id)| {
                *s == slot
                    && self.slots[slot]
                        .as_ref()
                        .is_some_and(|open| open.device_id == *id)
            });
            if !keep {
                if self.slots[slot].take().is_some() {
                    // Stop the producer; do not reset the ring here — the clock
                    // callback may still finish a push.  open_follower resets
                    // before the next open.
                    shared
                        .follower_ring_active(slot)
                        .store(false, Ordering::Release);
                }
                // Do not clear retry_at here when the slot is still wanted —
                // a missing device keeps its backoff.
                if !wanted.iter().any(|(s, _)| *s == slot) {
                    self.retry_at[slot] = None;
                    shared
                        .follower_bus(slot)
                        .missing
                        .store(false, Ordering::Relaxed);
                }
            }
        }

        let now = Instant::now();
        for (slot, device_id) in wanted {
            if self.slots[slot].is_some() {
                // Still playing; clear a stale missing flag from a brief glitch.
                if !shared.follower_bus(slot).missing.load(Ordering::Relaxed) {
                    continue;
                }
                // Device reported lost: drop and retry.
                self.slots[slot].take();
                shared
                    .follower_ring_active(slot)
                    .store(false, Ordering::Release);
            }

            if let Some(at) = self.retry_at[slot] {
                if now < at {
                    continue;
                }
            }

            match open_follower(
                slot,
                &device_id,
                self.sample_rate,
                shared,
                self.buffer_frames,
            ) {
                Ok(opened) => {
                    shared
                        .follower_ring_active(slot)
                        .store(true, Ordering::Release);
                    shared
                        .follower_bus(slot)
                        .missing
                        .store(false, Ordering::Relaxed);
                    // Tell the producer the clock's block size once we know the
                    // output rate (open_output already published it).
                    let clock_step = shared.output_step_frames.load(Ordering::Relaxed) as usize;
                    if clock_step > 0 {
                        shared.follower_bus(slot).note_input_block(clock_step);
                    }
                    self.slots[slot] = Some(opened);
                    self.retry_at[slot] = None;
                }
                Err(_) => {
                    shared
                        .follower_bus(slot)
                        .missing
                        .store(true, Ordering::Relaxed);
                    shared
                        .follower_ring_active(slot)
                        .store(false, Ordering::Release);
                    self.retry_at[slot] = Some(now + RETRY_AFTER);
                }
            }
        }
    }

    fn close_all(&mut self, shared: &Shared) {
        for slot in 0..self.slots.len() {
            if self.slots[slot].take().is_some() {
                shared
                    .follower_ring_active(slot)
                    .store(false, Ordering::Release);
            }
            self.retry_at[slot] = None;
        }
    }
}
