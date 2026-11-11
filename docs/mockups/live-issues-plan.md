# Live Issues strip

UI plan for a shared Warnings / Errors surface on Live Bridge.
Mockup: `live-issues.html` (open in a browser; scenario and Today/Proposed
toggles in the top bar).  Code: `src/app/issues.rs` (producers, actions) and
`src/ui/pages/live/issues.rs` (the strip).

Low-latency silence detection is a **producer** for this strip, not a
separate toast family.

## Problem

Live warned in several places that are hard to grasp as one story:

- Truncated PipeWire quantum note under the OUTPUT picker
- DRIVER line + “Check driver” as a status/action hybrid
- MODE line turning amber
- Underrun badge on BUFFER
- Plus `device_alert`, routing “plays silence”, Diagnostics activity log

Facts and judgments are mixed into the same cards.  Severity and “what should
I do?” never share one place.

## Decision

**One Issues strip on Live.**  Readings stay on the cards; judgments move up.

| Keep on the card | In Issues today | Still to move into Issues |
|---|---|---|
| DRIVER actual rate/step (neutral fact) | Unaligned paths summing on a device | |
| MODE / buffer / path numbers | Path past the alignment limit | |
| Underrun count on BUFFER (neutral) | Unmuted path no unmuted device plays | |
| Low latency toggle, buffer, cushion | DSP overload of the block | |
| | Quantum change failed / does not fit / not low-latency | |
| | Driver unchecked under a low-latency mode | |
| | Underruns this session | |
| | Bridge stopped on a lost device (after the alert) | |
| | Per follower: not playing, underruns, drift at limit | |
| | Sink silent under Low latency | |
| | Device against the safety ceiling (−1.0 dBFS) | |

Severity rule: **Error** is reserved for audio breaking (DSP past the block,
an underrun burst, a bridge stopped on a lost device).
Everything that still plays, unaligned sums included, is a **Warning**.

## Placement

Under the Live Bridge page header, **above** Input / Bridge / Output.

- Empty → hidden.
- Active → header always visible; body collapsible.
- Activity log stays a **history** line at the bottom (“Recent activity”).
  Issues is current state, not a log.

## Strip behaviour

Built:

- **Collapsible.**  Header click collapses to one line: worst title +
  “+N more”, with error / warning counts.
- **Severity:** Error (red) vs Warning (amber).  One row per problem, not per
  symptom.
- **Row content:** severity · short title · one-line detail · where · one
  action button, plus the secondary Dismiss / Show on <card>.
- **Do not auto-change** settings.  Actions need an explicit click.
- **State moves per tick** (`RigWerkApp::tick_issues`, from `logic`), also
  while Live is not drawn: the underrun watch and the DSP latches.
  `live_issues` only reads.  Both start over on every bridge start and stop.
  The tick keeps the issues it judged and the strip draws those, so they
  are not judged again for every frame drawn.
- **A new Error expands** a collapsed strip: an error title the last tick
  did not have.  Warnings, and an error collapsed while showing, do not.
- Hard stop / disconnect stays `device_alert`-class: the modal window.  Once
  it is dismissed, an Error row “The bridge stopped: a device was
  disconnected” keeps the alert text, with **Open Devices**, until the next
  bridge start.  No row while the window is up, so the two never show at once.

- **Filters** All · Warnings · Errors on the header, with counts.  Choosing
  one expands the strip; a filter with no rows shows “No errors right now.”
  It falls back to All when nothing is left, and when a new error opens a
  strip filtered to warnings.  Not persisted.
