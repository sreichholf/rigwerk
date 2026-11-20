# Findings: stackable NAM blocks

What the repo actually looks like, written before Phase 1.  `docs/nam-blocks-plan.md`
is the brief; this note records where it meets the code and where it had to bend.
`AGENTS.md` wins over the plan.

## The current NAM stage

- `src/audio/amp.rs` is the whole DSP stage: `AmpControls` (the `Copy` controls the
  callback reads), `AmpSettings` (the same plus the capture path, persisted),
  `AmpSlot::{Pre, Post}`, `AmpStage` (one model per path), `AmpPark` (models the
  audio side no longer runs, dropped on the UI thread), `prepare_amp_*` and the
  catalog scan.
- `PathMix::amp` carries the controls; `PathProcessor::run_front` runs the amp when
  its slot is `Pre`, `finish_block` when it is `Post`.  The stage adds no latency.
- Trims, bypass and the model swap all fade inside `AmpStage`; a model taken from
  the queue runs its receptive field of silence first, then fades in.
- `nam-rs 0.4` parses and runs the model.  `Model::process_buffer` allocates
  nothing; the crate pins that in its own `rt_safety` test.  `expected_sample_rate`
  is 48 kHz unless the file says otherwise; **nothing resamples**, and a capture at
  another rate does not install (`AmpFile::RateMismatch`).

## Threading

- The UI builds, resets and prewarms a model (`app::amp::sync_amp_slot`), then
  `PathControls::queue_amp` puts it in `pending_amp: Mutex<Option<AmpCommand>>`.
- The callback `try_lock`s that at block start (`AmpStage::begin`); a replaced model
  goes to `AmpPark` (a `Mutex<Vec<Model>>` with fixed capacity), and only the UI
  thread may drain the park.  In fact nothing drained it during a run: only
  `AudioEngine::replace_run` and `Drop` flushed it.  The stack flushes every
  path's NAM and cab parks once per UI frame.
- The plugin chain has a stronger version of the same idea: commands through a
  queue, slots handed back through a return queue, the audio thread never builds or
  destroys anything (`audio/plugins/chain.rs`).
- DSP time is measured per path per block (`DspMeter`), published through
  `PathControls::store_dsp_time` and `Shared::total_dsp`.

## Presets

- Preferences and sessions are RON (`app/settings`).  One path is a `PathPref`;
  `PathPref::amp: AmpSettings` holds the single stage.
- There is no schema number for a field that can be read from its default:
  `PREFERENCES_SCHEMA` is bumped only when an old save cannot load.  A new
  `nam_blocks` list with `#[serde(default)]` loads old saves, so the plan's
  "bump the schema version" is not needed and `AGENTS.md` wins.

## UI

- The chain is a fixed list of pinned rows plus a scrolling effects list
  (`ui/pages/console/chain.rs`): Pre EQ, gate (pre), amp (pre), Effects n,
  + Add effect, amp (post), Cab, gate (post), Post EQ.
- `ChainStage`/`ChainSelection`/`Sheet` name each stage; `Sheet::Amp` edits the one
  stage (`ui/sheets/amp.rs`) from the models folder.
- The DSP overload warning already reacts to the *peak* (error at 95 %) and to the
  smoothed share (warning at 70 %), with hysteresis (`app/issues.rs`).  The
  meter already shows avg and peak in Diagnostics.

## The block model (Phase 1)

- `NamStack` is the published order: up to 16 `NamBlockControls`, each with an
  `id` the UI assigns, never reuses and saves nothing about.  A preset loaded
  into a live path numbers its blocks past every id that path still runs.  It is `Copy`, so the
  callback takes it with one `try_lock` and moves nothing.
- The runtime keeps one `NamSlot` per block id, not per index, so reordering the
  stack moves no model and restarts no fade.  A block that leaves the stack fades
  to dry at its old place over the bypass fade, then parks its model; a block
  that is added gets a free slot.  When the order of audible blocks changes (a
  reorder, a move to the other side of the effects), the touched group dips to
  silence over the bypass fade, switches at zero and fades back.  The models run
  through the dip: one model's state cannot play in two places, so there is no
  crossfade between the two orders.
- Models cross the same way they always did: the UI queues a prewarmed model for
  one id (`NamPending`), the callback takes it at block start, and a model it
  replaces goes to `NamPark` for the UI thread to drop.
