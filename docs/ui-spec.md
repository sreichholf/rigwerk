# UI rewrite spec

The plan for replacing the nine-page sidebar UI with one console, a tuner
and a setup screen.  It is the contract for the rewrite PRs; once they have
landed, this file is removed like the earlier plans (AGENTS.md).

Mockups: the "RigWerk UI concept" canvas (boards 1–7).  Where a mockup and
this file disagree, this file wins.

## 0. Scope

- `src/ui` is rewritten.
- `src/app` gains the screen, sheet and drawer state, the theme and size
  preferences, issue anchors, session actions and the latency estimate in
  §3.5.  It loses no behaviour.
- `src/audio` gets the renames in §2.4 and these additions, each a small PR
  before the UI that needs it:
  - an `OutputMode` enum published with the opened output, so `app` stops
    classifying mode strings (`app/latency.rs` matches `"REAL…"`,
    `"Low latency…"` today).  This lands before any mode string is renamed,
    or every stored measurement would read stale;
  - the gate's gain reduction in the snapshot (for the gate sheet's meter);
  - a *paused* flag in `PluginView` (an effect restarting);
  - per extra output: its opened rate, driver buffer size and bridge-buffer
    fill in the snapshot (today only dropouts, *missing* and
    *at drift limit* are published).
- `app::plugins` adds channel count and scan error to `PluginCandidate`.
- Not in scope: an EQ input spectrum (needs an analyser on the path; later),
  native file dialogs (no dialog crate; paths stay text fields).

No feature is dropped.  §9 lists every control, reading and state the
current UI has and where it goes.  Things that are new are marked *(new)*.

## 1. Principles

1. **The signal is the interface.**  The home screen shows the path the
   sound takes, left to right, with live meters on every strip.  Settings
   open where the thing they change is drawn.
2. **One name per thing** (§2).  The same word in the UI, the docs and the
   code.
3. **One colour, one meaning.**  The accent marks an engaged state:
   running, selected, armed, muted, soloed.  Path colours identify paths.
   Amber is a warning and red an error, a clip or a recording; both always
   come with an icon or a word.
4. **Measurements over decoration.**  Numbers that change use the mono face.
   An estimate and a measurement are never shown as one value.
5. **No prose on the surface.**  No page taglines and no inline
   explanations.  Hover text explains; the setup guide teaches.
6. **Nothing modal while the bridge plays** except the measure-latency
   window.  Destructive actions (remove a path, delete a session, load a
   session over unsaved edits) use a two-click inline confirm.
7. **Desk first.**  Mouse and keyboard.  Designed at 1440 × 900 points; it
   must work down to today's minimum window, 1040 × 700 points.

## 2. Ubiquitous language

The glossary is binding for UI text, hover text, issue text, log lines,
README, docs and new code.  Words in the "Never in the UI" column may stay
inside platform code where they are the platform's own term (`quantum` in
the PipeWire module, `period` in ALSA), but never in a string a person
reads.

### 2.1 Glossary

| Term | Meaning | Never in the UI |
|---|---|---|
| **Bridge** | RigWerk running: the input, the paths and the outputs.  You start and stop it. | Live Bridge, Linking, "bridge live" |
| **Input** | The device RigWerk listens to.  Its channels are *In 1* and *In 2*. | capture, IN CLOCK |
| **Input gain** | The input's level in dB.  *Gain* alone is only an EQ band's boost or cut. | input level |
| **Output** | Any device RigWerk plays on. | endpoint, sink |
| **Clock output** | The output whose callback runs the paths.  Exactly one. | clock device, OUT CLOCK |
| **Extra output** | An output that plays the master as well: on its own clock with its own bridge buffer, or, on PipeWire, in the same graph with none. | follower, "also playing on" |
| **Virtual output** | An extra output other apps record from: *RigWerk Out* on PipeWire, VB-Cable on Windows. | publish, published, endpoint |
| **Path** | One of up to four parallel chains from the input to the outputs. | layer, signal path |
| **Source** | Which input channels feed a path, and whether it is *Stereo* or *Mono*. | routes, layer 1, mono sum |
| **Path outputs** | Whether a path's L and R play at all (*L*, *R*), before any output. | OUT 1 / OUT 2 (for this) |
| **Chain** | A path's stages, in the order the sound passes them. | signal chain, rack |
| **Stage** | One step of a chain: *Pre EQ*, *Amp*, an effect, *Cab*, *Gate*, *Post EQ*. | slot, block |
| **Effect** | A VST3 or CLAP plugin placed in a chain. | insert, plug |
| **Plugin** | The installed VST3 or CLAP file: the format, the scan, the plugin's own window. | (kept) |
| **Editor** | The window a plugin draws itself. | plugin interface, own editor |
| **Controls** | RigWerk's list of an effect's parameters. | parameters, plugin controls |
| **Wet** | How much of an effect's output is mixed with the dry signal, 0–100 %. | mix (for an effect) |
| **Bypass** | A stage passes the sound unchanged and keeps its latency.  *Bypass all* does that for every effect on a path. | BYP, FX |
| **Master** | The sum of all paths, with one level and one mute, before the outputs. | Output stage, MIX, master mix |
| **Level** | A fader's value in dB: path, master, output, and a path's level on one output. | volume, LEVEL HERE |
| **Channel map** | Which path channels play on which output channels. | routing (noun), layer 2, map |
| **Map preset** | A button that fills one path's channel map: *Out 1/2*, *Out 3/4*, *Out 1/2 + 3/4*, *Mono to all*, *Swap L/R*, *Clear*. | Stereo (for this), preset (alone) |
| **Channels nothing plays** | What an output's unmapped channels play: *Mono copy* or *Silent*. | Unused channels |
| **Align** | Every path on one output waits for the slowest path there. | alignment layer |
| **Trim** | A per-path delay on one output, ±5 ms, on top of the alignment. | (kept) |
| **Polarity** | Flips a path's sign on one output. | phase |
| **Buffer size** | Samples a driver hands over per callback: the input's, the clock output's, an extra output's, the PipeWire graph's. | block, period, step, quantum, chunk |
| **Bridge buffer** | Audio RigWerk holds between two clocks so neither side runs dry.  In ms.  Modes: *Auto*, *Minimum*, *Fixed*. | cushion, ring, ring fill, safety buffer |
| **Output mode** | How the clock output opens its device.  Linux: *Standard* or *Low latency*.  Windows WASAPI: *Shared*, *Shared, smallest buffer*, *Exclusive*.  ASIO, DirectSound and MME have none. | REAL-style, REAL |
| **Dropout** | An audible gap: an output or a bridge buffer ran dry. | underrun, xrun, drop, ran dry |
| **Lost samples** | Audio a take could not write in time. | overrun |
| **Drift** | How far two clocks run apart, in ppm.  *Drift correction* holds it. | (kept) |
| **Latency** | Time from the input to an output.  Always *estimated* or *measured*. | PATH, HEARD, US · AFTER, budget |
| **Measure latency** | Plays a test click through a loopback and times its return. | calibration |
| **Test click** | The sound *Measure latency* plays. | (click, alone) |
| **Metronome** | Tempo, rhythm and level.  Its sound is the *metronome click*.  Each output picks the channels it plays on. | beat, click (alone) |
| **Limiter** | The safety limiter on every output, holding −1 dBFS.  Its meter shows *reduction*. | (kept) |
| **DSP load** | The share of the buffer time the chains take, per path and in total. | DSP time, % of quantum |
| **Session** | A saved setup: devices, paths, effects, outputs. | patch, preset |
| **Layout** | A starting arrangement of paths and channel maps, applied to the current setup: *Single path*, *Dry L / Wet R*, *Blend clean*, *Dual amp*, *Per-output mixes*. | START FROM, card |
| **Take** | One recording file.  It is named after the session. | (kept) |
| **Demo device** | A built-in stand-in device used without hardware. | preview |
| **Issue** | Something that needs attention: an *error* (something is broken) or a *warning*. | (kept) |
| **Driver panel** | An ASIO driver's own settings window. | Panel |
| **Sheet** | The editor that slides up from the bottom of the console. | inspector |
| **Drawer** | The list that slides in from the right: the effect browser and the issues. | panel |

