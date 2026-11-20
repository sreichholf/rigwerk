# RigWerk manual

The full tour of the app.  For what RigWerk is and how to build it, see the
[README](../README.md).

## The console

The console is the home screen. The signal runs left to right — input,
paths, master, outputs — and every strip ends in a meter and a fader, so
levels line up across the screen.

![The console](screenshots/console.png)

**Title bar.** The session menu, bridge start and stop, the estimated
latency, Rec, the metronome, Tuner, Setup and Mute. The dot beside the
bridge reads *Stopped*, *Starting…*, *Running*, *Demo*, *Stopping…* or
*Error*.
**Ctrl+Shift+M** mutes the master from any RigWerk window.

**Input.** The device, driver and channel count; In 1 and In 2; monitor;
the scope; and the input gain. A click on the device opens Setup › Input.

**Paths.** Up to four, each with:

- a source: which input channels feed it, in stereo or summed to mono;
- a chain in signal order: Pre EQ, the gate (when before), NAM blocks
  before the effects, the effects, NAM blocks after them, the cab, the
  gate (when after), Post EQ;
- its latency and DSP load;
- level, mute and solo.

The dashed column adds an empty path, a duplicate (muted), or applies a
starting layout: *Single path*, *Dry L / Wet R*, *Blend clean*, *Dual
amp*, *Per-output mixes*. A layout only changes routing; your effects stay
where they are. The path name menu renames a path, sets its colour,
duplicates it (muted) and removes it; path outputs (L, R) are in the path
sheet.

![Four paths](screenshots/console-4-paths.png)

**Master.** One level and one mute for the sum of all paths.

**Outputs.** The clock output first, then the extra outputs. An output
shows which paths play on it (one dot per path), its metronome switch, its
buffer, a limiter reduction bar, one meter per channel, and its level and
mute. The dashed column adds another device, or the virtual output: on
Windows that is VB-Cable, on PipeWire it is RigWerk Out. An extra output
that fails stays silent and is retried; the clock output keeps playing.

At the 1040 × 700 minimum the strips get denser, and the paths group
scrolls sideways when not all four fit. Everything a narrow step hides
stays in that item's sheet.

![The console at the minimum size](screenshots/console-1040x700.png)

**Status bar.** Drivers and rates, buffer sizes, the estimated and the
measured latency, drift, dropouts, total DSP load, and the newest log line
for five seconds. On the right, the issue count opens the issues drawer and
*Log* opens diagnostics. A click on the drivers, rates, buffers or latency
opens Setup at that section.

**Metronome.** Tempo (40–240 BPM), rhythm (4/4, 3/4, 6/8, 7/8 and other
common bars, with an accented downbeat) and click level. Each output picks
the channels the click plays on, so it can go to your headphones only. The
click is not part of the path mix, the scope or the meters, it keeps
ticking when the rig is muted, and it stops while the tuner mutes the
outputs. It always starts stopped.

**The bridge buffer.** When the input and the clock output run on different
clocks, RigWerk converts between them and holds a little audio in between:
the bridge buffer. It is *Auto*, *Minimum* or *Fixed* in Setup › Clock.
*Auto* grows after a dropout; *Fixed* sets the size in milliseconds. On a
single PipeWire graph clock nothing has to be held, and RigWerk says so.
The status bar shows drift and dropouts while the bridge runs.

**Keeping paths in time.** Effects such as amp sims and lookahead limiters
add delay. When two paths with different delays play on one output they
sound hollow (comb filtering), so RigWerk delays the faster paths to match
the slowest one there. Each output can turn this off when nothing is
summed on it, and each path has its own level, polarity and a small timing
trim per output. Paths are lined up within one output, not between
outputs, and only for delays up to about 0.7 s. A path cannot feed another
path; duplicate it instead.

## Sheets, drawers and diagnostics

Click a stage in a chain, a path name or an output to edit it in a sheet at
the bottom of the console. A sheet covers one thing at a time: EQ, gate,
NAM, cab, effect, path, output or scope. It can be resized from its top
edge and moves through the chain with *‹ previous* and *next ›*. Esc
closes it. The title bar and status bar keep working while it is open,
and each strip shows a thin meter.