- **Dismiss** (warnings only: an error is audio breaking and stays until
  fixed).  Keyed by kind + device row or path slot (`IssueKey`), not the
  title; a row that can say different things (the quantum row's four) has
  a key for each, so dismissing one does not hide another.  Hidden until the warning clears (or turns into an error) and comes
  back, or the bridge session changes.  The header shows “N dismissed ·
  show”, and stays up while every row is dismissed.
- **Show on <card>** for rows about a Live card (Output: quantum, driver,
  underruns; Bridge: device lost) rings that card in the row's colour for
  1.5 s.  Nothing scrolls; routing and DSP rows have no card.

## Producers today

- **Unaligned sum** (Warning): unaligned paths add up on one device channel
  (the Routing `Sum` warning, `app::alignment`).  → **Align paths** or
  **Show in Routing**.
- **Past the alignment limit** (Warning): a device plays a path whose chain
  latency exceeds `MAX_ALIGN`.  → **Show in Routing**.
- **Routed nowhere** (Warning): an unmuted path that no unmuted device plays,
  also with a single path.  A muted path is parked on purpose and does not
  warn.  → **Show in Routing**.
- **DSP overload** of the block (graph quantum on PipeWire direct, output
  block otherwise), with hysteresis: Warning at 70% smoothed, clears under
  60%; Error at 95% peak, clears under 85%.  The peak is the per-block total
  DSP time across paths, published by the mixer.  → **Open diagnostics**.
- **Quantum** (Warning, one row): the quantum change failed
  (`AudioEngine::quantum_note`) → **Open diagnostics**; the output period
  does not divide the graph block → **Set buffer to N** (the largest buffer
  that divides it) when the buffer is what the output asked for, else
  **Turn Low latency off**; the graph block still above the low quantum
  while Low latency runs → **Open diagnostics**.  A large block with Low
  latency off, or before the bridge forces it, is not an issue.
- **Driver unchecked** (Warning): a checkable output with no reading, no
  check running, and a low-latency mode chosen (Low latency, REAL-style,
  Exclusive).  → **Check driver**.
- **Underruns** (`UnderrunWatch`, moved per tick, reset on bridge start and
  stop):
  Warning “The output ran dry” with the count and how long ago, cleared after
  10 s without a new one; Error “Audio is dropping out” once 3 fall within
  5 s, held until quiet.  → the safer step that exists: **Use Safe cushion**,
  else **Turn Low latency off** / **Use Shared output**, else **Set buffer
  to** the next size.
- **Device lost** (Error): the bridge stopped on a disconnected device and
  the `device_alert` window was dismissed.  → **Open Devices**.
- **Sink silent** (`app::issues::silence`, Output card): RigWerk sends
  above −40 dBFS (the clock device's meter, after gate, map, level and
  mutes) while the clock sink's monitor stays below −70 dBFS for 1.5 s with
  an underrun run open, 3 s without.  With underruns it is an Error
  “Output went silent under low latency”.  Without them it is a Warning
  “Output may be silent under low latency” that also names the desktop
  mixer: RigWerk's meter is read before PipeWire's stream volume, so a
  RigWerk stream muted or turned down there looks the same.  A rest (both
  quiet) counts for nothing, and 2 s of rest drop the evidence gathered
  before it; 1 s of monitor signal clears it.  Armed only while a Low latency bridge runs; master or
  device mute, or no unmuted path to the clock device, disarm it.  The
  monitor (`audio::linux::sink_monitor`) is a passive PipeWire capture
  stream with `stream.capture.sink` on the clock sink (by name, or the
  default sink behind the ALSA shim), opened and dropped by the engine
  with the Low latency bridge; its process callback only folds a peak into
  an atomic.  No blocks (no PipeWire, not linked, suspended, other
  platforms) → no reading → no row.  → **Turn Low latency off** (the plan's
  “Use safer latency”).
- **Safety ceiling** (Warning, Output card): a playing device (bridge running,
  not muted, not missing, and a virtual device only while it is published)
  whose sample-peak limiter is reducing by at least 0.5 dB.  One row, naming
  the device with the most reduction: “<device> is against the safety
  ceiling”, detail “−1.0 dB · 3.2 dB reduction.”  No dismiss while it holds;
  the next tick drops it once every device is under 0.5 dB.  The ceiling is
  a fixed −1.0 dBFS after each device’s level and before mute, with no
  lookahead.  → **Show on Output**.
- **Per follower** (`OUTPUT · <name>`, Output card, keyed by follower slot,
  only while the bridge runs):
  - Warning “<name> is not playing”: its stream failed to open or was lost
    and is being retried (`FollowerBus::missing`), or it is not among the
    output devices (one key each, so dismissing one does not hide the
    other).  → **Show in Mixer** (pick another device or remove it).  No
    other row for that follower while it is out.
  - Underruns of its own ring (`FollowerBus::underruns`, counted since its
    stream opened), one `UnderrunWatch` per slot with the clock output's rules: Warning
    “<name> ran dry”, Error “Audio is dropping out on <name>”.  → the
    follower's own **Use Safe cushion**, else **Set buffer to** the next
    size (its stream asks for the output buffer).  Never Low latency or the
    WASAPI mode: those govern the clock device only.
  - Warning “<name> drifts too far to follow”: its drift integral is pinned
    at the ±500 ppm limit (`FollowerBus::drift_at_limit`), held so it
    does not flicker: raised after 1 s pinned, cleared after 3 s free.
    Not while the loop closes a large step of its target (a new cushion),
    which pins it with nothing wrong with the clock.  → **Open
    diagnostics**.
  - A slot that holds another follower than the last tick (removed,
    reordered or repointed live) starts its watches over and drops its
    dismissals.
  - PipeWire in-graph followers have no ring: only the missing check
    applies.

DSP share only times the path chains, not the resampler, followers or mixer.
Underruns are the direct evidence that the block was late.

The cards keep only readings: no quantum note under the OUTPUT picker, no
Check driver button or amber on DRIVER (a click on the line checks again),
the underrun badge on BUFFER and the quantum tone on MODE are neutral.  The
activity log still records the quantum note and driver differences.

## Copy / remediation (map to real controls)

Do not invent per-output quantum steppers.  Name what RigWerk owns:

- **Low latency** (desktop `clock.force-quantum` + stream `node.latency`)
- **Output buffer** / OUT CLOCK period
- **Cushion** Safe / Low latency / Manual (per follower too, in the Mixer)
- **Check driver** (probe only)

Examples for the migrated rows:

- Warning: “Graph block and output period do not line up” → **Set buffer to 64**
- Warning: “Output driver not checked yet” → **Check driver**
- Error: “Output went silent under low latency” → **Turn Low latency off**
  (with underruns; without them a Warning that also names the desktop mixer)

A quantum step table (`64 → 128 → 256 → off`) is a separate product decision.

## Open items (in order)

1. ~~Migrate today’s card judgments~~ (done: quantum, driver, underruns).
2. ~~Underrun deltas per session~~ (done: clock output; followers in
   item 6).
3. ~~Strip state~~ (done: per-tick latches, a new Error expands, a
   device-lost row after the alert).
4. ~~Filters, Dismiss, deep-link~~ (done: warnings-only Dismiss, a 1.5 s
   card ring).
5. ~~Silence~~ (done: loud send + quiet sink monitor under Low latency,
   an Error only with recent underruns; rests, mutes and nothing routed
   never raise it.  The monitor reads a live graph: on a null sink it
   links to the default sink's monitor and reads a 0.5 sine as 0.5 (see
   `AGENTS.md`).  A sink starving under Low latency is not reproduced).
