# Live Issues strip

UI plan for a shared Warnings / Errors surface on Live Bridge.
Mockup: `live-issues.html` (open in a browser; scenario and Today/Proposed
toggles in the top bar).

Related: low-latency silence / xrun detection is a later **producer** for this
strip, not a separate toast family. Dual-out work (PR #7) changes which
controls remediation can name; this strip should land in a form that works
for today’s single clock device first.

## Problem

Live already warns in several places that are hard to grasp as one story:

- Truncated PipeWire quantum note under the OUTPUT picker
- DRIVER line + “Check driver” as a status/action hybrid
- MODE line turning amber
- Underrun badge on BUFFER
- Plus `device_alert`, routing “plays silence”, Diagnostics activity log

Facts and judgments are mixed into the same cards. Severity and “what should
I do?” never share one place.

## Decision

**One Issues strip on Live.** Readings stay on the cards; judgments move up.

| Keep on the card | Move into Issues |
|---|---|
| DRIVER actual rate/step (neutral fact) | Quantum does not fit / not low-latency |
| MODE / buffer / path numbers | Driver unchecked when that matters |
| Low latency toggle, buffer, cushion | Sink silent / xrun stress (later) |
| | Device lost, quantum force failed, ASIO4ALL conflict, nothing routed (where Live is the right home) |

## Placement

Under the Live Bridge page header, **above** Input / Bridge / Output.

- Empty → compact “No issues” (or hide entirely once the strip is familiar).
- Active → header always visible; body collapsible.
- Activity log stays a **history** line at the bottom (“Recent activity”).
  Issues is current state, not a log.

## Strip behaviour

- **Collapsible.** Header click (or ▾) collapses to one line: worst title +
  “+N more”. Filters stay on the header.
- **Filters:** All · Warnings · Errors (with counts). Choosing a filter
  expands the list if it was collapsed.
- **Severity:** Error (red) vs Warning (amber). One row per problem, not per
  symptom (quantum misfit + xruns on the same path → one issue at the
  strongest severity).
- **Row content:** severity · short title · one-line detail · where
  (e.g. `OUTPUT · Out clock`) · primary CTA · optional secondary
  (“Show on Output”, “Dismiss”).
- **Deep-link** highlights the related card; the story stays in the strip.
- **Do not auto-change** settings. CTAs need an explicit click.
- Hard stop / disconnect stays `device_alert`-class, not a soft Issues row.

## Copy / remediation (map to real controls)

Do not invent per-output quantum steppers. Name what RigWerk owns today
(and what dual-out adds):

- **Low latency** (desktop `clock.force-quantum` + stream `node.latency`)
- **Output buffer** / OUT CLOCK period
- **Cushion** Safe / Low latency / Manual (and per-follower cushion once
  generic followers exist)
- **Check driver** (probe only)

Examples:

- Warning: “Graph block and output period do not line up” → **Set buffer to 64**
- Warning: “Output driver not checked yet” → **Check driver**
- Error (later): “Output went silent under low latency” → **Use safer latency**

A quantum step table (`64 → 128 → 256 → off`) is a separate product decision,
not assumed UI from dual-out.

## Producers (rollout)

1. **Migrate today’s Live judgments** into the strip (quantum misfit, driver
   unchecked when relevant). Biggest UX win; no new detection.
2. **B-lite:** session underrun deltas + existing `QuantumInfo` /
   `quantum_note` as Warnings with safer-latency / buffer CTAs.
3. **A (later):** loud stream + quiet **sink monitor** (new PipeWire metering),
   armed only while low latency is engaged; prefer A ∧ recent stress, and
   gate/mute/nothing-routed as false-positive guards — guitar rests are common.
4. **Per-member / follower** issues after PipeWire followers (dual-out step 2+)
   or generic followers (step 3): evaluate per device; CTAs name cushion vs
   Low latency correctly for that path.

## Non-goals

- Scraping `pw-top` for W/Q.
- Perfect silence detection in musical content.
- Auto-changing latency without a click.
- A parallel toast system beside this strip.
- Reworking output/routing graph logic (belongs to dual-out steps).

## Reality check (today)

| Assumption | Today |
|---|---|
| Per-output preferred quantum | No — global force-quantum + `node.latency` |
| Sink monitor meters | Not present (RigWerk peaks only) |
| Driver / underrun / quantum notes | Present — migrate first |
| Follower cushion | Stored in session; plays in later steps |
| Shared Issues strip | Mockup only |

## Status

- [x] Mockup (`live-issues.html`)
- [x] Plan (this file)
- [ ] Implement strip shell on Live (collapse + filters + empty state)
- [ ] Migrate quantum + driver judgments
- [ ] Wire CTAs to real settings
- [ ] Later: silence / xrun producers
