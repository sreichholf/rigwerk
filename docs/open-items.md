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
- LV2 effects use RigWerk's parameter panel only; their X11 and Windows
  UIs are not embedded yet.
- The LV2 host is effects-only: no MIDI, time position, `patch:`
  parameters, CV ports or presets, and atom ports are connected empty.  A
  plugin whose only controls are `patch:writable` properties therefore has
  no panel controls.  An optional CV port gets a silent buffer, an optional
  port of a kind the host cannot use is connected to NULL, and a required
  one is a load error.  Atom ports are only connected to `atom:Sequence`;
  a required atom port that lists no such `atom:bufferType` is the same
  load error.  The scan lists such a plugin with the loader's own error.
- LV2 plugins that require a fixed or power-of-two block length, `log:log`,
  `state:makePath`, `instance-access`, `data-access`, `resize-port` or an
  option the host does not supply are listed with an error and refuse to
  load.  A default state (`state:loadDefaultState`) restores only the
  values that have a plain atom form: paths, URIs, numbers, booleans and
  strings.  `state:mapPath` keeps the path it is given, so a saved sample
  or impulse response path is absolute and does not move with the session.
- LV2 worker responses are delivered before a `run`; a bypassed effect
  keeps them, and values set while no audio runs stay queued until the
  next processed block.  State saves keep only `LV2_STATE_IS_POD` values
  and accept empty values (some plugins store empty defaults).  An LV2
  state value whose body contains URIDs (an `atom:Object` or a sequence)
  is stored opaque and only round-trips in the process that saved it.  A
  top-level `atom:URID` is stored as the URI.

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

- **Loudness.**  Integrated loudness is the last 60 s, not the whole run.
  *Learn* is one-shot; nothing follows the player.  Learn measures an
  output for 8 s, needs at least 27 absolute-gated 400 ms blocks (about 3 s
  of signal), and writes an output trim of ±12 dB.  There is one match per
  path, measured before the path fader, so it is the same on every output.
  It is matched to a target or to another path with *Match paths*, which
  measures every path for 4 s and proposes a ±24 dB trim.  Muted paths are
  measured too: the meter runs before the mute, so a muted path reads what
  it would play.  Returns are excluded by default because their loudness
  follows their sends; layered paths are handled with the tick boxes.
  Both Learn and Match refuse a reading more than 20 dB below the
  reference, so noise and silence cannot get a full boost.  Both measure
  before the fader, so the fader stays a plain volume control.  There is
  no true-peak meter, and a device with more than eight channels is
  measured on its first eight only.  NAM normalization uses the file's RMS
  figure, not LUFS, and only the last running capture of a Pre or Post
  group is normalized.
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
  rate other than the output's still does not install: nothing resamples a
  file to another rate.  A series of two or more captures that can be
  oversampled runs each block at twice the output rate through a fixed 2x
  up/down pair.  What still cannot oversample is an LSTM stack (its
  recurrence is in samples), a WaveNet with a conv head
  (`head_kernel_size` above 1, or a post-stack head with a kernel above 1),
  and a capture whose file rate is not the output rate.  Each capture's
  load figure is measured once off the audio thread, at the current rate,
  factor and output step, and is not measured again when only the buffer
  size changes; a block's runtime cost is not measured per block.  A
  reorder dips the touched group to silence for about 20 ms instead of
  crossfading.  The level match between two blocks uses only the files'
  `input_level_dbu`/`output_level_dbu`; without them it is 0 dB.
  Impedance interaction is not modelled, and captures that already include
  a boost or a cabinet double up with a pedal or the Cab row.
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
- **Backend selection.**  `audio/bridge.rs` names one arm per backend, but
  the arms are gated to the platform that has them: ASIO only on Windows,
  native PipeWire only on Linux.  A backend has no arm to return an error
  from on another target, so a backend id from another platform falls
  through to that target's fallback instead, and its arm cannot be exercised
  there.