6. ~~Per follower~~ (done: not playing / retried, its own underruns, drift
   at limit; the safer step is the follower's cushion, then the output
   buffer).

## Non-goals

- Scraping `pw-top` for W/Q.
- Perfect silence detection in musical content.
- Auto-changing latency without a click.
- A parallel toast system beside this strip.
- Reworking output/routing graph logic.

## Reality check

| Assumption | Today |
|---|---|
| Shared Issues strip | On Live: routing, alignment, DSP, quantum, driver, underrun, device-lost, per-follower, sink-silent, safety-ceiling producers |
| Strip state | Moves per tick, not per draw; a new Error expands it |
| Per-output preferred quantum | No: global force-quantum + `node.latency` |
| Sink monitor meters | Clock sink only, PipeWire only, only under Low latency; peak, no UI meter |
| Driver / underrun / quantum judgments | In Issues; cards show readings, the log keeps history |
| DSP timing | Path chains only; not resampler, followers, mixer |
| Follower cushion | Stored in session |

## Status

- [x] Mockup (`live-issues.html`) and plan
- [x] Strip on Live: hidden when empty, collapse, counts, one action per row
- [x] Producers: unaligned sum, alignment limit, routed nowhere, DSP overload,
  quantum, driver unchecked, underruns, device lost, per follower, sink
  silent, safety ceiling
- [x] Open items 1–2: card judgments migrated, underruns as a producer
- [x] Open item 3: strip state per tick, Error expands, device-lost row
- [x] Open item 4: filters, Dismiss, Show on <card>
- [x] Open item 6: per-follower issues
- [x] Open item 5: sink silent under Low latency (sink monitor stream)

All open items are resolved.
