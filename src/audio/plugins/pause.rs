//! The pause handshake for plugin work that must not overlap processing:
//! VST3 `setActive`, `setupProcessing`, a `restartComponent` for latency or
//! I/O, a state load; CLAP `deactivate` / `activate`.  Format-neutral.
//!
//! Only the stop and start calls belong to the audio thread (VST3
//! `setProcessing` may be called there, as may CLAP `stop_processing` /
//! `start_processing`); the work in between is UI-thread work.  The
//! handshake is atomics only, so the audio side never blocks or allocates.
//!
//! The audio side brackets every block with [`PauseHandshake::begin_block`]
//! and the [`BlockGuard`] it returns, so the UI can tell when no audio
//! thread is inside the plugin and may force a pause without waiting for
//! one (no bridge, a lingering one, a path that went inactive).
//! [`drive_pause`] is the UI side, one call per frame.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::time::{Duration, Instant};

/// How long blocks may stop being served before [`drive_pause`] takes the
/// stream to be stopped and forces the pause.  Forcing is always safe (see
/// [`PauseHandshake`]), so this is only how long a stalled pause waits.
/// Well above the gap between callbacks with large buffers (8192 frames at
/// 48 kHz is about 170 ms), so a running stream is not taken for a stopped
/// one and its pause goes through the audio side's stop call.
pub(crate) const PAUSE_STALL: Duration = Duration::from_millis(500);

/// Processing as usual.
const RUNNING: u8 = 0;
/// The UI asked for a pause; the audio side has not seen it yet.
const REQUESTED: u8 = 1;
/// The audio side saw the request and is making its stop call, or a
/// request arrived while the plugin was not processing (before a restart or
/// its first start): the next block marks it paused with no stop call.
const STOPPING: u8 = 2;
/// The audio side has stopped, or the UI forced the pause: blocks skip.
/// The UI's work waits for [`PauseHandshake::quiet`].
const PAUSED: u8 = 3;
/// Not processing, and the next block makes the start call: the UI
/// finished its work, or the plugin is new and has not started yet.
const RESUMING: u8 = 4;

/// What the audio side does with this block, from
/// [`BlockGuard::action`].
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
    /// call [`PauseHandshake::started`] if it succeeded, then process this
    /// block.  If it failed, call [`PauseHandshake::start_failed`] and play
    /// the block dry: the plugin is then paused and later blocks skip until
    /// the UI resumes it.  An insert that already failed skips the start
    /// call and goes straight to `start_failed`.
    Start,
}

/// One plugin's pause handshake.  The sequence:
///
/// 1. The UI calls [`Self::request`].
/// 2. The audio side's next [`Self::begin_block`] returns
///    [`BlockAction::Stop`]; it makes the stop call and calls
///    [`Self::stopped`], which marks the plugin paused.
/// 3. The UI waits for [`Self::quiet`], does its work and calls
///    [`Self::resume`].
/// 4. The next `begin_block` returns [`BlockAction::Start`]: the audio
///    side makes the start call, calls [`Self::started`] and processes the
///    block.
///
/// The paused mark is stored after the stop call and the UI's `quiet` load
/// acquires the guard's release, so the UI's work happens after the stop
/// call, and the start call (which sees `RESUMING`) after the UI's work.
///
/// Processing starts lazily on the audio thread: a new handshake is for
/// an activated plugin that is not processing yet, so its first block
/// returns [`BlockAction::Start`].
///
/// A [`Self::resume`] before the audio side stopped cancels the request
/// with no Stop/Start pair; one during its stop call turns into a Start on
/// the next block.  A [`Self::request`] before the audio side restarted
/// (or before its first block) cancels the start, and the next block marks
/// the plugin paused with no stop call.
///
/// When no block comes to serve the request, the UI calls [`Self::force`],
/// waits for [`Self::quiet`], and makes the stop call itself when
/// [`Self::take_processing`] returns true.  It leaves with
/// [`Self::resume`] as usual: the next block makes the start call.  After a
/// force, `resume` refuses until `quiet` has returned true and the stop
/// call is taken: a block still inside the plugin, or a plugin still
/// processing, would otherwise get a second start call.
///
/// Forcing is safe whatever the audio side does (Dekker): `begin_block`
/// stores `in_block = true` and then reads or moves `state`; `force` swaps
/// `state` to paused and `quiet` then loads `in_block`.  All four are
/// `SeqCst`, so in their single total order either the audio's state
/// access comes after the swap (it sees paused and returns
/// [`BlockAction::Skip`]), or its `in_block` store comes before the UI's
/// load (the UI sees the block, and waits for the guard's release).
///
/// One block at a time per handshake: `in_block` is a bool, not a count.
/// The processor's `&mut` and the chain's `try_lock` give that.  A guard
/// kept too long, or leaked, only keeps a pause from finishing (`quiet`
/// stays false); it never makes one unsafe.
pub(crate) struct PauseHandshake {
    state: AtomicU8,
    paused_blocks: AtomicU64,
    /// Audio side: true from `begin_block` until its guard drops.
    in_block: AtomicBool,
    /// The plugin is processing: set by [`Self::started`], cleared by
    /// [`Self::stopped`] or [`Self::take_processing`], whichever thread
    /// made the last start or stop call.
    processing: AtomicBool,
    /// Blocks begun, ever: lets the UI see whether the audio side runs.
    served: AtomicU64,
    /// UI side: set by [`Self::force`], cleared by the [`Self::quiet`] that
    /// follows it.  [`Self::resume`] refuses while it is set.
    forced: AtomicBool,
}

