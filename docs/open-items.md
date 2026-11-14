# Open items

What is known to be missing or limited.  Everything else from the old
plans and the code review has landed; that history is in git.

## Features

- Native PipeWire playback on two streams (not the duplex node) does not
  play to followers.  `audio/linux/native.rs` has no `FollowerOutputs`.
- A take on a follower with more than four channels still includes the
  metronome click: only narrower followers carry the click-free lanes
  through their ring.

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
