# Implementation Plan: Stackable NAM Blocks (Rigwerk)

## Context

Rigwerk currently has a single, fixed NAM amp slot followed by IR cab processing.
Goal: turn NAM into a generic chain block that can be inserted anywhere, any number
of times (e.g. Tube Screamer NAM → JCM800 NAM → IR), with no artificial product limit.
CPU safety comes from load estimation, visibility and the existing DSP overload
warning, not from hard caps.

Reference measurement: one standard NAM ≈ 12% DSP load (as shown by Rigwerk's meter).

## Ground rules for the implementing agent

- **Discover first.** Before changing anything, map the current code: NAM wrapper,
  audio callback / chain, IR block, preset (de)serialization, DSP load meter,
  overload warning, UI for the NAM slot. Write a short findings note (files, types,
  threading model, preset format) and **stop for review** before Phase 1.
- **Work phase by phase.** One commit (or PR) per phase. After each phase: stop,
  summarize what changed, what was tested, and any deviations from this plan.
  Wait for review before continuing.
- **Real-time safety is non-negotiable.** On the audio thread: no allocation, no
  locks that can block, no file I/O, no logging that allocates. If the codebase lacks
  a mechanism for this (lock-free swap, RT allocation detection), add it in Phase 1.
- **No sound changes for existing users.** Migrated presets must render bit-identical
  (or within float noise, document the threshold) to the current build.
- Follow existing code style, build system and test framework. Don't introduce new
  dependencies without flagging it in the phase summary.
- If something in this plan conflicts with how the codebase actually works, say so
  and propose an alternative instead of forcing it.

---

## Phase 0: Decisions (fixed, do not re-litigate)

- NAM is a block type like any other chain block; arbitrary position, arbitrary count.
- Technical cap: 16 NAM blocks per chain (memory / load time protection only).
- Soft UI confirmation from the 5th NAM block onward ("This is a lot of NAM blocks; check DSP load").
- Overload handling is advisory: never auto-remove or silently bypass user blocks.

## Phase 1: Engine – `NamBlock` and dynamic chain

- Extract current NAM processing into a `NamBlock` (name per codebase convention) owning:
  - loaded model (shared_ptr or equivalent, RT-safe release)
  - input trim (dB), output trim (dB)
  - bypass flag
  - measured/estimated cost (see Phase 3)
  - model metadata (architecture, sample rate, calibration levels if present)
- Chain = ordered list of blocks. Mutations (add/remove/reorder/replace model):
  - build the new chain off the audio thread
  - publish via atomic pointer swap (or the codebase's existing equivalent)
  - free the old chain off the audio thread (deferred deletion / GC queue)
- Model loading:
  - load + parse + allocate on a background thread
  - warm-up: run several silent buffers through the model before it goes live
    (NAM models have internal state / receptive field)
- Bypass: true skip — no inference when bypassed. Smooth on/off with a short
  crossfade (~10–20 ms) to avoid clicks.
- Model replacement in an existing block: crossfade old → new (~20–50 ms), both run
  only during the fade.
- Resampling: NAM models expect their trained rate (normally 48 kHz). If host rate ≠
  model rate, group **adjacent** NAM blocks with the same model rate and resample
  once around the group, not per block. Non-NAM blocks between them break the group.
  If this is invasive, implement per-block first and the grouping as a separate
  commit within this phase.
- Add an RT-allocation guard for debug builds (hook/sanitizer) that asserts if the
  audio callback allocates.

**Exit criteria:** chain with 1..N NAM blocks processes correctly; add/remove/reorder
while audio runs without glitches or RT allocations; bypassed blocks cost ~0.

## Phase 2: Migration of the existing single NAM

- Existing fixed NAM slot becomes one `NamBlock` at the same chain position, with
  identical model, gains and parameters.
- Preset schema:
  - bump schema version
  - loader accepts old format and converts (e.g. `nam: {...}` → `blocks: [{type: "nam", ...}]`)
  - saver writes new format only
  - keep the old-format reader permanently (or until explicitly removed later)
- Map any existing NAM-specific parameters (input/output gain, etc.) onto the
  block's trims with **no** change in effective gain.
- UI: remove the fixed NAM slot; existing shortcuts/hotkeys targeting "the NAM"
  target the first NAM block in the chain.
- Golden/null test: a set of representative old presets rendered with the old build
  vs the new build on a fixed input file → difference must be below the documented
  threshold. Commit reference renders or a script to generate them.

**Exit criteria:** every existing preset loads and sounds identical; round-trip
load→save→load is stable.

## Phase 3: Load estimation and metering

- Per-model cost measurement on load (background thread): process ~200 buffers at
  the current buffer size and sample rate, record mean and p99 µs/buffer, convert to
  % of buffer period.
- Cache results per (model file hash, sample rate, buffer size) on this machine.
- Fallback before measurement completes: lookup table by architecture
  (standard / lite / feather / nano) × sample rate, calibrated from the 12% reference.
- Before adding a block / loading a model: show a projected load
  ("≈ +6% → ~31% DSP"). Warn (non-blocking) if projected peak > ~70%.
- DSP meter: show average **and** peak-hold. Make sure the existing overload warning
  reacts to peak or sustained high load, not average only. Document thresholds.

**Exit criteria:** projected vs actual load within reasonable tolerance (document
measured error); meter shows peak; warning fires before/at audible dropouts in a
stress test.

## Phase 4: UI

- "Add block → NAM" available at any chain position; drag-and-drop reorder.
- Each NAM block shows: model name, architecture badge, estimated/measured load,
  bypass toggle, input/output trim.
- Soft confirmation dialog when adding the 5th+ NAM block. Hard stop at 16 with a
  clear message.
- On overload: highlight the most expensive active block(s) and offer one-click bypass.

**Exit criteria:** all of the above usable without touching presets manually.

## Phase 5: Heavy presets on weaker machines

- On preset load: compute projected total load before activating.
- If projected peak exceeds threshold: load anyway, show a warning listing the most
  expensive blocks, offer "bypass suggested blocks". Never refuse to load, never
  drop or bypass blocks silently.
- Bypass state chosen this way is a normal user edit (saved only if the user saves).

**Exit criteria:** a preset with many standard models opened at a small buffer size
loads, warns, and recovers cleanly when blocks are bypassed.

## Phase 6: Level calibration between NAM blocks (required for stacking)

Background: an amp model only reacts correctly when its input level matches the level
used during capture. Stacking a pedal model into an amp model without level matching
makes the amp model behave as if it had more/less gain.

- Parse NAM model metadata for input/output calibration levels when present
  (check the actual field names in the NAM format/version supported by the code;
  handle absence gracefully).
- Default behavior: if both adjacent NAM blocks have calibration metadata, apply an
  automatic level match between them (output level of block A → expected input level
  of block B). Make this a per-block toggle ("Auto level"), default on.
- Without metadata: 0 dB default, manual trims, and a small hint in the UI
  ("no calibration data, adjust input trim by ear").
- The existing first NAM block's input calibration (if any already exists) must not
  change behavior for migrated presets (Phase 2 null test must still pass).

**Exit criteria:** TS NAM → amp NAM with calibrated models behaves sensibly at
unity trims; behavior with uncalibrated models is predictable and documented.

## Phase 7: Tests and verification

- Unit: preset migration (old → new), round-trip, schema version handling.
- Golden: null test from Phase 2 in CI (or as a documented script if CI can't run audio).
- RT safety: add/remove/reorder/replace/bypass under running audio with the RT
  allocation guard enabled — zero violations.
- Stress: add standard models until overload; verify warning, highlighting, and
  clean recovery after bypass.
- Performance: measure and report DSP load for 1, 2, 4 standard models and for
  standard + lite at the smallest supported buffer size, on the weakest supported
  machine if available; otherwise state which machine was used.
- Listening sanity check: TS-style pedal capture → amp capture → IR, with and
  without auto level. Note results in the phase summary.

## Final deliverable

- All phases merged on a feature branch (or one PR per phase, per repo convention).
- A short `CHANGES`/PR description: new block model, migration notes, preset schema
  version, measured performance numbers, known limitations
  (impedance interaction not modeled, aliasing with stacked high-gain models at 48 kHz,
  captures that already include a boost or cab will double up).

---

## Reviewer notes: where the plan meets this repo

The plan was written without the code.  These are starting points for the
findings note, not decisions.  `AGENTS.md` wins over this plan where they
disagree; flag each such case in the phase summary.

- **Current NAM.**  `audio/amp.rs` is a built-in stage, one per path, using
  `nam-rs`.  It is not a plugin-chain slot.  `PathMix.amp: AmpControls`,
  `PathControls::{amp, pending_amp, amp_park}` (`audio/path/mod.rs`),
  `app/amp.rs`, `ui/sheets/amp.rs`.  Position today is `AmpSlot::{Pre, Post}`
  relative to the plugin effects; Phase 2's "same chain position" has to
  cover both.
- **RT handoff that already exists.**  The UI thread builds, resets and
  prewarms a model; the callback `try_lock`s `pending_amp` at block start;
  a replaced model goes to `AmpPark`, drained only on the UI thread.  The
  plugin chain (`audio/plugins/chain.rs`) applies queued commands at block
  start and returns slots through a return queue.  Reuse one of these; do
  not add a `Mutex` on the sample path or an `Arc` swap the callback drops.
- **"Presets"** are the RON preferences and sessions in `app/settings`
  (`AmpSettings` in preferences and sessions).  Use `#[serde(default)]`;
  bump `PREFERENCES_SCHEMA` only if an old save cannot load from field
  defaults.  An old `amp` field maps to the first NAM block.  Keep
  unreadable or newer files' original bytes.
- **Resampling.**  Today a model at the wrong rate does not install and
  nothing resamples.  Treat Phase 1 resampling as new scope; it may be
  proposed as a later phase.
- **Slimmable width** (`AmpSettings.slim`) is per model and must move onto
  the block.
- **DSP meter and overload** come from the live snapshot (`audio/shared.rs`,
  `audio/mixer`, `app/issues.rs`).  New figures go through the snapshot.
- **New numeric settings** follow the path in `AGENTS.md`: preference field,
  `sanitized`/`sanitize_preferences`, `RigWerkApp` field, engine setter,
  widget.
- **Tests** go in `tests.rs` beside the code, no `tests/` directory.  Done
  means `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D
  warnings`, `cargo test --locked`.  No real devices in committed tests.
- **Branch.**  Work on `audio/nam-blocks` (or a branch per phase off it),
  rebased on `origin/master`.
