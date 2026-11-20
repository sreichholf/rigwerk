# Open items

What is known to be missing or limited.  Everything else from the old
plans and the code review has landed; that history is in git.

## Features

- RigWerk Out, the virtual output, is played by the PipeWire duplex node alone.  The
  two-stream bridge (a mixed bridge, or the duplex fallback) does not
  play it; its other extra outputs play.
- A take on an extra output with more than four channels still includes the
  metronome click: only narrower extra outputs carry the click-free lanes
  through their ring.
- A VST3 editor cannot open in a Wayland session with no X server.
  RigWerk's VST3 host (`audio/vst3`) opens Linux editors as X11 windows
  only and does not implement `IWaylandHost` / `IWaylandFrame`; XWayland
  with `DISPLAY` set works.  The plugin still runs, and RigWerk's
  parameter panel still opens.
- A VST3 bundle without `moduleinfo.json` (built with an SDK before
  3.7) is listed as one effect named after the bundle, because the scan
  never loads a binary (`audio/vst3/scan.rs`).  Loading it takes the
  factory's first audio class, so a second class in such a bundle cannot
  be picked, and an instrument in one is listed too.

## Not reproduced

- A sink starving under Low latency.  The sink monitor behind the
  console's silence warning (the issues drawer row for an output that went
  quiet) reads a live PipeWire graph (see "A PipeWire graph without
  hardware" in `AGENTS.md`), but the starving case has not been seen.
- Plugin editors on a real X11 desktop and on Windows.  The editor test
  opens and closes AGain's editor under Xvfb and on the Windows CI
  runner; nobody has used an editor by hand on our own VST3 host yet.

## Known limits

Recorded when the review fixes landed (November 2026), not re-checked
since.

- **Cabinet IR.**  A cabinet impulse response is truncated to 50 ms at
  rates up to 192 kHz, and to 9600 taps above that.  The first 128 taps
  are direct; the rest is partitioned FFT convolution.
- **Effect limit.**  A path holds at most 32 effects
  (`plugins::MAX_EFFECTS`): the audio side's slot list is allocated once.
  Adding one more is refused with a message; a saved chain longer than
  that is cut to 32 when it is restored, with only a log line, and the
  next save keeps the 32.
- **NAM blocks.**  A path holds at most 16 (`nam::MAX_NAM_BLOCKS`), for
  memory and load time, not as a product rule.  Blocks are ordered among
  themselves and sit before or after the plugin effects — the two places
  the built-in stage always had — not between two effects.  A capture at a
  rate other than the output's still does not install: nothing resamples
  it.  Each capture's load figure is measured once off the audio thread,
  at the current rate and output step, and is not measured again when only
  the buffer size changes; a block's runtime cost is not measured per
  block.  A reorder dips the touched group to silence for about 20 ms
  instead of crossfading.  The level match between two blocks uses only the
  files' `input_level_dbu`/`output_level_dbu`; without them it is 0 dB.
  Impedance interaction is not modelled, stacked high-gain captures alias
  at 48 kHz, and captures that already include a boost or a cabinet double
  up with a pedal or the Cab row.
- **VSTGUI editor reopened on Linux.**  VSTGUI's X11 run loop closes
  its xcb connection when its last editor closes, without finishing the
  cairo device for it.  If a later editor from the same loaded module
  gets a new connection at the same address, cairo reuses the stale
  device and aborts (`cairo-xcb-screen.c` `_get_screen_index`).  A reload
  opens the new editor before closing the old one, so it never hits
  this; closing an editor by hand and reopening it later still can.
  The fix belongs upstream (`cairo_device_finish` before
  `xcb_disconnect`, as VSTGUI's Wayland path does).
- **CLAP log.**  `host_log` (`clap/host.rs`) builds a `String` and replaces
  `last_log` (under `try_lock`) on whatever thread logs, so a plugin that
  logs from `process` allocates and frees on the audio thread.  Nothing
  reads `last_log`.
- **Stuck drivers.**  A driver that never returns from open or teardown
  leaves a parked bridge thread and an error; RigWerk asks for a restart
  but cannot release the driver.
- **ASIO.**  Callback timing, driver stop and dispose, and reset requests
  are compile-checked only.  `sampleRateDidChange` is ignored (some
  drivers send it during start); only `kAsioResetRequest` ends a session.
- **Resampler.**  Content between the output Nyquist and the lower rate
  minus 20 kHz (24–28 kHz for 96→48) is only partly rejected; it folds
  above the audible band.
- **Drift.**  ±500 ppm is a hard ceiling.  The loop settles over tens of
  seconds, so a large offset at start, or a Minimum bridge buffer, can
  have a dropout once before it has learned the offset.  A large bridge buffer cut
  pops the excess, which the resampler turns into a short crossfade.
