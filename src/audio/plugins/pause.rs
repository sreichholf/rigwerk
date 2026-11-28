//! Pause handshake so plugin work does not overlap processing (VST3
//! `setActive`, `setupProcessing`, `restartComponent`, a state load; CLAP
//! `deactivate` / `activate`).
//!
//! Stop and start are the audio thread's (VST3 `setProcessing`, CLAP
//! `stop_processing` / `start_processing`).  The work between them is the
//! UI thread's.  Atomics only: the audio side never blocks or allocates.
//!
//! The UI may force a pause when no block is inside the plugin.  Unquiet
//! for [`PAUSE_STRANDED`] is stranded: the slot is not destroyed while a
//! thread may still be inside `process`.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::time::{Duration, Instant};

/// How long blocks may stop being served before [`drive_pause`] takes the
/// stream to be stopped and forces the pause.  Forcing is always safe (see
/// [`PauseHandshake`]), so this is only how long a stalled pause waits.
/// Well above the gap between callbacks with large buffers (8192 frames at
/// 48 kHz is about 170 ms), so a running stream is not taken for a stopped
/// one and its pause goes through the audio side's stop call.
pub(crate) const PAUSE_STALL: Duration = Duration::from_millis(500);

/// How long a pause may stay unconfirmed before it is taken to be stranded:
/// four times [`PAUSE_STALL`].  A plugin hung inside `process` never drops
/// its [`BlockGuard`], so [`PauseHandshake::quiet`] never turns true and the
/// effect would stay "paused" forever.  Four stalls give a stream that
/// stopped and restarted a few times, or a large buffer, plenty of room; a
/// block held past that is treated as stranded, whether it is a hang, a
/// debugger stop on the audio thread or severe preemption.
pub(crate) const PAUSE_STRANDED: Duration = Duration::from_secs(2);

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BlockAction {
    Process,
    /// Make the stop call (`setProcessing(false)` / `stop_processing`),
    /// then call [`PauseHandshake::stopped`].  The effect plays this block
    /// dry-delayed through the skip path.
    Stop,
    /// Stopped: keep playing dry-delayed.
    Skip,
    /// Make the start call (`setProcessing(true)` / `start_processing`),
    /// call [`PauseHandshake::started`] if it succeeded, then process this
    /// block.  If it failed, call [`PauseHandshake::start_failed`] and play
    /// the block dry: the plugin is then paused and later blocks skip until
    /// the UI resumes it.  An effect that already failed skips the start
    /// call and goes straight to `start_failed`.
    Start,
}

