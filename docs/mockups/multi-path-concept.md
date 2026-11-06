# Multiple signal paths

Concept for review. UI: `multi-path-routing.html` (mockup F, open in a
browser; the bar at the top switches between the cards below and turns
alignment off to show what goes wrong without it). Builds on
`dual-out-plan.md` (clock device + followers, two routing layers).

## Job

Today the input runs through **one** chain (EQ, inserts, gate) and the
finished mix fans out to every output device. Users want more than one
chain on the same input:

- **Dry left / wet right**: the clean signal on one speaker, the effected
  one on the other (or on another amp).
- **Blend**: mix some clean tone back under a driven one.
- **Dual-amp**: two amp sims (or two IRs) in parallel, summed.
- **Different mix per device**: monitors get the full chain, the stream
  (RigWerk Out) gets gate + limiter, headphones get it without reverb.

All four are the same thing: **N paths, each a full Mixer chain, and one
matrix that decides which path lands on which device channel**. Cells that
meet on one channel add up.

It is still a cable with a rack, not a DAW: no tracks, no automation, no
sends between paths.

## Engine shape

```
capture ─▶ [ring + resampler + drift, only when clocks differ]
        ─▶ split (one stereo frame, per-path input routing)
              ├─ path 1: front (route, pre EQ, PRE gate) · inserts · POST gate · post EQ · path level
              ├─ path 2: …
              └─ path N
        ─▶ alignment (one delay line per path, one read tap per device)
        ─▶ sum matrix: paths × device channels, level + polarity per path and device
        ─▶ master (level, mute) ─▶ device level / mute ─▶ clock device + followers
```

- **The split is where the chain starts, not at the input callback.** Both
  playback paths hand the chain one stereo frame: the ring path after the
  resampler, the PipeWire duplex path straight from the graph buffers
  (`render_direct_devices`). Paths never need a ring of their own; the
  duplex path stays ringless. Clock handling (ring, cushion, drift,
  dropout fade) runs once, in front of the split.
- `stage_front` / `finish_block` in `src/audio/mixer/mod.rs` are already the
  only chain code for every backend. They become per-path methods on a
  `PathProcessor` (EQs, gate, ramps, plugin scratch). `OutputMixer` keeps
  the clock side and owns `[PathProcessor; MAX_PATHS]`.
- `Shared` grows a `[PathControls; MAX_PATHS]` (routes, mono, enable, EQ,
  gate, plugin chain, level, mute, solo, meters) plus an active count, the
  same pattern as `followers` / `follower_count`. Everything is
  preallocated; adding a path never allocates in a callback.
- `MAX_PATHS = 4`. Inactive paths cost nothing.
- **Sum matrix** replaces the single `DeviceMapper::map(l, r)`: per device,
  per active path, a 2 × channels map (as today) plus a level and a
  polarity. One path with level 0 dB is bit-identical to today (test it).
- **Direct path budget.** In duplex mode every path has to fit in one graph
  quantum. Diagnostics shows the time per path; an overload names the path.

## Latency and alignment

Plugins (amp sims, lookahead limiters, linear-phase EQ) delay their path.
Two paths that differ by 64 samples and add up on one channel comb filter:
the sound turns thin and hollow, and sweeps when the latency changes.

- **Rule: on each device, every path waits for the slowest path that plays
  there.** The user hears the slowest path's latency; there is no way
  around that if the paths should line up.
- **Alignment is per device, not global.** A path that only plays on its
  own device (per-device mixes) keeps its own latency. Headphones without
  the lookahead limiter don't wait for the stream's limiter.
- Every path routed to a device counts, muted or not soloed included, so
  muting, soloing or unmuting never moves the latency.
- On by default. A per-device switch turns it off (e.g. dry/wet on two
  separate amps where the user wants the dry side as fast as possible).
  With it off, Routing warns: red when unaligned paths **add up on one
  channel**, amber when they only sit on different channels (image shift).
- Per path and device: **polarity flip** and a **trim** (±5 ms) on top of
  the automatic alignment. Two amp sims can be out of phase even at equal
  latency; dual-ampers expect both controls.
- Implementation: one delay line per path, sized once for the maximum
  alignment (e.g. 2¹⁵ frames, ~170 ms at 192 kHz); each device reads its
  own tap. Memory scales with paths, not with paths × devices.
