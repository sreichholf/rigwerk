//! A request to open an ASIO driver's control panel.
//!
//! The UI thread only sets a flag.  The bridge thread that created the
//! driver takes the flag between callbacks and is the one that calls
//! `controlPanel`.  Nothing here opens a driver, so the flag can be tested
//! without hardware.  The call itself stays in the Windows ASIO bridge.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Which bridge side owns the driver.  Two ASIO devices are two drivers,
/// so each side has its own request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AsioPanelSide {
    Input,
    Output,
}

/// Flags the UI sets and the bridge clears.  One per session, on the shared
/// state both sides already hold.
pub(crate) struct AsioPanelRequests {
    input: AtomicBool,
    output: AtomicBool,
    /// Bridge threads currently inside `controlPanel`.  More than one when
    /// the input and output drivers both have a panel open.
    open: AtomicUsize,
}

impl AsioPanelRequests {
    pub(crate) const fn new() -> Self {
        Self {
            input: AtomicBool::new(false),
            output: AtomicBool::new(false),
            open: AtomicUsize::new(0),
        }
    }

    /// The UI side.  A second request before the bridge takes the first
    /// stays one open.
    pub(crate) fn request(&self, side: AsioPanelSide) {
        self.flag(side).store(true, Ordering::Release);
    }

    /// The bridge side.  `true` once per request, then the flag is clear.
    /// Only the Windows bridge calls this; Linux keeps the flag so the
    /// same type can be tested without a driver.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(crate) fn take(&self, side: AsioPanelSide) -> bool {
        self.flag(side).swap(false, Ordering::AcqRel)
    }

    /// Drops any request the bridge has not taken.  Does not clear
    /// [`Self::enter`]: a panel that is already open still has to finish.
    pub(crate) fn clear_requests(&self) {
        self.input.store(false, Ordering::Relaxed);
        self.output.store(false, Ordering::Relaxed);
    }

    /// Marks the panel open until the guard drops, including on unwind.
    /// Only the Windows bridge calls this, around `controlPanel`.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(crate) fn enter(&self) -> AsioPanelOpen<'_> {
        let _ = self.open.fetch_add(1, Ordering::AcqRel);
        AsioPanelOpen(self)
    }

    /// Whether a bridge thread is inside `controlPanel`.  The engine uses
    /// this so a driver that pauses callbacks for its panel is not treated
    /// as stalled, and so stop waits out the panel.
    pub(crate) fn is_open(&self) -> bool {
        self.open.load(Ordering::Acquire) > 0
    }

    fn flag(&self, side: AsioPanelSide) -> &AtomicBool {
        match side {
            AsioPanelSide::Input => &self.input,
            AsioPanelSide::Output => &self.output,
        }
    }
}

/// Clears the open mark when the `controlPanel` call returns.
/// Only the Windows bridge constructs it.
#[must_use = "the panel stays marked open until this guard drops"]
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) struct AsioPanelOpen<'a>(&'a AsioPanelRequests);

impl Drop for AsioPanelOpen<'_> {
    fn drop(&mut self) {
        let _ = self.0.open.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests;
