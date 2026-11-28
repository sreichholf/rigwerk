//! ASIO requires `controlPanel` on the thread that created the driver,
//! between callbacks.  A buffer change still arrives as `kAsioResetRequest`;
//! opening the panel does not start a second reset.

use azo::driver::Driver;

use crate::audio::asio_panel::AsioPanelSide;
use crate::audio::shared::Shared;

/// A duplex driver passes both sides and opens once.  The bool is whether
/// the call ran, so the watchdog ignores the time the panel held this
/// thread.  A missing or refused panel does not end the run.
pub(crate) fn service_asio_panel(
    driver: &impl Driver,
    shared: &Shared,
    sides: &[AsioPanelSide],
) -> bool {
    let mut requested = false;
    for side in sides {
        // `|=` takes every side.  `any` would leave the other request set,
        // so a duplex bridge would open twice.
        requested |= shared.asio_panel.take(*side);
    }
    if !requested {
        return false;
    }
    // Held across the call, including a panic, so a stalled heartbeat is
    // not left suppressed after the panel returns.
    let _open = shared.asio_panel.enter();
    // The driver's own reset request ends the run when the panel
    // changes the buffer or the rate.  A panel that fails to open does not.
    let _ = driver.open_control_panel();
    true
}