- **EQ** has five bands (low shelf, three bells, high shelf) with a curve
  and a band table, and a *Flat* button.
- **Gate** has threshold, range, hysteresis, attack, release and hold, a
  reduction meter, *Before effects* / *After effects*, and what it
  listens to: the *Gated signal*, or the *Dry input* before the chain,
  lined up with the effects' latency.  Keyed on the dry input, a gate
  after a high-gain amp is not held open by the amp's own noise; set its
  threshold for the guitar's level.
- **NAM** edits one block: a Neural Amp Modeler `.nam` capture from the
  models folder, *Before effects* / *After effects*, input and output
  trims, *Auto level*, and a width dial for slimmable captures.  Blocks
  stack, e.g. a drive pedal into an amp, up to 16 per path; the fifth
  asks once.  Each row shows its measured DSP load.  *Auto level* matches
  a block to the one before it when both files carry calibration levels.
  When the DSP alarm is up, the sheet points at the heaviest blocks.
- **Cab** loads a `.wav` impulse response from a folder, with level, low
  and high cut and polarity.  It sits after the effects and the NAM
  blocks, and adds no latency.
- **Effect** has on/bypass, wet, latency, state, its controls, *Editor*,
  *Reset to defaults* and *Remove*.
- **Path** has name, colour, source, path outputs (which of the path's
  L and R play at all, before any output), level, mute, solo, duplicate,
  layouts and its latency on each output it plays on.
- **Output** has the channel map grid and its presets, the metronome
  channels, *Channels nothing plays* (mono copy or silent), and per path
  the level here, polarity, trim and when it arrives. The alignment switch
  and the bridge buffer of an extra output are here too.
- **Scope** shows input and output overlaid, with the peaks and the sample
  count.

![The EQ sheet](screenshots/eq-sheet.png)

![The NAM sheet](screenshots/nam-sheet.png)

![The cab sheet](screenshots/cab-sheet.png)

![The output sheet](screenshots/output-sheet.png)

The effects group opens the **effect browser** from its dashed *+ Add
effect* row, or from an empty chain, and the browser lists VST3 and CLAP
plugins found in the standard folders (and in `CLAP_PATH` or
`RIGWERK_CLAP_PATH` for CLAP).
Search by name, maker, format or category; a prefix match ranks first,
then favourites and recently used effects. ↑/↓ chooses, Enter adds,
Shift+Enter adds and opens the editor. *Add from path* and *Rescan* are in
the footer. Adding, removing or reordering effects does not stall the
audio.

![The effect browser](screenshots/browser.png)

The **issues drawer** opens from the status bar count. Each row says what
is wrong, what to do and where: *Show* closes the drawer and selects the
thing the issue points at, in the console or in Setup. Warnings that allow
it can be dismissed. Errors also show a banner under the title bar until
they clear or are dismissed; *Audio device disconnected* is one, with
*Rescan* and *Open Setup*.

The **diagnostics sheet** opens from *Log*. Its tabs are the activity log
(newest first; Copy and Export write it oldest first), performance (DSP
load per path and in total, skipped buffers, reset), the latency estimate
and the measurement's conditions, the scan notes, and the engine status
with the last error.

**Measure latency.** From Setup › Latency, *Measure latency…* plays a short
test click through a cable from the output back into the input, or through
a microphone in front of the speaker, and times its return. The clock
output is muted for the whole capture, not only while the click plays. The
result is a measurement; the estimate beside it is not. A stored result is
marked stale, with the reason, when the input, output, rate, buffer or
output mode changes.

## Tuner

The tuner is a full screen: *Tuner* in the title bar, or **Ctrl+T**; Esc
or Ctrl+T goes back. It shows the note very large, its octave, the target
and the heard frequency, and a needle or a strobe with ±50 cents and the
in-tune band. Level and confidence run underneath.