/// One plugin's pause handshake.  Atomics only.
///
/// The paused mark is stored after the stop call, and `quiet` acquires the
/// guard's release, so UI work happens after the stop and the start call
/// (which sees `RESUMING`) happens after that work.
///
/// A new handshake is an activated plugin that is not processing yet: its
/// first block returns [`BlockAction::Start`].  [`Self::resume`] before
/// the audio side stopped cancels the request.  [`Self::request`] before
/// the restart cancels the start, and the next block marks paused with no
/// stop call.  After [`Self::force`], `resume` refuses until `quiet` and
/// the stop call: a block still inside, or a plugin still processing,
/// would get a second start.
///
/// Forcing is Dekker.  `begin_block` stores `in_block` then reads `state`;
/// `force` swaps `state` to paused and `quiet` loads `in_block`.  All four
/// are `SeqCst`.  Either the audio side's state access is after the swap
/// (it sees paused and skips) or its `in_block` store is before the UI's
/// load (the UI waits for the guard).
///
/// One block at a time: `in_block` is a bool.  The processor's `&mut`
/// gives that.  A guard held too long only keeps `quiet` false.  Unquiet
/// for [`PAUSE_STRANDED`] is stranded: the effect fails and the slot is
/// not destroyed.
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
    /// UI side: set by [`drive_pause`] when a pause stayed unquiet for
    /// [`PAUSE_STRANDED`].  Never cleared: a plugin that never returned
    /// from `process` is never trusted again.
    stranded: AtomicBool,
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
            stranded: AtomicBool::new(false),
        }
    }

    /// `SeqCst` both ways.  When `next` leaves the state alone this is a
    /// load, and [`Self::begin_block`] needs that load in the total order.
    fn step(&self, next: impl FnMut(u8) -> Option<u8>) -> u8 {
        match self
            .state
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, next)
        {
            Ok(previous) | Err(previous) => previous,
        }
    }

    /// UI side.  A no-op while a pause is already requested or in place.
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

    /// True from a [`Self::request`] (or [`Self::force`]) until the
    /// [`Self::resume`] that ends it: a pause is on its way or in place, so
    /// blocks skip the plugin or soon will.
    pub(crate) fn in_pause(&self) -> bool {
        matches!(
            self.state.load(Ordering::Acquire),
            REQUESTED | STOPPING | PAUSED
        )
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

    pub(crate) fn processing(&self) -> bool {
        self.processing.load(Ordering::Acquire)
    }

    pub(crate) fn served(&self) -> u64 {
        self.served.load(Ordering::Relaxed)
    }

    /// Whether a pause was stranded: a block has been inside the plugin for
    /// [`PAUSE_STRANDED`] and the plugin no longer processes audio.  Set
    /// once and never cleared; a plugin that never returned from `process`
    /// is never driven again, and the slot that holds it is never
    /// destroyed.
    pub(crate) fn stranded(&self) -> bool {
        self.stranded.load(Ordering::Acquire)
    }

    /// UI side, once: marks the pause stranded.  The processor is still
    /// inside `process`, so no processor call follows.
    fn mark_stranded(&self) {
        self.stranded.store(true, Ordering::Release);
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

    /// Audio side, after a successful [`BlockAction::Start`].
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

    // Diagnostics counts pauses per effect (`SlotControls`), so a reset
    // there does not clear this.  The tests read it.
    #[cfg(test)]
    pub(crate) fn paused_blocks(&self) -> u64 {
        self.paused_blocks.load(Ordering::Relaxed)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PauseStep {
    /// Not paused yet: call again next frame.
    Waiting,
    /// Stopped, and no block is inside the plugin: do the work, then
    /// [`PauseHandshake::resume`], which succeeds from here.
    Ready,
    /// A block has been inside the plugin for [`PAUSE_STRANDED`] and never
    /// came out: the plugin is hung in `process`.  The handshake is marked
    /// stranded and stays so; the caller fails the effect.  No stop call
    /// was made and none may be.
    Stranded,
}

/// UI side, one frame.  `wait` starts as `None` and [`PauseStep::Ready`]
/// clears it, so the same slot serves the next pause.
///
/// Not processing, or unserved for [`PAUSE_STALL`], is forced at once.
/// Once [`PauseHandshake::quiet`], `stop` runs only if the plugin is still
/// processing.  Unquiet for [`PAUSE_STRANDED`] (from the last served block
/// or the request) is [`PauseStep::Stranded`] and stays so after a late
/// `quiet`: no stop call was made and none may be.
pub(crate) fn drive_pause(
    pause: &PauseHandshake,
    wait: &mut Option<(Instant, u64)>,
    stop: impl FnOnce(),
) -> PauseStep {
    if pause.stranded() {
        return PauseStep::Stranded;
    }
    let now = Instant::now();
    let served = pause.served();
    let mut waiting_since = None;
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
        Some((since, _)) => {
            waiting_since = Some(since);
            now.duration_since(since) >= PAUSE_STALL
        }
    };
    if stalled || !pause.processing() {
        pause.force();
    }
    if !pause.quiet() {
        // Nothing served a block and no stop call confirmed the pause for
        // the whole stranded deadline: the block inside the plugin is not
        // coming out.
        let stranded =
            waiting_since.is_some_and(|since| now.duration_since(since) >= PAUSE_STRANDED);
        if stranded {
            pause.mark_stranded();
            *wait = None;
            return PauseStep::Stranded;
        }
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