### 2.2 Units and formats

- Levels: `−4.0 dB`, one decimal, the real minus sign `−`.  Unity reads
  `0.0` on faders.  Off is `−∞`.
- Times: one decimal below 10 ms (`1.3 ms`), none from 10 ms (`13 ms`);
  trims two decimals (`+0.25 ms`).
- Buffer sizes: `64 samples` in settings, `64 smp` in the status bar.
- Rates: `48 kHz`, `44.1 kHz` (`rate_label` in `app/settings` prints
  `48.0 kHz` today and changes).  Drift `+0.4 ppm`.  DSP load `61 %`.
- Sentence case everywhere.  Upper case only for channel numbers inside a
  grid header.

### 2.3 Examples

| Today | After |
|---|---|
| Cushion: Safe. | Bridge buffer: Auto. |
| Manual cushion set to 6.0 ms. | Bridge buffer fixed at 6.0 ms. |
| A Safe cushion grows after each dropout. | An Auto bridge buffer grows after each dropout. |
| Follower · plays the finished mix on its own clock. | Extra output: plays the master on its own clock. |
| Same graph clock as the clock device · no ring or cushion. | In the PipeWire graph: no bridge buffer needed. |
| Published · RigWerk Out · PipeWire source | RigWerk Out is on.  Other apps can record it. |
| PipeWire node · no ring | One clock: no bridge buffer needed. |
| The chain overloads the graph quantum | The paths need 82 % of the 64-sample buffer |
| 1 underrun | 1 dropout |
| REAL-style · 144 smp · 3.0 ms | Shared, smallest buffer · 144 samples · 3.0 ms |
| CURRENT PATCH / Save as new patch | Session / Save as new session |
| Plugins · 2 / BYP | Effects 2 / Bypass all |
| mix 100% · 240 smp | Wet 100 % · 5.0 ms |
| Click on 3/4 | Metronome on 3/4 |
| Record the click | Record the metronome and test clicks |

### 2.4 Code renames

The code follows the glossary.  Each rename is its own mechanical PR.

Saved files do not change.  A renamed field keeps its stored key with
`#[serde(rename = "…")]`, and so does a renamed enum variant that is stored
by name (`WasapiOutputMode::RealStyle`).  String values that are stored
stay as they are: `MeasuredLatency.output_mode` keeps `"real-style"` and
`"low-latency"`.  `StoredCounts` (`app/settings/preferences.rs`) mirrors the
stored keys and keeps them.  Old and new builds read each other's
preferences, and `PREFERENCES_SCHEMA` stays.

| Today | After | Notes |
|---|---|---|
| `cushion`, `CushionMode::{Safe, LowLatency, Manual}` | `bridge_buffer`, `BufferMode::{Auto, Minimum, Fixed}` | 42 files |
| `follower`, `FollowerPref`, `audio::follower` | `extra_output`, `ExtraOutputPref`, `audio::extra_output` | 73 files |
| `clock_device` | `clock_output` | 27 files |
| `insert`, `MAX_INSERTS` (the chain item) | `effect`, `MAX_EFFECTS` | about 75 files; leave `Vec::insert` and map inserts |
| `Preset`, `presets.rs` (the saved setup), "patch" | `Session`, `sessions.rs` | see the collision below |
| tuner `PRESETS` (tunings) | `TUNINGS` | 10 files |
| `underrun(s)` (counted for people) | `dropout(s)` | 20 files |
| `publish`, `virtual_out` (virtual output on/off) | `virtual_output` | not `publish_engine`, `publish_dsp` or "published views" |
| `app::cards`, `Card` | `app::layouts`, `PathLayout` | about 11 files; not `Layout`, which clashes with `egui::Layout` |

Kept on purpose:
- `MapPreset` stays: a map preset is not a session.
- `Ring` (`ring.rs`) stays: it is the data structure, not the concept.
- `quantum` stays inside `audio/linux`.

Collision: AGENTS.md and the engine already use *session* for one run of
the bridge (`Shared` per `AudioEngine::start`, `issue_state.new_session`).
That internal sense becomes *run* (`new_run`) in the same PR as the
`Session` rename, and AGENTS.md says so.

## 3. Screens

Three screens and two overlays.  No sidebar.

```
┌ Title bar ──────────────────────────────────────────────────────────────┐
│ RigWerk │ Session ▾  ·  ● Running [Stop]  ≈ 7.6 ms  ·  ● Rec  ♩ 96 4/4  Tuner │ Setup  Mute │
├─────────────────────────────────────────────────────────────────────────┤
│ (Error banner, only while an error is open)                               │
├─────────────────────────────────────────────────────────────────────────┤
│   Screen: Console | Tuner | Setup                ┌ Drawer (right) ┐     │
│   ┌ Sheet (bottom, over the console) ┐                                   │
├─────────────────────────────────────────────────────────────────────────┤
│ Status bar                                                                │
└─────────────────────────────────────────────────────────────────────────┘
```

### 3.1 Title bar (52 pt)

Left to right:

- **Logo** and *RigWerk*.  A click opens *About* (today's text, including
  *preview build*).
