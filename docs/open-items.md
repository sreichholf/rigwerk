# Open items

What is known to be missing or limited.  Everything else from the old
plans and the code review has landed; that history is in git.

## Features

- RigWerk Out is published by the PipeWire duplex node alone.  The
  two-stream bridge (a mixed bridge, or the duplex fallback) does not
  play it; its other followers play.
- A take on a follower with more than four channels still includes the
  metronome click: only narrower followers carry the click-free lanes
  through their ring.
- A VST3 editor cannot open in a Wayland session with no X server.
  RigWerk's VST3 host (`audio/vst3`) opens Linux editors as X11 windows
  only and does not implement `IWaylandHost` / `IWaylandFrame`; XWayland
  with `DISPLAY` set works.  The plugin still runs, and RigWerk's
  parameter panel still opens.
- `vst3-host` is still a dependency, for the VST3 bundle scan only
  (`app/plugins.rs`).  Step 4 of #28 replaces it.

## Not reproduced

- A sink starving under Low latency.  The sink monitor behind the Live
  silence row reads a live PipeWire graph (see "A PipeWire graph without
  hardware" in `AGENTS.md`), but the starving case has not been seen.
- Plugin editors on a real X11 desktop and on Windows.  The editor test
  opens and closes AGain's editor under Xvfb and on the Windows CI
  runner; nobody has used an editor by hand on our own VST3 host yet.

## Known limits

Recorded when the review fixes landed (November 2026), not re-checked
since.

- **Plugin chain lock.**  Structural edits still take the chain lock:
  adding, removing, moving or restoring inserts, reopening them for a
  new rate or because a plugin changed its layout, and opening a slot
  that was never loaded.  The audio thread
  `try_lock`s it; when it is busy, the plugin output for that chunk is
  lost: the path plays its input delayed by the chain latency, so timing
  holds but the inserts' sound drops out.  The step into that dry
  fallback is declicked over 2 ms; once the chain runs again it waits
  one latency, then crossfades back over 5 ms.  Step 3 of #28 hands the
  chain to the audio side.
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
  seconds, so a large offset at start, or a Low-latency cushion, can
  underrun once before it has learned the offset.  A large cushion cut
  pops the excess, which the resampler turns into a short crossfade.
