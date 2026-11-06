# Dual-out and virtual endpoints

Concept for review. Superseded where they differ by `dual-out-plan.md` and mockup E (`output-devices.html`, `signal-routing.html`); `dual-out-endpoints.html` (A–D) is kept as history.

## Job

RigWerk is a two-clock audio bridge: capture on the driver the interface wants, play on the driver the speakers want, with a short rack (EQ, VST3/CLAP inserts, gate) in between. It is a trustworthy cable, not a DAW.

The missing half of that job is **one mix, several places**:

- **Listen** — what you hear (headphones, monitors). Owns the felt playback clock.
- **Send** — extra hardware destinations (monitors while you listen on headphones, a second interface). Same post-mix stereo, slave cushion only if that clock differs.
- **Publish** — a virtual source other apps can capture (Discord, OBS, a DAW).

Two destinations is the common case. Three or four must stay usable (listen + monitors + published source + a spare cable).

## Engine shape (shared, both OSes)

```
capture  →  ring + drift only when clocks differ
         →  one mixer (EQ / inserts / gate)
         →  fan-out
              listen   (clock owner)
              send…    (slave resample + short cushion if needed)
              publish  (virtual source)
```

- One mix. Not a second full bridge per destination.
- When input and output already share a clock (PipeWire duplex), skip the ring. `render_direct` on `origin/claude/pipewire-duplex-node` is the mixer split for that.
- The existing Windows ASIO duplex path is same-driver in+out through the ring. It is not dual-out and not a virtual endpoint.

## Platform

**Linux.** A first-party node is possible in userspace. Today’s private `RigWerk Capture` / `RigWerk Playback` streams are not advertised as endpoints. The duplex branch publishes one `pw_filter` named `RigWerk` and links it to the chosen source and sink. Publishing means other apps can see a Source (and optionally a Sink into the rack).

**Windows.** A normal app cannot create a WASAPI endpoint. Dual-out is userspace (another render client). A name other apps can pick is either:

- v1: drive an installed cable (VB-Audio and similar), or
- v2: a separate signed virtual audio driver (`RigWerk Out`).

The UI must say which of those is true. Never imply a WASAPI device exists when it does not.

## UI that was rejected

Mocked as A/B/C in the same HTML file. They fit two outs and fall apart at three or four.

| Variant | Idea | Why not |
| --- | --- | --- |
| A | Green Send strip under the Live Output card | A second card-within-a-card does not grow to N |
| B | Replace Output with a Listen + Send stack | Same heat; Live row gets taller per destination |
| C alone | Publish banner, Live stays one hardware out | Right Live story, no home for per-out detail |

## Preferred UI (D)

Live stays the publish-simple view. Per-destination detail uses the Mixer path that EQ already uses.

**Live**

- Publish status for the virtual source (on, rate, or “needs a cable / driver” on Windows).
- The existing Output card stays the **listen** clock (device, level, meters).
- Other destinations are chips, not cards.
- `+ Add output` jumps to Mixer and opens the add sheet.

**Mixer** (this is the configuration surface)

Mixer already is a left signal chain and a right inspector. `ChainSelection` is `Input`, `PreEq`, `Plugins` / `Plugin(n)`, `Gate`, `PostEq`, `Output`. Selecting a stage fills the inspector. Inserts are children under Plugins, with `+ Add insert` and remove.

Destinations follow that pattern. They are **children of the existing Output stage**, not a new page and not a new column on Live.

- Chain order stays Input → Pre EQ → Plugins → Gate → Post EQ → **Output**.
- Under Output: one row per destination (name, role, level), like insert rows.
- Select a row → the **same right inspector** shows that destination: device, role (Listen / Send / Publish), rate and buffer, level, mute, meters.
- Selecting the Output header can stay the master view (today’s mute + output gain) or a short list of children. Either way, child detail is the inspector, same as Pre/Post EQ.

**Add** (three doors, one sheet)

- Mixer toolbar `+ Add output`, beside `+ Add insert`.
- Dashed `+ Add output` under the Output children.
- Live chip `+ Add output` (opens Mixer).

The sheet asks for kind (Listen / Send / Publish) and device, then leaves the new row selected.

**Remove** (same as insert remove)

- `×` on the chain row.
- `Remove output` in the inspector.

The last **Listen** clock cannot be removed until another destination is promoted to Listen. Sends and the published source can be removed without stopping the others.

## What a review should check

- Does hanging N destinations under `ChainSelection::Output` fit the chain, or does the enum need `Output(index)` the way plugins are `Plugin(index)`?
- Is one Listen clock plus N slaves the right clock model, including the PipeWire duplex (no ring) and ASIO duplex (one driver) cases?
- Session format: today’s preset stores one input and one output. N destinations, roles, and publish-on need a place that old presets still load.
- Windows copy: cable vs future driver, and what Live shows when neither is present.
- Whether Live chips should jump to the selected Mixer child (assumed yes) and whether they need meters.
- Overload: a chain that cannot keep the buffer should fail safe on every destination, not only the listen clock.

## Out of scope

Multitrack, clip launching, surround, a second independent bridge per out, and any Windows API that pretends to register a WASAPI endpoint from the GUI process.
