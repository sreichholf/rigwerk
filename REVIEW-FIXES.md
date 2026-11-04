# Follow-up: must-fix findings

This file records the work done after `REVIEW.md` was committed. The six
blocking findings (M1-M6) were implemented by independent agents on
`review-fixes`; each fix was then checked by an independent read-only reviewer.

## Fix commits

| Finding | Commit | Subject |
| --- | --- | --- |
| M1 | `b6ab061` | Make the audio ring lock-free |
| M2 | `59992eb`, `f2770c9` | Never block the audio callback on plugin locks; keep the CLAP audio path lock-free and rate-safe |
| M3 | `da24c64`, `9ae88ac` | Give every bridge start its own shared session; give preview its own shared session |
| M4 | `82af36b`, `84c6e5b` | Make bridge stop asynchronous and detect dead sessions; harden stopping against late errors and hung teardown |
| M5 | `9b211fc` | Process ASIO buffers inside the driver callback |
| M6 | already fixed by `9b211fc` | ASIO input gain is read per callback; no separate commit was needed |

## Reviewer verdicts

- **M1:** PASS. SPSC ring memory ordering, full/empty conditions, wrapping,
  `ring_frames`/`ring_fill`/`reset`, and no per-frame ring lock all verified.
- **M2:** PASS after follow-up. Audio callback uses non-blocking locks end to
  end; CLAP host-state mutex removed from the audio path; `start_processing`
  moved to load time; `add_plugin` rate race closed.
- **M3:** PASS after follow-up. Real starts and preview each get a fresh
  `Arc<Shared>`; old bridge threads keep the old session and cannot write into
  the new one. Preview no longer resets a lingering session.
- **M4:** PASS after hardening. Stop is asynchronous; `Stopping`, 12 s start
  timeout, 4 s frames heartbeat, clean-exit detection, panic capture, 15 s
  teardown watchdog, and pending-restart cancellation are in place.
- **M5:** PASS. ASIO buffers are read/written only in
  `bufferSwitch`/`bufferSwitchTimeInfo` (plus the prescribed silence prefill
  before `driver.start()`); the channel/worker buffer path is gone; callback
  contexts are CAS-owned per session; no blocking mutex or allocation on the
  normal callback path.
- **M6:** PASS. Every ASIO input path reaches one per-callback gain read; no
  cached session gain remains.

## Checks run here

Installed Rust 1.99.1 (musl) and the Linux build dependencies in this container.
No tests were run (container policy).

```sh
source "$HOME/.cargo/env"
cargo fmt --check                         # pass
cargo check                               # pass; 2 pre-existing dead-code warnings
cargo check --target x86_64-pc-windows-gnu # pass
RUSTFLAGS="-C target-feature=-crt-static" cargo build  # pass
```

The Linux link needs `RUSTFLAGS="-C target-feature=-crt-static"` on this Alpine
image because `alsa-lib` and `xcb` do not ship static archives here. The default
static-pie link fails with `cannot find -lasound` / `cannot find -lxcb`; that is
an environment packaging limitation, not a source error. `cargo check` for the
Windows GNU target type-checks the ASIO/WASAPI code and passes.

## Remaining risks after the fixes

These are recorded so the fixes are not overclaimed:

- **M2:** UI-side chain holds now cause a dry bypass rather than an audio block.
  `refresh_rate`/`ensure_loaded`/`plugin_views`/`service_plugins` can still hold
  the chain for a long time; a plugin processing error still formats a string on
  the audio thread; `vst3-host`'s internal level mutex remains untouched.
- **M3/M4:** a driver that never returns from teardown leaves a parked thread
  and a visible error; the 15 s watchdog tells the user to restart RigWerk but
  cannot release a stuck driver. Late teardown errors are suppressed for the
  state machine but may still appear in the activity log/status tile. The
  hidden-window `logic()` gap (H10) means these watchdogs are not polled while
  the window is minimized. Input-side death is not part of the output frames
  heartbeat.
- **M5:** no ASIO hardware or Windows desktop was available, so callback
  timing, `outputReady` behavior, and driver stop/dispose semantics are
  compile-verified only. `outputReady` is issued one wake-up later and can
  coalesce; a driver that requires it from the callback thread may behave
  differently.
- **M6:** gain is applied per block, not ramped per sample, so large slider
  moves can still step; the H3 ramping issue remains.

The non-blocking findings in `REVIEW.md` (resampler aliasing, drift splices,
plugin lifecycle/state, device identity, scan false positives, latency
reporting, UI geometry/accessibility, packaging) are unchanged.