- Each block's own state — trims, the level match, bypass and its fades, the
  warm-up, the outgoing model of a swap — lives with its id.
- Bypass is a true skip (no inference when the wet side is at 0), fade 10 ms
  (the single stage faded over 5 ms).  A model swap fades over 35 ms, and only
  during the fade do both run.
- A full command queue drops a command for a block that left the stack first,
  the oldest only as a last resort, and always keeps the new one.

## Level matching (Phase 6)

`*_level_dbu` in a `.nam` is the analog level at 0 dBFS.  A signal at x dBFS out
of block A is x + out(A) dBu, which block B reads as x + out(A) − in(B) dBFS, so B
gets `out(A) − in(B)` dB, clamped to ±36 dB.  "Before" is the previous block in
the same group that runs: a bypassed block, or one without a model, passes the
signal dry and is skipped.  A block with nothing before it, without calibration
on either side, or with Auto level off gets 0 dB, so a migrated single stage is
unchanged.

## Real-time safety (Phase 1's guard)

`audio::rt_guard` wraps the crate's allocator in a debug build and counts what a
thread allocates while a guard is held; the mixer's and the input callback's blocks
hold one.  It counts and never panics: a hosted VST3 or CLAP plugin may allocate
inside its own `process`, and aborting the callback for a third party's allocation
would be worse than the allocation.  The tests enforce the zero on the chains
RigWerk builds itself (`audio::mixer::rt_tests`), including adding, bypassing,
reordering and removing NAM blocks, replacing an audible model and removing an
audible block, with audio running.  A debug build also publishes the count:
Diagnostics, Performance shows it, and the log says so once when it leaves 0.

## Migration null test (Phase 2)

`scripts/nam-null-test.sh <dir>` renders a fixed stereo input through the old
`AmpStage` at e429fee and through a migrated one-block stack, for a tiny and a
synthetic WaveNet, at unity, with trims, after the effects and across a bypass
toggle.  The difference is 0 in every case (threshold 1e-6).  The bypass case
skips the fade windows, since the fade went from 5 to 10 ms.

## Load measurement

A capture is measured once on the loader thread (200 buffers, mean and longest,
as a share of the block period) and cached per file, rate, block size and width.
The estimate shown before the measurement is a per-architecture constant (12 %
for a WaveNet at 48 kHz, 7 % for LSTM, scaled by rate).  It is coarse — the
architecture string does not name a lite or feather width — so a light capture
reads high for the second before its measurement replaces it.  The value is a
per-model number, not a per-block runtime measurement: the per-path DSP meter
already covers what the block really costs.  The block size is the output step
the driver opened, else the requested output buffer.  A capture measured before
the bridge reports its step keeps that figure until its capture, rate, run or
width changes.

## Heavy presets (Phase 5)

After a preset load the projected NAM load of every live path is summed and
compared with the DSP warning share (70 %).  Over it, a banner lists the heaviest
blocks across paths and offers "Bypass suggested (N)": the heaviest blocks, in
order, until the projection fits.  It never bypasses anything by itself.  The
NAM sheet's overload button bypasses the same set, or this path's heaviest block
when the projection fits but the live DSP is still up.

## Where the plan had to bend

1. **Position in the chain.**  NAM blocks are ordered among themselves and can sit
   before or after the plugin effects — the two positions the built-in amp has.
   They are not interleaved *between* plugins: the plugin chain is an ordered list
   of loaded plugin slots with their own latency and handover, and a NAM block is
   not a plugin.  The console draws the pre group before the effects and the post
   group after, and a block moves between the groups in its sheet.
2. **Preset schema.**  No version bump; the old `amp` field is read and folded into
   `nam_blocks` on load, then never written again (see above).
3. **Resampling.**  Still out of scope.  A capture at another rate stays
   uninstalled with the message it has today; with several blocks that message is
   per block.
4. **Background loading.**  The plan asks for it and it is worth it: parse, build
   and the load measurement run on a loader thread, and the UI only queues the
   finished, prewarmed model.
5. **Cost measurement** is per model at load (rate and buffer size), not per block
   at runtime: timing every block inside the callback would cost more than it
   tells, and the per-path DSP meter already covers the real load.  The estimate
   is scaled by rate only — a model's cost per *sample* is fixed, so its share of
   the block does not depend on the buffer size beyond fixed overhead.