- **Plugin latency must be known first.** Today it isn't: the CLAP host's
  `latency.changed` is a no-op, port latencies are 0, and VST3
  `getLatencySamples` is not queried. Needed: query on activate, re-query
  on CLAP `latency.changed` / VST3 `kLatencyChanged`, publish per insert and
  per path. A latency change moves the read taps with a short crossfade,
  never a jump.
- Same root cause, smaller scale: an insert's own dry/wet `mix` blends the
  undelayed dry signal with a latent wet one. Fix it with the same
  latency number (delay the dry side inside the chain).

## UI

### Signal Routing (the main surface)

Three layers, left to right, as the engine has them:

1. **Input → paths.** Per path, the existing 2 × 2 input matrix (IN 1/2 onto
   that path's L/R). Mono guitar into two paths = IN 1 on in both.
2. **Paths.** One card per path: colour, name, chain summary, latency, path
   level, mute, solo, `Open in Mixer`. `+ Add path` and `Duplicate` here.
   Duplicate is the main action: a full copy of what the user already has,
   **added muted**, so the sum doesn't jump 6 dB.
3. **Paths → outputs.** The existing device list. The selected device's
   matrix has one row pair (L, R) per path, coloured by path. Below it, a
   **sum** block per path on that device: level, polarity, trim, and the
   alignment delay it gets. A latency line says what the device waits for
   and why.

**Cards** (starting points, the presets of the new matrix): *Single path*,
*Dry L / Wet R*, *Blend clean*, *Dual-amp*, *Per-device mixes*. A card sets
up paths, input routing and the sum matrix in one go; any edit afterwards
shows *Custom*. Cards never delete inserts: applying one to existing paths
only changes routing; missing paths are added empty (or as duplicates for
*Blend* / *Dual-amp*).

### Mixer

- Sidebar: `Mixer` gets one sub-entry per path (colour dot, name). With one
  path nothing changes visually.
- The Mixer page edits **one path** at a time: Input → Pre EQ → Plugins →
  Gate → Post EQ → **Path level**. A path selector (chips) sits in the
  Mixer toolbar too, for users who stay on the page.
- The Output stage (master + output devices) is shared by all paths. It
  shows at the bottom of every path's chain with a `SHARED` tag.

### Live

- The chain strip shows the path count (`3 paths`) and, if alignment adds
  latency, the added amount on the Output chip.
- Live Issues gets: *paths add up unaligned on <device>*, *path <name> is
  muted and routed nowhere audible*, *paths overload the graph quantum*.

### Plugins

Insert rows carry their path's colour and name. Plugin GUI windows are
keyed by (path, insert), not insert index.

## Sessions

- `paths: Vec<PathPref>` (name, colour, routes, mono, enable, EQ, gate,
  plugins, level, mute) and per device `path_map` (per path: 2 × channels,
  level, polarity, trim) plus `align`.
- Old files: today's top-level mix fields become path 1; each device's
  existing map becomes path 1's map. No other change, tests for old → new
  and round trips (`Preferences` and `Preset`).

## Steps

1. **One path, new shape.** `PathControls` / `PathProcessor` /
   per-path device map with exactly one path. No visible change,
   bit-identical output, all tests green.
2. **N paths without summing.** Add / duplicate / remove paths, Mixer
   sub-entries, Routing layers 1–3 with the sum matrix. Cards *Single*,
   *Dry L / Wet R*, *Per-device mixes*. Paths may share a channel, but
   until step 3 Routing warns that they are not aligned.
3. **Latency and alignment.** Plugin latency reporting (VST3 + CLAP), delay
   lines, per-device align switch, polarity, trim, the dry/wet fix inside
   the chain. Unlocks *Blend* and *Dual-amp*.
4. **Polish.** Live summary, Live Issues, per-path DSP time in Diagnostics.

## What a review should check

- Is "master after the sum" right, or should each path also be able to skip
  the master (e.g. a stream path that ignores the monitor mute)?
- Is per-device alignment the right granularity, or per channel group?
- `MAX_PATHS = 4`: enough for dual-amp + blend + stream?
- Should a path be able to take its input from another path's output
  (e.g. stream = main + limiter)? Assumed **no** for now: duplicate instead.
- `try_lock` in `process_plugins` skips a chain for a block while the UI
  edits it. In a sum that is a level/phase jump, not just a tonal blip.
  Needs a lock-free handover (or a fade) before step 3 ships.

## Out of scope

More than the two captured input channels, sends between paths, per-path
sample rates, automation, any path that runs on its own clock.
