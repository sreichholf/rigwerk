//! The bridge-thread call that opens an ASIO driver's control panel.
//!
//! ASIO requires `controlPanel` on the thread that created the driver.
//! That thread is the bridge (`run_asio_bridge`, `run_asio_input_only`,
//! `run_asio_duplex_bridge`, or `run_asio_output_loop`), and it only calls
//! here between callbacks.  The UI sets a flag; this takes it.  A new
//! buffer still arrives as `kAsioResetRequest`.  This does not start a
//! second reset.

use azo::driver::Driver;

use crate::audio::asio_panel::AsioPanelSide;
use crate::audio::shared::Shared;

/// Opens the panel when any of `sides` was requested.  One driver (a duplex
/// bridge) passes both sides and opens once.  Returns whether the call ran,
/// so the callback watchdog can ignore the time the panel held this thread.
/// A driver with no panel, or one that refuses, does not end the session.
pub(crate) fn service_asio_panel(
    driver: &impl Driver,
    shared: &Shared,
    sides: &[AsioPanelSide],
) -> bool {
    let mut requested = false;
    for side in sides {
        // `|=` takes every side.  `any` would stop at the first and leave
        // the other request set, so a duplex bridge would open twice.
        requested |= shared.asio_panel.take(*side);
    }
    if !requested {
        return false;
    }
    // Held across the call, including a panic, so a stalled heartbeat is
    // not left suppressed after the panel returns.
    let _open = shared.asio_panel.enter();
    // The driver's own reset request ends the session when the panel
    // changes the buffer or the rate.  A panel that fails to open does not.
    let _ = driver.open_control_panel();
    true
}
