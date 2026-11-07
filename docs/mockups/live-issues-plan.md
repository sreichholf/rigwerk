# Live Issues strip

UI plan for a shared Warnings / Errors surface on Live Bridge.
Mockup: `live-issues.html` (open in a browser; scenario and Today/Proposed
toggles in the top bar).  Code: `src/app/issues.rs` (producers, actions) and
`src/ui/pages/live/issues.rs` (the strip).

Low-latency silence / xrun detection is a later **producer** for this strip,
not a separate toast family.

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
| DRIVER actual rate/step (neutral fact) | Unaligned paths summing on a device | Quantum does not fit / not low-latency |
| MODE / buffer / path numbers | Path past the alignment limit | Driver unchecked when that matters |
| Low latency toggle, buffer, cushion | Unmuted path no unmuted device plays | Underrun deltas |
| | DSP overload of the block | Sink silent (later) |

Severity rule: **Error** is reserved for audio breaking (DSP past the block).
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
  action button.
- **Do not auto-change** settings.  Actions need an explicit click.
- Hard stop / disconnect stays `device_alert`-class, not a soft Issues row.

Not built (see Open items): All · Warnings · Errors filters, a secondary
action (“Show on Output”, “Dismiss”), deep-link highlight of the related card.

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

DSP share only times the path chains, not the resampler, followers or mixer.
Underruns are the direct evidence that the block was late.

## Copy / remediation (map to real controls)

Do not invent per-output quantum steppers.  Name what RigWerk owns:

- **Low latency** (desktop `clock.force-quantum` + stream `node.latency`)
- **Output buffer** / OUT CLOCK period
- **Cushion** Safe / Low latency / Manual (per follower once generic
  followers exist)
- **Check driver** (probe only)

Examples for the migrated rows:

- Warning: “Graph block and output period do not line up” → **Set buffer to 64**
- Warning: “Output driver not checked yet” → **Check driver**
- Error (later): “Output went silent under low latency” → **Use safer latency**

A quantum step table (`64 → 128 → 256 → off`) is a separate product decision.

## Open items (in order)

1. **Migrate today’s card judgments:** the quantum note (logged from
   `app/bridge.rs`), the amber Check driver line (`live/driver_line.rs`) and
   the underrun badge on BUFFER (`live/health.rs`).  Remove the card warnings
   in the same change.
2. **Underrun deltas** per session as a producer, with safer-latency / buffer
   actions.  Before silence detection: they are the direct evidence.
3. **Strip state:**
   - Move the DSP alarm latch to the per-tick update.  Today it advances in
     `live_issues(&self)` through a `Cell`, and only while Live is drawn.
   - Decide whether a new Error expands a collapsed strip.
   - When the bridge stops on device lost, `device_alert` takes over and the
     strip empties.  Say so in the strip or add a row; do not vanish silently.
4. **Filters, Dismiss, deep-link** highlight of the related card.
5. **Silence:** loud stream + quiet **sink monitor** (new PipeWire metering),
   armed only while low latency is engaged.  Prefer silence ∧ recent
   underruns; gate / mute / nothing routed guard false positives, since
   guitar rests are common.
6. **Per follower:** evaluate per device; actions name cushion vs Low latency
   correctly for that path.

## Non-goals

- Scraping `pw-top` for W/Q.
- Perfect silence detection in musical content.
- Auto-changing latency without a click.
- A parallel toast system beside this strip.
- Reworking output/routing graph logic.

## Reality check

| Assumption | Today |
|---|---|
| Shared Issues strip | On Live: routing, alignment, DSP producers |
| Per-output preferred quantum | No: global force-quantum + `node.latency` |
| Sink monitor meters | Not present (RigWerk peaks only) |
| Driver / underrun / quantum judgments | Still on the cards and the log |
| DSP timing | Path chains only; not resampler, followers, mixer |
| Follower cushion | Stored in session |

## Status

- [x] Mockup (`live-issues.html`) and plan
- [x] Strip on Live: hidden when empty, collapse, counts, one action per row
- [x] Producers: unaligned sum, alignment limit, routed nowhere, DSP overload
- [ ] Open items 1–6 above (next: migrate the card judgments)