/// One block of the audio side, from [`PauseHandshake::begin_block`].
/// Keep it until the block is done with the plugin: dropping it tells the
/// UI that no audio thread is inside.  Hold at most one per handshake at a
/// time (see [`PauseHandshake`]).
#[must_use]
pub(crate) struct BlockGuard<'a> {
    pause: &'a PauseHandshake,
    action: BlockAction,
}

impl BlockGuard<'_> {
    /// What to do with this block.
    pub(crate) fn action(&self) -> BlockAction {
        self.action
    }
}

impl Drop for BlockGuard<'_> {
    fn drop(&mut self) {
        // Release: the UI's `quiet` load sees every plugin call of the block.
        self.pause.in_block.store(false, Ordering::Release);
    }
}

impl PauseHandshake {
    /// For an activated plugin that is not processing yet: the first
    /// block starts it.
    pub(crate) fn new() -> Self {
        Self {
            state: AtomicU8::new(RESUMING),
            paused_blocks: AtomicU64::new(0),
            in_block: AtomicBool::new(false),
            processing: AtomicBool::new(false),
            served: AtomicU64::new(0),
            forced: AtomicBool::new(false),
        }
    }

    /// Moves the state through `next` and returns the state it moved from
    /// (or the current one, when `next` leaves it alone).  `SeqCst` both
    /// ways: when `next` leaves the state alone this is a plain load, and
    /// [`Self::begin_block`] needs that load in the total order too.
    fn step(&self, next: impl FnMut(u8) -> Option<u8>) -> u8 {
        match self
            .state
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, next)
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

    /// UI side: true once the plugin is marked paused.  A block may still
    /// be inside it after a [`Self::force`]; the UI's work waits for
    /// [`Self::quiet`].
    pub(crate) fn is_paused(&self) -> bool {
        self.state.load(Ordering::Acquire) == PAUSED
    }

    /// UI side: the work is done; the audio side restarts on its next
    /// block.  Before the audio side stopped, this just cancels the
    /// request.  True unless refused.
    ///
    /// Refused, with the plugin left paused, after a [`Self::force`] that
    /// no [`Self::quiet`] has confirmed yet, or while the plugin is still
    /// processing (a forced pause whose [`Self::take_processing`] has not
    /// been taken): the start call would overlap a block still inside the
    /// plugin, or start it twice.  After [`PauseStep::Ready`] it is never
    /// refused.
    #[must_use]
    pub(crate) fn resume(&self) -> bool {
        let mut refused = false;
        self.step(|state| {
            refused = false;
            match state {
                REQUESTED => Some(RUNNING),
                STOPPING => Some(RESUMING),
                PAUSED => {
                    // Loaded after the state: a pause the audio side made
                    // published `processing` with it.
                    refused = self.forced.load(Ordering::Acquire)
                        || self.processing.load(Ordering::Acquire);
                    (!refused).then_some(RESUMING)
                }
                _ => None,
            }
        });
        !refused
    }

    /// UI side: marks the plugin paused at once, whatever the audio side
    /// is doing.  A block already inside the plugin finishes it; every
    /// later block skips.  Wait for [`Self::quiet`], then make the stop
    /// call if [`Self::take_processing`] says so.
    pub(crate) fn force(&self) {
        self.forced.store(true, Ordering::Release);
        self.state.swap(PAUSED, Ordering::SeqCst);
    }

    /// UI side: paused, and no block is inside the plugin, so the UI's
    /// work may start.  True also confirms a [`Self::force`], so
    /// [`Self::resume`] may take effect.
    pub(crate) fn quiet(&self) -> bool {
        let quiet = self.is_paused() && !self.in_block.load(Ordering::SeqCst);
        if quiet {
            self.forced.store(false, Ordering::Release);
        }
        quiet
    }