- **Session menu**: the session name, then *edited* when the setup differs
  from the saved session of that name.  Actions, in `app/sessions.rs`
  (today's `presets.rs` logic, which matches sessions by name):
  - the list of saved sessions with rates, buffer and estimated latency.
    Picking one loads it, which replaces the setup and reopens the devices.
    While the current setup is *edited*, loading needs a second click
    (*Load and lose changes*);
  - *Save* (Ctrl+S): update the saved session with the current name, or add
    it if there is none;
  - *Save as new session* (Ctrl+Shift+S): an inline name field, then add;
  - *Rename* *(new)*: an inline field; renames the saved entry and the
    current name;
  - *Start new session*: today's *Start from blank*.  It names the current
    setup *Untitled session* and changes nothing else;
  - per session *Delete*, with a two-click confirm.
  - *edited* *(new)* compares the fields a session stores (devices, rates,
    buffers, paths, effects, outputs, metronome); `RigWerkApp::session_edited`.
  - The session name also names takes (`app/record.rs`).
- **Bridge state**: a dot and *Stopped*, *Starting…*, *Running*, *Demo*
  (demo devices, today's *Preview active*), *Stopping…* or *Error*, then
  *Start* or *Stop*.  The accent is used only while running.
- **Latency**: `≈ 7.6 ms`, the estimate defined in §3.5.  Hover lists each
  path on the clock output, and the measured latency when there is one.
- **Rec**: start and stop a take, with the elapsed time and lost samples.
  The arrow opens: what is recorded (the clock output, or on Windows the
  virtual output when there is one; automatic, as today:
  `app::record::record_source`), *Record the metronome and test clicks*,
  and the last take's path, selectable to copy.
- **Metronome**: tempo, rhythm, and the beat number with the downbeat in the
  accent while it runs (today's BEAT).  A click starts or stops it; the arrow
  opens tempo (40–240 BPM), rhythm and level.
- **Tuner** (Ctrl+T).
- **Setup** (Ctrl+,).
- **Mute**: mutes the master (Ctrl+Shift+M).  While muted it is filled with
  the accent and reads *Muted*.

### 3.2 Status bar (30 pt)

Mono, left to right:

- drivers and rates, input then clock output: `ASIO → WASAPI · 48 → 44.1
  kHz`, or `PipeWire · 48 kHz` when they are the same;
- buffer sizes: `64 / 128 smp`, or one value when equal;
- `Estimated 7.6 ms`, `Measured 8.9 ms` (*not measured*, or *stale* with
  the reason on hover);
- drift;
- dropouts: the total, each output on hover;
- total DSP load;
- for five seconds after it arrives, the newest log line (today's *RECENT
  ACTIVITY*);
- on the right, the issue count (*2 warnings*, *1 error*) and *Log ▴*.

The issue count opens the issues drawer (§6.4).  *Log* opens the
diagnostics sheet (§6.5).  Clicking the drivers, rates, buffers or latency
opens Setup at that section.  The status bar stays on every screen.

### 3.3 Console

The home screen (boards 1, 6).  Groups in a row, each with a small label:
**Input**, **Paths · n of 4**, **Master**, **Outputs**.  Strips inside a
group are separated by 1 pt lines.  A dashed *add* column (32 pt) follows
Paths and Outputs.  At the limit it stays, disabled, with the reason on
hover (*Four paths is the most*, *Four extra outputs is the most*, *A
removed path is still fading out*).

Every strip ends in the same row: meter and fader, value, then *M* (and *S*
on paths), so the faders line up across the screen.

**Input strip**

- Device name, driver and channel count.  A click opens Setup › Input.
- *In 1* / *In 2* toggles.
- Scope.  A click opens the scope sheet: input and output overlaid, the
  peaks, and the sample count (today's Live scope).
- *Driver panel* when the input is ASIO.  Disabled until the bridge runs on
  that driver, with that reason on hover.
- Input gain fader, stereo meter.

**Path strip** (one per path, in path order)

- A 3 pt cap in the path colour, then the name.  An issue marker follows
  the name when an issue points at this path.  The name's menu: *Rename*
  (names are unique), *Colour ▸*, *Duplicate (muted)*, *Apply layout ▸*,
  *Remove* (two-click confirm: it drops the path's effects).
- *Source* button: `In 1 · mono`.  It opens the source popover: a 2 × 2
  grid (In 1, In 2 → L, R) and *Stereo* / *Mono*.  When the path outputs
  are not both on, the button adds `· L only` or `· R only`.
- The chain (§4.2), in signal order: *Pre EQ*, *Gate* when it sits before
  the effects, *Amp* when it sits before the effects, the effects group,
  *Amp* when after, *Cab*, *Gate* when after, *Post EQ*.
- The chain's latency and the path's DSP load.  The DSP alarm is raised on
  the total (as today, `app::issues`); the strip of the path with the
  largest share carries its marker.
- Level fader, M, S.  A muted path dims its fader and lights M.  While any
  path is soloed, the others dim.

**Master strip**: *Master*, level fader, M.

**Output strip** (clock output first, then extra outputs in their order)

- Name, and a badge: *Clock*, *Extra*, *In graph* (a PipeWire extra
  output), *Virtual*, or in amber *Missing · retrying*, *Starting*,
  *Error*.
- *Plays*: one dot per path, filled when any of its channels play here.
  On an empty map a click applies *Out 1/2*.  On a plain *Out 1/2* map it
  clears it.  On any other map it opens the output sheet, so a custom map
  is never lost to one click.
- *Metronome* toggle.  It changes only the output's click channels: on
  restores the channels last used *(new: a `last_click_mask`,
  `#[serde(default)]`)*, or channels 3 and 4 on a device with four or more,
  or 1 and 2.  Native PipeWire extra outputs take at most two click
  channels (`app/output_devices.rs`).
- Buffer: on the clock output, the output mode and its real buffer time; on
  an extra output, the bridge buffer (`auto 5.3 ms`), or *In graph*.
- Limiter reduction bar.
- Short-term loudness in LUFS, and the learned trim when it is not zero.
  Learn and Match are one-shot; nothing moves on its own.
- One meter bar per device channel (2–8; thinner when there are more), the
  level fader, M.
- Clicking the strip anywhere else opens its output sheet (§6.2).

### 3.4 Tuner

A full screen (board 4), opened with *Tuner* or Ctrl+T and left with Esc,
*Tuner* or Ctrl+T.  The title bar and the status bar stay.  Everything that
keys on `Page::Tuner` today (`app/tuner.rs`, `app/bridge.rs`: analysis and
auto-mute) keys on `Screen::Tuner`.

- While auto-mute holds the outputs, a strip under the title bar says
  *Outputs and the metronome are muted while you tune* (today's *Output
  muted by tuner*).
- The note, very large, its octave, the target and the heard frequency.
- The ±50 cent needle with the in-tune band and its centre, or the strobe:
  three discs seen from the front, sliding sideways (flat to the left,
  sharp to the right).  In All strings either gauge is one face per
  string across the row, taken from the window this frame.  A strobe face
  is scaled by its width over the row, so the discs keep the full strobe's
  bar count.  A needle's arc is fitted to the column.  Each face heads
  with its string and its cents.
- Cents and the state: *tune up*, *tune down*, *in tune*, *centered*
  (within the centre band).
- With a tuning: one cell per string with its note, target Hz, cents, a
  mark and the state, and *4 / 6 in tune*.  The string the face is
  following is underlined in its pitch colour.  A pinned string has an
  accent border.  The harmonic marker of today's rows is a small
  *H* on the cell.  Chromatic shows *Pick a tuning to see each string*.
- Level and confidence: a thin bar under the scale.  The readout is a
  fixed column, right aligned, wide enough for a signed `xxx.x` level.
- Bottom bar: *Tuning* (Chromatic and the 20 tunings, grouped Guitar,
  Extended, Bass), *A4* (−/+), *In tune within* (±3, ±5, ±10), *Input*
  (L, R, Sum), *Listen* (Single note, All strings), *Mute while tuning*
  (switch), *Needle* / *Strobe*.
- When the bridge is stopped: *Start the bridge to tune* with *Start*.

### 3.5 Setup

One scrolling screen, two columns (board 5).  *Done* returns to the
console.

Left: devices.

- **Input**: one device list grouped *Direct* (ASIO, PipeWire, ALSA hw:)
  and *System* (WASAPI, DirectSound, MME, PulseAudio).  Always-visible
  *Search* above the list (substring, or `*` wildcards on label and
  backend).  Denser single-line rows (~30 px) show driver, channels,
  *In use*, *Demo device*.  After 5 rows the list scrolls in a fixed
  viewport (so the page scroll cannot crush it); the *In use* row stays
  pinned above the scroll.  Picking an ASIO row sets
  `use_asio_input` and `asio_id`; any other sets `input_id` (today's two
  remembered inputs keep working).  Then *Rate* and *Buffer size*, the
  driver line (what the driver really runs, why it differs, hover for
  detail), and *Driver panel* for ASIO.
- **Clock output**: the same list (Search, denser rows, nested scroll,
  pinned *In use*), *Rate* (*Match input* or a rate), *Buffer size*,
  *Output mode* (§2.1; disabled with the reason for ASIO, DirectSound
  and MME), the driver line, and *Check driver* while it has not been
  checked.  On Windows the ASIO4ALL conflict warning shows here.
- *Rescan devices*, the PipeWire graph line (its buffer size, and whether
  the ALSA buffer size fits it), and the platform's driver note.

Right: time and preferences.

- **Latency**: the estimate as a large number and a stacked bar of its
  parts: *In*, *Bridge buffer*, *Chain*, *Out*, *After RigWerk* (the graph
  delay after RigWerk's node), *Alignment*.  The estimate is for the clock
  output: the slowest audible path routed there, including that output's
  alignment (`RigWerkApp::heard_estimate`, replacing the selected-path
  estimate in `app/budget.rs`).  Under it, the measured latency with its
  rates, buffers, mode and age, *stale* with the reason, and *Measure
  latency…* (§6.6).
- **Clock**: drift, *Drift correction* on/off, the bridge buffer (*Auto* /
  *Minimum* / *Fixed*, the size when fixed, the live fill), and dropouts.
  With one shared clock: *One clock: no bridge buffer needed*.
- **Start**: *Start the bridge when RigWerk opens*.
- **Appearance** *(new)*: *Theme* (System, Dark, Light) and *Size*
  (80–150 %, default 100 %).
- **Effects**: *Double-click an effect opens* (*Editor* / *Controls*), the
  plugin folders that are scanned, *Rescan plugins*.
- **Recordings** *(new)*: the folder, as a text field (today it is only a
  preference).
- **Help**: the setup guide for this platform (today's Guide page, as
  collapsible sections) and *About*.

## 4. Components

### 4.1 Fader and meter

- Meter bars of 5 pt (2–3 pt when a strip shows more than two channels), a
  scale (0, 6, 12, 24, 48), a fader cap of 24 × 12 pt on a 3 pt rail, and a
  unity tick wider than the cap.
- One taper on every fader: unity at 75 % of the travel whatever the range,
  linear above it, bent below it so the travel near unity is finer than
  near the floor.
- Meter colours: `meter` up to −6 dBFS, `meter_hot` to −1, `error` above.
  Peak hold 1.5 s as a 2 pt line.  A clip latches red until clicked.
- Drag 0.1 dB per pt near unity, Shift for fine, double-click for 0.0,
  scroll ±0.5 dB.  Double-click the value to type one.
- Fader height: `clamp(strip_height × 0.3, 140, 300)` pt.

### 4.2 Chain rows

- 28 pt high, 3 pt corners, `slot` fill.  An LED dot (filled: on; ring:
  bypassed), the name, then a mini curve (EQ), the threshold (Gate) or the
  latency (effect).
- Selected: accent outline and `accent_bg`.  Failed to load: the name in
  `error_text` with an icon, *Didn't load* and the reason on hover.  Paused
  while restarting: a pause glyph.
- Click selects and opens its sheet.  Click the LED to bypass.
  Double-click an effect opens its editor or its controls (Setup).
  Right-click: *Editor*, *Controls*, *Bypass*, *Move up*, *Move down*,
  *Remove*.
- Drag a row to reorder inside its chain; the list scrolls at its edges.
  Alt+↑/↓ moves the focused row.  Moving an effect closes its open editor,
  as `move_plugin` does today; the hover on the drag handle says so.
- The effects header: *Effects n*, the group's latency, and *Bypass all*
  (lit with the accent while on).
- *+ Add effect* (dashed) is the last row.  It opens the browser for that
  position.  At 32 effects it is disabled with *32 effects is the most*.

### 4.3 Sheet

A bottom sheet over the console, for one thing at a time (boards 2, 7).

- Height 40 % of the console, at least 300 pt, resizable by its top edge.
  The height is a preference (`sheet_height`, `#[serde(default)]`, clamped
  in `sanitize_preferences`).
- Header: the path dot and name, `/`, the item's name, its switches, *‹
  previous* and *next ›* through the chain, *×*.
- It covers the lower part of the strips, faders included.  While it is
  open each strip shows a 4 pt horizontal meter under its name, and the
  sheet holds the level of what it edits (path sheet: path level; output
  sheet: output level).  The title bar and status bar keep working.
- Esc closes it.  Selecting another stage swaps its content.

### 4.4 Drawer

A right-side drawer, 460 pt, with a 38 % scrim over the rest (board 3):
the effect browser and the issues.  Esc or a click on the scrim closes it.

### 4.5 Controls

- Buttons 32 pt high (28 inside sheets), 4 pt corners: *outline*
  (default), *ghost* (title bar), *primary* (one per screen: *Done*,
  *Measure*), *danger* (only the second click of a confirm).
- Segmented controls for 2–4 exclusive choices; toggle chips for
  independent switches.
- Dropdowns show the value and a chevron; from 8 items they have a filter.
- Hover text on every control whose label does not say everything.  This
  is where today's explanations go, shortened.
- Every control has an AccessKit label matching its visible text, or
  saying what an icon does (*Mute master*).  The UI tests find controls by
  these labels, and screen readers read them: eframe's `accesskit`
  feature is on (AT-SPI on Linux, UI Automation on Windows).

## 5. Scaling

All sizes are in egui points.  With *Size* at 100 % and no OS scaling, a
point is a pixel.  At 125 % OS scaling a 1920 × 1080 screen is 1536 × 864
points; at 150 % it is 1280 × 720.

### 5.1 Limits it must hold

| Thing | Most | Source |
|---|---|---|
| Paths | 4 | `audio::path::MAX_PATHS` |
| Effects per path | 32 | `plugins::chain::MAX_INSERTS` |
| Outputs | clock + 4 extra (one may be virtual) | `output_map::MAX_FOLLOWERS` |
| Channels per output | 8 | `output_map::MAX_OUTPUT_CHANNELS` (and the `u8` click mask) |
| Input channels | 2 | `input_channels: [bool; 2]` |
| Installed plugins | unbounded (1000+) | scan |
| Sessions, issues | unbounded | preferences, `app::issues` |

### 5.2 Console width: density steps

The console takes the widest step that fits the window, in this order:

| Step | Input | Path | Master | Output | What changes |
|---|---|---|---|---|---|
| 1 Normal | 188 | 224 | 112 | 160 | everything shown |
| 2 Compact outputs | 188 | 224 | 112 | 96 | outputs lose the subtitle; *Plays* becomes dots |
| 3 Narrow paths | 132 | 172 | 88 | 96 | chain rows lose the mini curve; names get an ellipsis |
| 4 Mini outputs | 132 | 172 | 88 | 64 | outputs lose the dB scale, buffer and limiter bar, and the badge becomes an icon; meter, fader, value, *Plays* dots and M stay |
| 5 Scroll | 132 | 172 | 88 | 64 | the paths group scrolls sideways; Input stays left, Master and Outputs stay right |

Everything a step hides stays on hover and in the output sheet.  In step 5
the *Paths* label shows a dot per path; a click scrolls that path into
view, and selecting a stage in a hidden path scrolls too.

Width needed = 24 (window padding) + 36 (group gaps) + 80 (two add columns
and their gaps) + input + paths + master + outputs, where n strips of
width w take n × w + (n − 1).  At step 5 the visible path count is
(visible width + 1) / (path width + 1), rounded down to one decimal.

| Setup | Step 1 | Step 2 | Step 3 | Step 4 | At 1440 | At 1280 | At 1040 |
|---|---|---|---|---|---|---|---|
| 1 path, clock output | 824 | 760 | 628 | 596 | 1 | 1 | 1 |
| 2 paths, 3 outputs (README set) | 1371 | 1179 | 995 | 899 | 1 | 2 | 3 |
| 4 paths, 4 outputs (board 6) | 1982 | 1726 | 1438 | 1310 | 3 | 5 (3.8 paths) | 5 (2.4 paths) |
| 4 paths, 5 outputs | 2143 | 1823 | 1535 | 1375 | 4 | 5 (3.4 paths) | 5 (2.0 paths) |

`ui/pages/console/density.rs` is a pure function from the available width
and the strip counts to the step and the scroll width.  Its tests are this
table.

### 5.3 Chain height: 32 effects

- *Pre EQ*, either *Gate* and *Post EQ* are pinned; only the effects group
  scrolls.
- The group gets the height left after the fixed rows and the fader: six
  rows at 1440 × 900, three at 1040 × 700.
- On overflow: a 3 pt scroll thumb, a fade at the bottom edge, *n more ·
  scroll* in the fade.  The wheel scrolls the group, not the console.
- The selected effect, and one being dragged, stays in view.
- Each path's group scrolls on its own, so the faders stay on one line.

### 5.4 Other lists

- **Effect browser**: a virtual list (`ScrollArea::show_rows`, rows of a
  fixed 52 pt), so 1000 plugins cost the same as 10.  Search matches name,
  maker, format and category, ranked by prefix match, then favourites, then
  recent use.
- **Output sheet channel grid**: one column per output channel (at most 8),
  one row per path channel (at most 8) and one for the metronome.
- **Session menu**: scrolls after 12 sessions, with a filter from 8.
- **Issues drawer**: scrolls.

## 6. Sheets and drawers

### 6.1 Stage and path sheets

**EQ** (Pre EQ and Post EQ, board 2): the curve with its five band handles
(low shelf, three bells, high shelf) and the band table (type, frequency,
gain, Q, on).  Header: *On* / *Bypass*, *Flat*.  Drag a handle for
frequency and gain, scroll for Q, double-click to reset a band.  Gain ±12
dB in 0.1 dB steps, Q 0.3–6.  The hover texts of today's EQ card (what the
pre and post EQ are for, shelf vs bell Q) stay.

**Gate**: threshold, range, hysteresis, attack, release and hold, each with
its value and today's hover text; a reduction meter (audio addition, §0);
*Before effects* / *After effects*; *Listens to*: *Gated signal* /
*Dry input*; *Reset* (keeps on/off).

**Effect**: header *On* / *Bypass*, *Wet* (its default on hover), latency,
state (*Active*, *Bypassed*, *Didn't load*: today's controls-window marks),
*Editor*, *Reset to defaults*, *Remove*.  Body: the controls with *Find a
control…*, or *This effect draws its own controls*, *reports no controls*,
*did not load*.  The errors the plugin reported are listed here.

**Path**: name, colour, source (*Input* / *Sends*; for the input the 2 × 2
grid and *Stereo* / *Mono*), the sends (*Send to* each return, or on a
return *Send from* each path, *Off* at the bottom), *Path
outputs* L and R *(the chips that were OUT 1 / OUT 2 on Live)*, level,
mute, solo (not on a return), *Duplicate (muted)*, *Apply layout ▸* with *Custom · edited
from Blend clean* when the setup no longer matches one, *Remove*, and on
every output it plays on its latency and a *Match* button that matches the
path to that output's loudness target.

### 6.2 Output sheet

Board 7.

- Header: name, badge, channel count, driver; *Driver panel* (ASIO);
  *Remove* (extra outputs; the clock output is changed in Setup).
- Per-channel meters, level and M for this output.
- Loudness: short-term, momentary and integrated LUFS, the target slider,
  *Learn*, the learned trim, *Clear* and *Reset*.  *Learn* measures the
  output for eight seconds and saves a trim so it reaches the target with
  the level fader at 0 dB; nothing moves on its own.
- The channel map grid.  Under it, the map presets this output offers,
  applied to the path chosen beside them (today's `MapPreset::for_channels`:
  mono devices get *Mono to all* and *Clear*; 2–3 channels add *Out 1/2*
  and *Swap L/R*; 4 or more add *Out 3/4* and *Out 1/2 + 3/4*), and
  *Metronome on 3/4*, which maps every path to 1/2, the metronome to 3/4
  and *Channels nothing plays* to *Silent*, as today.
- *Channels nothing plays*: *Mono copy* / *Silent*.  A mono device plays
  (L + R) / 2 of whatever is mapped.
- **Paths here**: per path, level here, polarity, trim, and when it arrives
  (*slowest*, *waits 5.5 ms*, *past the limit · plays late*).  *Align paths*
  On / Off with its summary (*Every path here waits for Drive*).  The
  comb-filter and alignment-limit warnings show here and are issues
  anchored to this output.
- Extra outputs: *Bridge buffer* (*Auto* / *Minimum* / *Fixed* and size, or
  *In graph*), its rate, its driver buffer size, the fill, its state and
  dropouts.
- Virtual output: *On* / *Off*; on Windows the VB-Cable state, with *Install
  VB-Audio Virtual Cable, then rescan* when it is missing.

**Adding an output** (the add column) opens a device list with today's
rules (`ui/pages/mixer/output_devices`):
- PipeWire clock output: PipeWire sinks only, plus *RigWerk Out*.
- ALSA or another Linux clock: the generic devices.
- Windows: WASAPI devices, with VB-Cable under *Virtual*, and the ASIO
  conflict warning when the clock output is ASIO.
- Elsewhere: *Extra outputs are not available on this platform*.
- Devices already added are shown disabled (*already added*).

### 6.3 Effect browser

Board 3.  Opens from *+ Add effect* (the position is that row) or from an
empty chain.

- Header: *Add to Room*, *after Limiter Stereo · change*.
- Search field, focused on open.  Category chips: *All*, *Favourites*,
  *Recent*, then the categories the scan reports.  A format filter (*VST3*,
  *CLAP*).
- Rows: name (the match in semibold), maker · format · channels *(new data,
  §0)*, the paths that already use it (*in Room, Drive*), a star for
  favourites, and the scan error when there is one.
- ↑/↓ chooses, Enter adds, Shift+Enter adds and opens its editor.  The
  drawer stays open, the position moved after the new effect, so several
  can be added in a row.
- A notice line above the footer shows what happened: added, could not add
  and why, a bad path, a bundle with more classes (today's
  `plugin_browser.notice`).
- Footer: *Add from path* (today's text field), *Rescan*.
- *(new)* `effect_favourites` and `effect_recent` preferences
  (`#[serde(default)]`; `sanitize_preferences` drops duplicates and keeps
  the last 10 recent).

### 6.4 Issues drawer

Today's Live issues, one row each: severity, title, one line of detail, the
action (*Use Auto buffer*, *Set buffer size to 256*, *Turn Low latency
off*, *Check driver*, …), *Show*, and *Dismiss* for the warnings that allow
it (not the limiter warning, as today).  Filter: *All*, *Warnings*,
*Errors*.  *Show dismissed* at the bottom.

- *Show* closes the drawer and selects the issue's anchor, which also
  carries the marker: `Anchor::{Input, Path(slot), Stage(slot, stage),
  Master, Output(id), Setup(section)}` replaces today's Live card.  The
  navigation actions map to anchors: `OpenRouting` → `Output(id)`,
  `OpenDevices` → `Setup(Devices)`, `ShowFollower` and `ShowOutput` →
  `Output(id)`.
- A new issue makes the count pulse once.  A new error also shows the
  error banner under the title bar, which stays until the error clears or
  is dismissed.  Today's *Audio device disconnected* modal becomes this
  banner, with *Rescan* and *Open Setup*.

### 6.5 Diagnostics sheet

From *Log*.  Tabs:

- *Log*: newest first; *Copy* and *Export* write it oldest first.
- *Performance*: DSP load per path and in total (average and peak against
  the buffer time); per path and per effect, how many buffers were skipped
  or paused out of how many were tried; *Reset*.
- *Latency*: the estimate's parts and the measurement's conditions.
- *Scan*: device and plugin scan notes, restore problems.
- *Status*: engine state, last error.

### 6.6 Measure latency

A small window, as today: *Cable* / *Microphone*; the clock output's mix is
muted while it measures, the test click ignores that output's mute and
level, extra outputs keep playing; progress; then the result with
resolution and conditions, and *Measure again*.

## 7. Theme

### 7.1 Tokens

Both themes define the same tokens.  `ui/theme.rs` holds two `Palette`
values; widgets read the active one, never a literal colour.

| Token | Dark | Light | Use |
|---|---|---|---|
| `bg` | `#121211` | `#e9e6df` | console well, window |
| `bar` | `#0d0d0c` | `#f4f2ed` | title bar, status bar |
| `strip` | `#1a1918` | `#fbfaf7` | strips, setup panels |
| `slot` | `#222120` | `#efede8` | chain rows, buttons |
| `well` | `#0f0f0e` | `#e4e1da` | meter tracks, scope; never text |
| `sheet` | `#1c1b19` | `#fdfcfa` | sheets, drawers |
| `line` | `#2a2825` | `#dcd8cf` | separators |
| `line2` | `#3a3733` | `#c9c4b9` | control borders |
| `text` | `#ebe7df` | `#1d1c1a` | primary text |
| `text2` | `#b0aa9f` | `#55514a` | secondary text |
| `text3` | `#948e83` | `#6b665d` | labels, hints |
| `led_off` | `#7d776e` | `#857f73` | bypassed LED ring, off states |
| `accent` | `#ff8f40` | `#c55a0c` | engaged states, outlines and fills |
| `accent_text` | `#ffb27a` | `#a8480a` | accent as text |
| `accent_bg` | `#2a1a0e` | `#fbeee2` | selected fill |
| `warn` | `#ffcc4d` | `#7a5700` | warnings, with an icon |
| `error` | `#ff5a4a` | `#d93a2b` | clip, recording, error icons |
| `error_text` | `#ff5a4a` | `#b8291c` | error text |
| `meter` | `#cfc8bb` | `#4a463f` | meter below −6 dBFS |
| `meter_hot` | = `warn` | = `warn` | meter −6 to −1 dBFS, limiter reduction |
| `path[0..4]` | `#7fb2d6 #c49bd8 #9cc48a #e39aa6` | `#3f7fae #8d5bb0 #5f8f4a #b5566a` | path identity |

- Path colours are *Blue*, *Violet*, *Green*, *Rose*.  Today they are Teal,
  Violet, Amber, Pink (`ui/theme.rs`, names in `path_stage.rs`).  The saved
  index stays.  Amber stops being a path colour because amber means warning.
- `theme/tests.rs` checks, in both themes:
  - text tokens (`text`, `text2`, `text3`, `accent_text`, `warn`,
    `error_text`) against `bg`, `bar`, `strip`, `sheet` and `slot` at
    4.5:1.  The lowest pair is light `text3` on `bg`, 4.57:1;
  - state colours (`accent`, `led_off`, `meter`, `meter_hot`, paths)
    against `strip` and `slot` at 3:1.  The lowest is the light green path
    colour on `slot`, 3.25:1.
- Control borders (`line2`) are below 3:1 on purpose: buttons are
  recognised by their label and fill, not the border.

### 7.2 Theme and size

- New preferences, each `#[serde(default)]` and checked in
  `sanitize_preferences`:
  - `theme`: *System* (default), *Dark* or *Light*;
  - `ui_size`: 0.8–1.5, default 1.0.
- RigWerk's preference wins over egui's own saved memory: at start the app
  sets `ctx.set_theme` and `ctx.set_zoom_factor` from it.
- *System* uses `ThemePreference::System`, with dark as the fallback when
  the platform reports nothing.
- Both egui styles are set once (`set_style_of`), so switching is instant
  and never restarts the bridge.
- `zoom_with_keyboard` is turned off; Ctrl++, Ctrl+− and Ctrl+0 change
  `ui_size` instead.
- `FONT_SCALE` (1.2, text only) goes.  The type scale below is already
  sized for reading, so the old pages are not smaller after step 2.
- Plugin editors keep their own look.

### 7.3 Type

- **IBM Plex Sans** (400, 500, 600) and **IBM Plex Mono** (400, 500),
  SIL OFL 1.1.  Roboto and Roboto Mono are OFL 1.1 too (Google Fonts moved
  them off Apache 2.0), so the licence is the same either way.  Plex is
  chosen because Roboto reads as Android's default.
- Licence: the fonts are data shipped beside the program.  egui's
  `default_fonts`, which RigWerk embeds today, are already under
  `(MIT OR Apache-2.0) AND OFL-1.1 AND Ubuntu-font-1.0`.  `assets/fonts/`
  holds the TTFs, `OFL.txt` and a note on source and version; the README's
  licence section names them.
- Embedded with `include_bytes!` and subset to Latin, Latin-1, General
  Punctuation, arrows and math operators (− · → ↑ ↓ ⇧ ± ≈ Ø ∞ …).  The
  size per face is measured when subsetting; the target is under 80 KB.
  egui's default fonts stay as the fallback.
- Static weights, each a named family (`sans`, `sans-medium`,
  `sans-semibold`, `mono`, `mono-medium`).  epaint 0.37 can set variable
  axes (`FontTweak::coords`), but static files are simpler and smaller after
  subsetting.
- Scale (pt at 100 %): 11 grid headers, meter scale; 12 secondary, status
  bar; 13 body, rows; 14 strip names, buttons; 15 title, sheet title; 18
  Setup headings; 22 tuner strings; 44 tuner cents; 200 tuner note.

### 7.4 Shape and space

- Corners 3 pt (rows, chips), 4 pt (buttons, groups).  No card borders.
- Spacing on a 4 pt grid: 4 inside a control, 8 between controls, 12
  between groups, 12–16 at the window edge.
- Icons are painted (`ui/icons.rs`) on a 16 pt grid with a 1.5 pt stroke.
  No emoji.

## 8. Keyboard

egui already uses Tab and the arrows to move focus, Space and Enter to press
the focused control, and Esc to clear focus.  RigWerk adds only chords,
plus Esc for its overlays:

| Key | Does |
|---|---|
| Ctrl+Shift+M | Mute or unmute the master, from any RigWerk window while no text field has focus.  Plugin editor windows are the plugin's and do not see it. |
| Ctrl+T | Tuner, and back |
| Esc | Closes the innermost: a popup, the measure window, a drawer, the sheet, then the tuner |
| Ctrl+S / Ctrl+Shift+S | Save session / Save as new session |
| Ctrl+, | Setup |
| Ctrl+F | The effect browser for the selected path |
| Ctrl++ / Ctrl+− / Ctrl+0 | Size up, down, 100 % |
| Alt+↑ / Alt+↓ | Move the focused effect |
| Ctrl+Enter | Open the focused effect's editor |
| Ctrl+Q | Quit (eframe's default, kept) |

Nothing removes on a key: there is no undo.

## 9. Feature inventory

Every control, reading and state the current UI has, and where it goes.
The rewrite is done when every row has been checked in the running app.

### Shell and frame

| Today | After |
|---|---|
| Sidebar pages; path entries with stage count and latency | Console strips; latency on each strip |
| Header: Start/Stop, state pill (incl. *Preview active*), auto-start | Title bar state (*Demo*), Start/Stop; auto-start in Setup › Start |
| *AUDIO PATH* box (both drivers) | Status bar drivers |
| Version, *about*, About dialog | Logo → About; Setup › Help |
| *Finding audio devices…* startup overlay | Same |
| *Audio device disconnected* modal | Error banner (§6.4) |
| Ctrl+Shift+M | Same (§8) |

### Live Bridge

| Today | After |
|---|---|
| Input picker, low-latency and SYS groups, ASIO vs other input | Setup › Input |
| IN 1 / IN 2 chips | Input strip |
| Input rate, buffer, driver line | Setup › Input |
| ASIO *Panel* chip, disabled until running | *Driver panel*: input strip, output sheet, Setup |
| Gain, input meters | Input strip |
| Output picker | Setup › Clock output |
| OUT 1 / OUT 2 chips (the selected path's L/R enable) | Path sheet *Path outputs*; source button shows *L only* / *R only* |
| Output mode: Linux Low latency; WASAPI Shared / REAL-style / Exclusive; ASIO direct | Setup › Clock output › *Output mode*; clock strip shows it |
| ASIO4ALL warning | Setup › Clock output |
| Output rate (*Match input*), buffer, driver line, click to check | Setup › Clock output |
| Output level, meters, *mute* chip | Master strip (level, M) and title bar *Mute*; meters on master and outputs |
| *ALSO PLAYING ON*, *+ add* | Output strips, add column |
| Bridge strip: ring fill, drift, *drift auto*, cushion mode and size, underruns, *no ring* line | Setup › Clock; status bar drift and dropouts |
| Record row: Record/Stop, time, overruns, *Record the click*, take name | Title bar *Rec* and its menu |
| Publish row | Virtual output strip and sheet |
| Chain strip, BYP, *Open mixer* | The console itself; *Bypass all* |
| Scope (in/out, peaks, samples, idle) | Input strip scope and scope sheet |
| Health tiles: PATH, DRIFT, BUFFER, UNDERRUNS, HEARD, MODE | Status bar; Setup › Latency and Clock |
| Measured loopback, *Measure…*, stale reason, measure window | Status bar, Setup › Latency, §6.6 |
| Issues strip: severity, filter, action, *Show on card*, Dismiss, show dismissed, reopen on new issue | Status bar count, issues drawer, anchors, banner (§6.4) |
| *RECENT ACTIVITY* line, *Open diagnostics* | Status bar log line, *Log* |
| BEAT number, downbeat colour | Metronome button |

### Mixer

| Today | After |
|---|---|
| Path chips, *+ Path* (Empty, Duplicate muted) | Strips; add column (*Empty path*, *Effects return*, *Duplicate ▸*, *Apply layout ▸*) |
| Path menu: rename (unique names), colour, duplicate, remove | Name menu, path sheet; remove with confirm |
| Input stage | Input strip |
| Pre EQ / Post EQ inspector | EQ sheet |
| Plugins: list, on/bypass, ↑ ↓, ×, mix %, latency, *+ add insert* | Chain rows, effect sheet |
| Group BYP | *Bypass all* |
| Open editor / Open parameters, double-click | Rows, effect sheet, Setup › Effects |
| Gate inspector, PRE/POST, reset | Gate sheet; PRE/POST also moves the row |
| Path stage: level, mute, solo, *not routed* | Path strip; *not routed* is an issue on it |
| Output stage: master level, mute, meters | Master strip |
| Metronome: start/stop, tempo, rhythm, level | Title bar metronome |
| Output devices: clock, extra, virtual, add (platform rules), remove, level, mute, state, rate, driver buffer, cushion mode and size, *Click on 3/4*, click channels, *Handled by PipeWire* | Output strips, output sheet, add-output list (§6.2) |

### Signal Routing

| Today | After |
|---|---|
| *START FROM* cards, *Custom · edited from* | *Apply layout ▸* in the add-path and path menus, path sheet |
| Signal path summary | The console |
| Input → paths matrix | Source popover, path sheet |
| Stereo / Mono sum | *Stereo* / *Mono* |
| Automatic drift correction | Setup › Clock |
| Path cards | Path strips |
| Paths → outputs: device list, map presets, matrix, unused channels, mono rule | *Plays* dots, output sheet |
| Per output: align, level here, polarity, trim, arrival, comb filter, alignment limit | Output sheet *Paths here*; warnings as issues |

### Plugins

| Today | After |
|---|---|
| Library, filter, count, rescan, scanning state | Effect browser |
| *ADD TO PATH* chips, next-add position | Browser opened from a chain position |
| *+ ADD*, *IN CHAIN*, the other paths using it | Browser rows (*in Room, Drive*) |
| Notices (added, could not add, bad path, extra classes) | Browser notice line |
| Controls window: ACTIVE / BYPASSED / PROBLEM, find, reset, bypass, editor | Effect sheet |
| *Prefer plugin interface* (per session in memory) | Setup › Effects, a new preference `effect_double_click` (not the dropped `prefer_plugin_interface` key) |
| *ADD FROM PATH* | Browser *Add from path* |

### Devices, Sessions, Diagnostics, Guide, Tuner

| Today | After |
|---|---|
| Devices: grouped lists, *IN USE*, preview, rescan, graph size, driver note | Setup › Input, Clock output |
| Sessions: list (rate, buffer, latency), current, save, update, start from blank, delete with confirm | Session menu (§3.1) |
| Diagnostics: status, engine, log, scan notes, DSP per path, chain latency, plugin block counters and reset, latency estimate and measurement, copy, export | Diagnostics sheet |
| Guide | Setup › Help |
| Tuner: tunings, A4, tolerance, input, listen, needle/strobe, auto-mute switch, muted state, n/m in tune, per-string target Hz, harmonic, centered, level, confidence, start prompt | Tuner screen (§3.4) |

### Preferences that change role

| Field | After |
|---|---|
| `page` | Maps old values: `live`, `mixer`, `routing`, `plugins`, `sessions`, `diagnostics` → `console`; `devices`, `guide` → `setup`; `tuner` → `tuner`.  An old build reading `console` falls back to Live. |
| `eq_edit`, `output_tab_low_latency` | Read and ignored; kept so older builds keep them. |
| `selected_session` | The session menu's highlighted entry. |
| `use_asio_input`, `asio_id`, `input_id` | One list in Setup sets them (§3.5). |

## 10. Code layout

Dependencies still point `ui` → `app` → `audio`.

```
src/app/
  screen.rs        Screen { Console, Tuner, Setup }, Sheet, Drawer, selection
  issues.rs        Anchor replaces the Live card (§6.4)
  layouts.rs       today's cards.rs
  sessions.rs      today's presets.rs, plus rename, edited, load confirm
  budget.rs        heard_estimate (§3.5)
src/ui/
  shell.rs         title bar, status bar, banner, sheet and drawer hosts
  frame.rs         eframe frame, modals, Ctrl+Shift+M (as today)
  theme.rs         Palette (dark, light), type scale, style setup
  fonts.rs         font loading
  menus/           session, record, metronome
  pages/console.rs (+ console/: input, path, master, output, chain, density)
  pages/tuner.rs   (keeps gauge.rs, rows.rs)
  pages/setup.rs   (+ setup/: devices, latency, clock, appearance, help)
  sheets/          eq, gate, effect, path, output, scope, diagnostics
  drawers/         browser, issues
  widgets/         fader, meter, chain_row, segmented, chip, dot, popover
```

- Which screen, sheet and drawer are open lives in `app::screen`, as the
  selected stage lives in `app::chain` today.  Pages draw; they do not own
  that state.
- `density.rs` is pure and tested (§5.2).
- AGENTS.md's Layout and UI sections change in the PR that changes the
  layout they describe: "one module per screen under `ui/pages`, one per
  sheet under `ui/sheets`, one per drawer under `ui/drawers`".

## 11. Screenshots and tests

- `RIGWERK_PAGE` takes `console`, `tuner`, `setup`.  `RIGWERK_SHEET`
  (`eq`, `gate`, `effect`, `path`, `output`, `scope`, `diagnostics`) and
  `RIGWERK_DRAWER` (`browser`, `issues`) replace `RIGWERK_MIXER_STAGE`.
  `RIGWERK_THEME` (`dark`, `light`) overrides the preference.
  `RIGWERK_SCREENSHOT_SIZE=<w>x<h>` *(new)* replaces the fixed 1440 × 1080
  (`app/screenshot.rs`).
- README set: console (2 paths), console with 4 paths, console light, EQ
  sheet, output sheet, browser, tuner, setup, at 1440 × 900; one console at
  1040 × 700.  `docs/screenshots/README.md` is rewritten to match.
- No test is deleted or weakened (AGENTS.md).  `src/ui` holds 90 tests
  today.  Each moves with its code, or to `app` where the logic it checks
  moves:

| Tests today | Count | Moves to |
|---|---|---|
| `pages/devices` | 2 | `pages/setup/devices` |
| `pages/diagnostics` | 8 | `sheets/diagnostics` |
| `pages/live/bridge_card` | 3 | `pages/setup/clock` |
| `pages/live/chain_strip` | 3 | `pages/console/chain` (the summaries) |
| `pages/live/issues` | 5 | `drawers/issues` |
| `pages/mixer/chain` | 5 | `pages/console/chain` |
| `pages/mixer/eq_card` | 3 | `sheets/eq` |
| `pages/mixer/output_devices` | 8 | `sheets/output`, `pages/console/output` |
| `pages/mixer/path_stage` | 2 | `pages/console/path` |
| `pages/plugins` | 2 | `drawers/browser`, `sheets/effect` |
| `pages/routing`, `routing/outputs`, `routing/paths`, `routing/sum` | 7 + 20 + 4 + 2 | `sheets/output`, `sheets/path`; map and alignment logic to `app` |
| `pages/sessions` | 2 | `menus/session` |
| `pages/tuner` | 3 | stays |
| `scope`, `theme`, `widgets`, `widgets/path` | 3 + 1 + 6 + 1 | stay (theme gains the contrast checks) |

- New tests: density steps, chain scroll keeping the selection visible,
  theme contrast, the old `page` values loading, `session_edited`,
  `heard_estimate`, the *Plays* dot rules, the metronome toggle.
- **Glossary test** (`src/tests.rs` or `app/tests.rs`): reads the string
  literals in `src/ui`, `src/app` and `src/audio` and fails on a banned
  word from §2.1: whole words, case-insensitive (so *patch* does not match
  *dispatch*), with an allowlist of files that may use platform terms in
  internal strings (`audio/linux`, `audio/win`).  It lands in step 1 with a
  second allowlist of files not yet migrated, which every later step
  shrinks and step 8 empties.  Today the literals in `app` and `audio` use
  *cushion* 16 times, *quantum* 37, *insert* 42 and *REAL-style* 17.

## 12. Order of work

Each step is one or more PRs, and each leaves every feature reachable.

1. Audio additions (§0), starting with the `OutputMode` enum.  Then the
   code renames (§2.4), one PR per term, and the glossary test with its
   allowlists.
2. Fonts, palettes, `theme` and `ui_size`.  The old pages use the tokens,
   so the look changes before the layout.
3. Setup screen.  It replaces Devices and Guide, and takes the rate,
   buffer, output mode, bridge buffer, drift correction, auto-start and
   measure controls.  Live keeps showing its readings until step 6.
4. Shell: title bar, status bar, session menu, issues drawer, diagnostics
   sheet.  The sidebar goes; a *Classic ▸* menu in the title bar reaches
   every old page not yet replaced (Live, Mixer, Routing, Plugins, Tuner).
5. Effect browser.  It replaces the Plugins page; the Mixer's *+ add
   insert* opens it.
6. Console: strips, chain rows, the stage, path and output sheets.  Live,
   Mixer and Routing go.  The saved theme starts to apply: until here
   `app::appearance::drawn_theme` keeps the window dark (only
   `RIGWERK_THEME` shows light), because the old pages paint with dark
   constants.
7. Tuner screen.  The old Tuner page and *Classic ▸* go.
8. Screenshots, README, the empty allowlist, and this file removed.

AGENTS.md, README and `docs/screenshots` change in the PR that changes
what they describe, not at the end.

## 13. Decisions to confirm

1. Code renames as in §2.4, stored keys unchanged, and the engine's
   internal *session* becoming *run*.
2. *Bridge buffer* with *Auto*, *Minimum*, *Fixed*; *Low latency* only as
   the Linux output mode and *Shared, smallest buffer* on Windows.
3. *Effect* in the chain, *plugin* for the format and the editor window.
4. IBM Plex over Roboto.
5. The window minimum stays 1040 × 700 points; below the widths in §5.2
   the paths group scrolls.

Decided: eframe's `accesskit` feature is on, so screen readers get the
labels from the start.
