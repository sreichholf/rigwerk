# Open items

What is known to be missing or limited.  Everything else from the old
plans and the code review has landed; that history is in git.

## Features

- Native PipeWire playback on two streams (not the duplex node) does not
  play to followers.  `audio/linux/native.rs` has no `FollowerOutputs`.
- Recordings include the metronome click: a take is what the output
  plays.

## Not reproduced

- A sink starving under Low latency.  The sink monitor behind the Live
  silence row reads a live PipeWire graph (see "A PipeWire graph without
  hardware" in `AGENTS.md`), but the starving case has not been seen.

## Known limits

Recorded when the review fixes landed (November 2026), not re-checked
since.

- **Plugin chain lock.**  When the audio thread finds the chain locked, it
  plays that block dry, without a crossfade.  Short holds that can collide:
  a plugin reload for a new output rate, opening an editor for a slot that
  was never loaded, cloning the plugin views, a parameter sync, and VST3
  editor servicing (vst3-host has no lock-free path).
- A plugin processing error formats a string on the audio thread, once per
  failure.
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