    /// UI side, once [`Self::quiet`]: true when the plugin is still
    /// processing, so the UI must make the stop call itself (and it is
    /// marked stopped now).  False when the audio side already stopped it,
    /// or it never started.
    pub(crate) fn take_processing(&self) -> bool {
        self.processing.swap(false, Ordering::AcqRel)
    }

    /// UI side: true when no block can be between its start call and
    /// [`Self::started`]: paused, or waiting to start with no block inside.
    /// A plugin that is idle may be paused at once with no stop call.
    pub(crate) fn is_idle(&self) -> bool {
        match self.state.load(Ordering::SeqCst) {
            PAUSED => true,
            RESUMING => !self.in_block.load(Ordering::SeqCst),
            _ => false,
        }
    }

    /// True while the plugin is processing (started and not stopped).
    pub(crate) fn processing(&self) -> bool {
        self.processing.load(Ordering::Acquire)
    }

    /// Blocks the audio side has begun, ever.
    pub(crate) fn served(&self) -> u64 {
        self.served.load(Ordering::Relaxed)
    }

    /// Audio side, at the start of every block.  Hold the guard until the
    /// block is done with the plugin.
    pub(crate) fn begin_block(&self) -> BlockGuard<'_> {
        // Before the state access, both `SeqCst`: see the type's docs.
        self.in_block.store(true, Ordering::SeqCst);
        self.served.fetch_add(1, Ordering::Relaxed);
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
        BlockGuard {
            pause: self,
            action,
        }
    }

    /// Audio side, after a successful start call a [`BlockAction::Start`]
    /// asked for.
    pub(crate) fn started(&self) {
        self.processing.store(true, Ordering::Release);
    }

    /// Audio side, after a start call a [`BlockAction::Start`] asked for
    /// failed: marks the plugin paused and not processing, so later blocks
    /// skip instead of processing a plugin that never started.  The UI sees
    /// [`Self::is_paused`]; a [`Self::resume`] tries the start again.
    pub(crate) fn start_failed(&self) {
        self.processing.store(false, Ordering::Release);
        self.step(|state| matches!(state, RUNNING | REQUESTED).then_some(PAUSED));
    }

    /// Audio side, after the stop call a [`BlockAction::Stop`] asked for:
    /// marks the plugin stopped and paused.  Leaves the state alone if the
    /// UI resumed meanwhile (the next block then starts again) or forced it.
    pub(crate) fn stopped(&self) {
        self.processing.store(false, Ordering::Release);
        self.step(|state| (state == STOPPING).then_some(PAUSED));
    }

    /// Blocks the audio side played dry because of a pause, ever.
    // Read by the tests; Diagnostics counts pauses per insert instead
    // (`SlotControls`), so a reset there does not clear this.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn paused_blocks(&self) -> u64 {
        self.paused_blocks.load(Ordering::Relaxed)
    }
}

/// One frame of [`drive_pause`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PauseStep {
    /// Not paused yet: call again next frame.
    Waiting,
    /// Stopped, and no block is inside the plugin: do the work, then
    /// [`PauseHandshake::resume`], which succeeds from here.
    Ready,
}

/// UI side: one frame of a pause the UI wants, for any format.  `wait`
/// starts as `None` and is kept between frames; [`PauseStep::Ready`] sets
/// it back to `None`, so the same one serves the next pause.
///
/// The first call requests the pause.  A plugin that is not processing
/// (never started, or already stopped) is forced at once; so is one whose
/// blocks have not been served for [`PAUSE_STALL`] (the stream stopped).
/// Otherwise the audio side's next block makes the stop call.  Once
/// [`PauseHandshake::quiet`], `stop` runs if the plugin is still
/// processing: the stop call the audio side did not make.
pub(crate) fn drive_pause(
    pause: &PauseHandshake,
    wait: &mut Option<(Instant, u64)>,
    stop: impl FnOnce(),
) -> PauseStep {
    let now = Instant::now();
    let served = pause.served();
    let stalled = match *wait {
        None => {
            pause.request();
            *wait = Some((now, served));
            false
        }
        Some((_, seen)) if seen != served => {
            *wait = Some((now, served));
            false
        }
        Some((since, _)) => now.duration_since(since) >= PAUSE_STALL,
    };
    if stalled || !pause.processing() {
        pause.force();
    }
    if !pause.quiet() {
        return PauseStep::Waiting;
    }
    if pause.take_processing() {
        stop();
    }
    *wait = None;
    PauseStep::Ready
}

#[cfg(test)]
mod tests;
