//! The UI sets a flag.  The bridge thread that owns the driver calls
//! `controlPanel` between callbacks.  Nothing here opens a driver.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Two ASIO devices are two drivers, so each side has its own request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AsioPanelSide {
    Input,
    Output,
}

/// The UI sets these; the bridge clears them.  One per run.
pub(crate) struct AsioPanelRequests {
    input: AtomicBool,
    output: AtomicBool,
    /// A counter: both drivers can have a panel open.
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

    /// A second request before the bridge takes the first stays one open.
    pub(crate) fn request(&self, side: AsioPanelSide) {
        self.flag(side).store(true, Ordering::Release);
    }

    /// `true` once per request.  Linux keeps the method so the flag can be
    /// tested without a driver.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(crate) fn take(&self, side: AsioPanelSide) -> bool {
        self.flag(side).swap(false, Ordering::AcqRel)
    }

    /// Does not clear an open panel.  One that is already open still finishes.
    pub(crate) fn clear_requests(&self) {
        self.input.store(false, Ordering::Relaxed);
        self.output.store(false, Ordering::Relaxed);
    }

    /// The guard drops on unwind too.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(crate) fn enter(&self) -> AsioPanelOpen<'_> {
        let _ = self.open.fetch_add(1, Ordering::AcqRel);
        AsioPanelOpen(self)
    }

    /// A panel that pauses callbacks is not a stalled device, and stop waits
    /// it out.
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
