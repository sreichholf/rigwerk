# Feature ideas

Ideas for the audio path that are not planned yet.  MIDI is left out on
purpose: it is the obvious next step and gets its own plan.  What is known
to be missing or broken is in [`open-items.md`](open-items.md), not here.

## Record the dry input and reamp

Rec records one endpoint (`RecordSource::Clock` or an extra output).

- Record the dry In 1 / In 2 sample-aligned with the take, as extra lanes
  or a second file.
- A file input: a WAV played into the paths in place of the device.  A
  reader thread fills a ring and the callback only pops, the mirror of the
  recorder's writer thread.

That gives tone dialling without playing, NAM captures compared on the
same performance, and an offline render.

## Two cabinet IRs per path

The NAM blocks hear `(L + R) / 2` and write the same sample to both
channels, so a path is mono after the amp.  `CabKernel` already holds
separate left and right taps.

- IR A on the left, IR B on the right: a stereo cab almost for free.
- A blend mode: two IRs mixed, with a small time or polarity offset
  between them.

## A doubler

A second guitar from one take: a copy a few to a few tens of ms late,
with the delay and the pitch drifting slowly and independently, panned
against the original.  Two or three voices, each with its own small
offset, sound like separate takes instead of a chorus.  It sits after the
amp and the cab, so it also gives a mono NAM path a real stereo image.
It adds no latency: the copies are only ever late.

## NAM at other rates

A capture at a rate other than the output's does not install.  Run the
NAM group at the capture's rate, with a resampler around it.  That adds
latency, which goes into path alignment like a plugin's.

## Correction and delay per output

- An EQ per output: headphones and monitors want different curves.
- A delay for the whole output, mainly to line the stream or the virtual
  output up with video.  The per-path trim lines paths up; it is not an
  A/V offset.

## More inputs

The input is fixed at two channels (`input_channels: [bool; 2]`).

- More channels from one device: guitar plus a mic on a 4-in interface.
- A second input device on its own clock, drift-corrected into the
  mixer, the mirror of what extra outputs do.

A mic path then wants a compressor and a de-esser.

## A built-in compressor

Bass, clean paths and a compressor pedal before the amp.  It is the stage
the plugin-free rig (EQ, gate, NAM, cab) is still missing.

## Practice tools

Next to the tuner and the metronome:

- A looper after the master that follows the metronome tempo.
- A backing-track player, mixed like the click (not part of the path
  mix), with a loop section.

## A slow continuous level

An automatic level that keeps riding an output in the background, in case
a set-once Learn is ever not enough.  It was dropped because in a live rig
a controller can only follow the player's dynamics: it fights faders,
lifts noise and pauses, and makes a verse and a chorus arrive at the same
level.  Learn and Match paths cover the static changes instead.

## Loudness-matched A/B

Two states of one path, switched from the path sheet so the louder tone
does not win by default.  Each state is a copy of the path's settings,
including its effects and NAM blocks, and the switch waits for the path
gain to reach zero or crossfades, so it does not click.  The comparison
can build on the path match: each state keeps a trim learned from its own
reading.  The routing and the device maps stay outside the A/B; this
compares a sound, not where it plays.

## Plugin sidechain inputs

A plugin bus beyond the main stereo pair gets silence
(`audio/clap/ports.rs`).  Feeding it the dry input or another path would
make ducking and keyed effects work.

## Probably not

A built-in reverb, delay or modulation.  Plugins cover them well, and
an effects return makes one instance serve every path.