Pick Chromatic, or one of the 20 tunings (6, 7 and 8-string guitar; 4, 5
and 6-string bass). A tuning shows one cell per string with its note,
target frequency and cents, and reads every string from a single strum;
the string being played is underlined. The bottom bar sets the tuning, A4,
the in-tune tolerance (±3, ±5 or ±10 cents), the input channel (L, R or
Sum), *Listen* (single note or all strings), *Mute while tuning* and the
display (needle or strobe).

*Mute while tuning* silences the outputs and the metronome while the tuner
is open. Without a running bridge the tuner asks you to start it.

![The tuner](screenshots/tuner.png)

## Setup

Setup is one scrolling screen: **Ctrl+,** or *Setup*. *Done* returns to
the console.

![Setup](screenshots/setup.png)

On the left are the devices. **Input** lists direct drivers (ASIO,
PipeWire, ALSA `hw:`) and system ones (WASAPI, DirectSound, MME,
PulseAudio), with *In use* and *Demo device* marks, then its rate, buffer
size and the driver line, and *Driver panel* when it is ASIO. **Clock
output** has the same list, rate (*Match input* or a rate), buffer size and
output mode: *Standard* or *Low latency* on Linux, *Shared*, *Shared,
smallest buffer* or *Exclusive* on WASAPI. ASIO, DirectSound and MME have
no output mode; an ASIO clock output has *Driver panel* too. *Rescan
devices*, the PipeWire graph line and the driver note for this platform
sit at the bottom. On Windows the ASIO4ALL conflict warning shows here.

On the right:

- **Latency**: the estimate as a large number with its parts (*In*,
  *Bridge buffer*, *Chain*, *Out*, *After RigWerk*, *Alignment*), then the
  measured loopback with its rates, buffers, mode and age, and *Measure
  latency…*. The estimate covers the clock output's slowest audible path,
  alignment included.
- **Clock**: drift, *Drift correction*, the bridge buffer and dropouts.
- **Start**: *Start the bridge when RigWerk opens*.
- **Appearance**: theme *System*, *Dark* or *Light*, and size 80–150 %.
  *System* follows the desktop's setting, with dark as the fallback.
  **Ctrl++**, **Ctrl+−** and **Ctrl+0** change the size from any screen.
- **Effects**: whether a double-click opens the *Editor* or the
  *Controls*, the plugin folders that are scanned, and *Rescan plugins*.
- **Recordings**: the folder takes are written to.
- **Help**: the setup guide for this platform, as collapsible sections,
  and *About*.

![The console in the light theme](screenshots/console-light.png)

## Sessions and recordings

A session stores the devices, rates, buffers, paths, effects, outputs and
the bridge buffer under a name. The session menu in the title bar lists
saved sessions with their rate, buffer and latency. *Save* (**Ctrl+S**)
updates the current name or adds it; *Save as new session*
(**Ctrl+Shift+S**) adds one under a new name; *Rename* and *Delete* are
there too, and *Start new session* names the current setup *Untitled
session* without changing it. When the setup differs from the saved
session of that name, the menu shows *edited*, and loading over it needs a
second click. Loading replaces the setup and reopens the devices. It
leaves the metronome and each output's click channels alone.

**Rec** records the clock output, or on Windows the virtual output when
there is one, after its level, limiter and mute, to a WAV file named after
the session. Writing happens on its own thread, so a slow disk does not
cause dropouts. The Rec menu shows the elapsed time, the lost samples and
the path of the last take. The folder is set in Setup › Recordings and
defaults to `Recordings` in your home folder.

## Keys

| Key | Does |
| --- | --- |
| Ctrl+Shift+M | Mute or unmute the master |
| Ctrl+T | Tuner, and back |
| Ctrl+, | Setup |
| Ctrl+S / Ctrl+Shift+S | Save session / save as new session |
| Ctrl+F | The effect browser for the selected path |
| Ctrl++ / Ctrl+− / Ctrl+0 | Size up, down, 100 % |
| Alt+↑ / Alt+↓ | Move the focused effect or NAM block |
| Ctrl+Enter | Open the focused effect's editor |
| Esc | Close the innermost popup, measure window, drawer, sheet or the tuner |

