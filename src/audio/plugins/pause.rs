//! The pause handshake for plugin work that must not overlap processing:
//! VST3 `setActive`, `setupProcessing`, a `restartComponent` for latency or
//! I/O, a state load; CLAP `deactivate` / `activate`.  Format-neutral.
//!
//! Only the stop and start calls belong to the audio thread (VST3
//! `setProcessing` may be called there, as may CLAP `stop_processing` /
//! `start_processing`); the work in between is UI-thread work.  The
//! handshake is atomics only, so the audio side never blocks or allocates.

// No callers yet: they arrive with the VST3/CLAP processor/controller split (#28).
#![cfg_attr(not(test), allow(dead_code))]

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};

/// Processing as usual.
const RUNNING: u8 = 0;
/// The UI asked for a pause; the audio side has not seen it yet.
const REQUESTED: u8 = 1;
/// The audio side saw the request and is making its stop call, or a
/// request arrived while the plugin was not processing (before a restart or
/// its first start): the next block marks it paused with no stop call.
const STOPPING: u8 = 2;
/// The audio side has stopped; the UI may do its work.
const PAUSED: u8 = 3;
/// Not processing, and the next block makes the start call: the UI
/// finished its work, or the plugin is new and has not started yet.
const RESUMING: u8 = 4;

/// What the audio side does with this block, from
/// [`PauseHandshake::begin_block`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BlockAction {
    /// Process as usual.
    Process,
    /// Make the stop call (`setProcessing(false)` / `stop_processing`),
    /// then call [`PauseHandshake::stopped`].  The insert plays this block
    /// dry-delayed through the skip path.
    Stop,
    /// Stopped: keep playing dry-delayed.
    Skip,
    /// Make the start call (`setProcessing(true)` / `start_processing`),
    /// then process this block.
    Start,
}

/// One plugin's pause handshake.  The sequence:
///
/// 1. The UI calls [`Self::request`].
/// 2. The audio side's next [`Self::begin_block`] returns
///    [`BlockAction::Stop`]; it makes the stop call and calls
///    [`Self::stopped`], which marks the plugin paused.
/// 3. The UI waits for [`Self::is_paused`], does its work and calls
///    [`Self::resume`].
/// 4. The next `begin_block` returns [`BlockAction::Start`]: the audio
///    side makes the start call and processes the block.
///
/// Every transition is a compare-exchange (`AcqRel` / `Acquire`), and the
/// paused mark is stored after the stop call, so the UI's work after
/// `is_paused` happens after the stop call, and the start call happens
/// after the UI's work.
///
/// Processing starts lazily on the audio thread: a new handshake is for
/// an activated plugin that is not processing yet, so its first block
/// returns [`BlockAction::Start`].
///
/// A [`Self::resume`] before the audio side stopped cancels the request
/// with no Stop/Start pair; one during its stop call turns into a Start on
/// the next block.  A [`Self::request`] before the audio side restarted
/// (or before its first block) cancels the start, and the next block marks
/// the plugin paused with no stop call.  With no bridge running, the UI
/// calls [`Self::stop_without_audio`] instead of waiting, and makes the
/// stop call itself when that returns true.  It leaves with
/// [`Self::resume`] as usual: the next bridge's first block makes the
/// start call.
pub(crate) struct PauseHandshake {
    state: AtomicU8,
    paused_blocks: AtomicU64,
}

impl PauseHandshake {
    /// For an activated plugin that is not processing yet: the first
    /// block starts it.
    pub(crate) fn new() -> Self {
        Self {
            state: AtomicU8::new(RESUMING),
            paused_blocks: AtomicU64::new(0),
        }
    }

    /// Moves the state through `next` and returns the state it moved from
    /// (or the current one, when `next` leaves it alone).
    fn step(&self, next: impl FnMut(u8) -> Option<u8>) -> u8 {
        match self
            .state
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, next)
        {
            Ok(previous) | Err(previous) => previous,
        }
    }

    /// UI side: asks the audio side to stop.  A no-op while a pause is
    /// already requested or in place.
    pub(crate) fn request(&self) {
        self.step(|state| match state {
            RUNNING => Some(REQUESTED),
            // Not restarted yet: the audio side confirms on its next block.
            RESUMING => Some(STOPPING),
            _ => None,
        });
    }

    /// UI side: true once the audio side has made its stop call, so the
    /// UI's work may start.
    pub(crate) fn is_paused(&self) -> bool {
        self.state.load(Ordering::Acquire) == PAUSED
    }

    /// UI side: the work is done; the audio side restarts on its next
    /// block.  Before the audio side stopped, this just cancels the
    /// request.
    pub(crate) fn resume(&self) {
        self.step(|state| match state {
            REQUESTED => Some(RUNNING),
            STOPPING | PAUSED => Some(RESUMING),
            _ => None,
        });
    }

    /// UI side, with no bridge running: marks the plugin paused at once.
    /// True when it was processing, so the UI must make the stop call
    /// itself; false when it was already stopped (paused, not yet
    /// restarted, or never started).  Only while no audio thread uses this
    /// handshake, which rules out a stop call in progress.
    pub(crate) fn stop_without_audio(&self) -> bool {
        matches!(
            self.state.swap(PAUSED, Ordering::AcqRel),
            RUNNING | REQUESTED
        )
    }

    /// Audio side, at the start of every block.
    pub(crate) fn begin_block(&self) -> BlockAction {
        let previous = self.step(|state| match state {
            REQUESTED => Some(STOPPING),
            // A request that cancelled a start: already stopped, or never
            // started.
            STOPPING => Some(PAUSED),
            RESUMING => Some(RUNNING),
            _ => None,
        });
        let action = match previous {
            RUNNING => BlockAction::Process,
            REQUESTED => BlockAction::Stop,
            RESUMING => BlockAction::Start,
            _ => BlockAction::Skip,
        };
        if matches!(action, BlockAction::Stop | BlockAction::Skip) {
            self.paused_blocks.fetch_add(1, Ordering::Relaxed);
        }
        action
    }

    /// Audio side, after the stop call a [`BlockAction::Stop`] asked for:
    /// marks the plugin paused.  Does nothing if the UI resumed meanwhile;
    /// the next block then starts again.
    pub(crate) fn stopped(&self) {
        self.step(|state| (state == STOPPING).then_some(PAUSED));
    }

    /// Blocks the audio side played dry because of a pause, ever.
    pub(crate) fn paused_blocks(&self) -> u64 {
        self.paused_blocks.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests;
