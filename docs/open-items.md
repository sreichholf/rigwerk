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
  `vst3-host` 0.11 opens Linux editors as X11 windows only and does not
  implement `IWaylandHost` / `IWaylandFrame`; XWayland with `DISPLAY` set
  works.  The plugin still runs, and RigWerk's parameter panel still opens.
- A VSTGUI editor built with VST3 SDK 3.8 (the SDK's `again` sample, for
  one) crashes RigWerk on Linux when it opens.  That VSTGUI takes the
  `Linux::IRunLoop` from the factory's host context, and `vst3-host` 0.11
  offers it only on the plug frame.  The editor test in
  `app/plugin_gui/tests.rs` skips the open on Linux until the host
  context offers it.

## Not reproduced

- A sink starving under Low latency.  The sink monitor behind the Live
  silence row reads a live PipeWire graph (see "A PipeWire graph without
  hardware" in `AGENTS.md`), but the starving case has not been seen.

## Known limits

Recorded when the review fixes landed (November 2026), not re-checked
since.

- **Plugin chain lock.**  When the audio thread finds the chain locked, the
  plugin output for that chunk is lost: the path plays its input delayed
  by the chain latency, so timing holds but the inserts' sound drops out.
  The step into that dry fallback is declicked over 2 ms; once the chain
  runs again it waits one latency, then crossfades back over 5 ms.  Short
  holds that can collide: a plugin reload for a new output rate, opening an
  editor for a slot that was never loaded, cloning the plugin views, a
  parameter sync, and VST3 editor servicing (vst3-host has no lock-free
  path).
- **VST3 level lock.**  vst3-host's `Plugin::process_audio` takes a
  blocking `Mutex` (`audio_levels.lock()`) after every block, on the audio
  thread.  This is upstream, not RigWerk's code.
- **VST3 latency changes.**  A plugin that changes its latency through
  its processor (the SDK's host checker) never reaches the new value.
  vst3-host 0.11 hands the processor's output parameters to the host
  only, never to the controller, so the controller's restart request is
  never raised.  When a restart does come, it reads `getLatencySamples`
  before reactivating, and a plugin that applies the new latency in
  `setActive (true)` reports the old one.  The host checker test prints
  the value it gets.
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
