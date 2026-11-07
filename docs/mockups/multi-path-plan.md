# Multiple signal paths: implementation plan

Working plan for the multi-path PR (branch `feature/multi-signal-paths`).
Read this before picking up any work package. Background and UI:
`multi-path-concept.md` and `multi-path-routing.html` (mockup F).

## Decisions (agreed)

- N paths (`MAX_PATHS = 4`), each a full Mixer chain. They split where the
  chain starts (`stage_front`), after the clock side (ring, resampler,
  drift). There is no ring per path.
- One sum matrix: for each device, each path has a 2 × channels map plus a
  level and a polarity. Cells that land on one channel add up.
- The master (level, mute) is shared by every path; no path skips it. It
  is applied to each path before the alignment (so one path stays
  bit-identical), so a master change reaches a delayed path up to its
  alignment delay later.
- Alignment is per device: every path routed to a device waits for the
  slowest one there. Muted or un-soloed paths still count. Each device has
  an align switch (default on). Each path on each device has a trim
  (±5 ms) and a polarity.
- A path never takes its input from another path. Users duplicate instead.
- One path at 0 dB is bit-identical to today. Old sessions load as path 1.

## Ground rules for every package

- No allocation, locks, or logging in callbacks. Preallocate
  `[_; MAX_PATHS]` and follow the `followers` / `follower_count` pattern.
- Each package is one commit (subject style per `AGENTS.md`). Fold review
  fixes into that commit.
- Before you commit: `cargo fmt --check`, `cargo clippy --all-targets -- -D
  warnings`, `cargo test`, and `cargo check --target
  x86_64-pc-windows-gnu` when Windows code changes.
- Update the module docs where you change behaviour. The README gets
  updated in the last package.

## Work packages

Each package is implemented once and reviewed separately.
Packages on the same line of the table can run in parallel, each in its
own worktree.

| Wave | Package | Depends on |
| ---- | ------- | ---------- |
| A | P1 one path, new shape | — |
| A | L1 plugin latency reporting | — |
| B | P2 N paths engine + sessions + sum matrix | P1 |
| C | U1 Mixer path sub-entries + path selector | P2 |
| C | U2 Signal Routing layers + cards | P2 |
| D | P3 alignment engine (delay lines, align, trim, polarity, dry/wet fix, chain handover) | P2, L1 |
| E | U3 alignment UI + Live / Live Issues / Diagnostics polish + README | U1, U2, P3 |
| F | Final whole-PR review | all |

### P1: one path, new shape (no visible change)

- `PathControls` in `Shared`: routes, mono, enable, EQ, gate, the plugin
  chain, level, mute, solo, and meters. For now there is exactly one,
  `paths[0]`. Existing accessors forward to path 0, so callers barely
  change.
- `PathProcessor` in `OutputMixer`: EQ state, gate, ramps, and plugin
  scratch. `stage_front` / `finish_block` become per-path methods, and the
  clock side stays on `OutputMixer`.
- Device maps are stored per path (`[map; MAX_PATHS]` per device), with
  only path 0 used.
- Output is bit-identical: the existing tests pass unchanged, plus one
  test that compares against a recorded pre-refactor output.

### L1: plugin latency reporting

- VST3: query `getLatencySamples` on activate, and again on
  `kLatencyChanged` (restart flags).
- CLAP: query `clap_plugin_latency` on activate, and handle
  `host_latency_changed` (flag it, then re-query on the main thread and
  re-activate as the spec requires).
- Publish the latency per insert, plus the sum over the chain:
  `PluginChain::latency_samples()`, and an atomic the audio thread can
  read. A bypassed insert still counts, because its processing is
  bypassed but its latency stays (document this).
- Show it in the Plugins page insert row (`· 64 smp`) only if non-zero.
- Touches the plugin host code only (`src/audio/clap`, `src/audio/plugins`,
  and the VST3 host). It does not touch `shared.rs` or the mixer.

### P2: N paths (engine, sessions, sum matrix)

- Active path count. Add, duplicate (added muted), and remove paths, all
  live with no restart. A removed path fades out.
- Per-path input routing: the existing 2 × 2 per path (the UI shows taps
  1/2; storage keeps today's 2 × 4 `routes`, taps 3/4 stay unused).
- Sum matrix in `output_map`: per device, per path, a map, a level, and a
  polarity. Cells add. Level and mute changes ramp.
- Solo: if any path is soloed, only soloed paths sound.
- Sessions: `paths: Vec<PathPref>` and a per-device `path_map`, in both
  `Preferences` and `Preset`. An old file → path 1 (tests for old → new
  and round trips).
- App layer: plugin GUI windows are keyed by (path, insert). Presets
  load and save every path.
- No alignment yet. Polarity can be stored and applied, but trim waits
  for P3.

### U1: Mixer per path

- Sidebar: one sub-entry under Mixer per path (colour dot, name, and an
  "N stages · X ms" line), the same height as the other main entries.
  With one path nothing changes.
- The Mixer page edits the selected path. Path chips in the toolbar. The
  Output stage is tagged `SHARED`. Path level, mute, and solo.
- Plugins page: the insert rows carry the path colour and name.

### U2: Signal Routing

- Layer ①: an input matrix per path. Layer ②: path cards (colour, name,
  chain summary, level, mute, solo, Open in Mixer, + Add path,
  Duplicate, remove). Layer ③: the device list, plus the selected
  device's matrix with one row pair per path, and a sum block per path.
- Cards: Single, Dry L / Wet R, Per-device mixes (Blend and Dual-amp
  disabled until P3, "needs alignment"). After any edit: Custom.
- Warn when paths share a channel unaligned (red), or sit on different
  channels of one device (amber).

### P3: alignment

- One delay line per path, sized once (2¹⁵ frames), and one read tap per
  device. The delay per device = the max over the paths routed there,
  minus the path's own latency, plus the trim. A delay change crossfades
  between the old and new tap. It never jumps.
- The per-device `align` flag lives in sessions and in `Shared`.
- Insert dry/wet: delay the dry side by that insert's latency.
- Chain handover: no `try_lock` skip in the audio thread. A UI edit
  must not cost a block. Pick the smallest safe design (e.g. prepare the
  edit off-thread and swap it in an O(1) section, or keep the last good
  chain running until the swap). Document the choice in the module docs.
  Outcome: `try_lock` stays, with a fallback. Mix, bypass and plugin
  controls no longer take the chain lock, so common edits are lock-free;
  a chunk that misses the lock (a structural edit) plays the chain input
  delayed by the chain latency, so the sum keeps its phase (see
  `path/plugins.rs`, "Chain handover").
- Publish the per-device total latency and the per-path delay for the
  UI.
- Tests: two identical paths, one with a latent fake plugin. With align
  on, the sum is 2× the signal, delayed. With it off, there is a comb.
  Also a crossfade with no clicks when the latency changes.

### U3: alignment UI and polish

- Routing: the align switch, trim, polarity, and the alignment delay per
  path. A latency line ("waits for <path>, +X ms"). Enable the Blend and
  Dual-amp cards.
- Live: path count on the chain strip, and the added latency on the
  Output chip. Live Issues: unaligned sum, a muted path routed nowhere
  audible, a path overloading the quantum.
- Diagnostics: DSP time per path.
- README: a section for the feature.

## Status

- [x] P1  - [x] L1  - [x] P2  - [x] U1  - [x] U2  - [x] P3  - [x] U3
- [x] Final review  - [ ] PR
