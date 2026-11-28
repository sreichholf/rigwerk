# Agent guidelines

Defaults for coding agents (and humans) working on RigWerk.

The MSRV is Rust 1.95.0 (`rust-toolchain.toml`). Leave that pin unless the
dependency that forced it changes. The license is GPL-3.0-or-later.

## Docs

- `docs/ui-spec.md` is the live UI layout spec and glossary; the glossary
  test in `src/tests.rs` mirrors the unambiguous words of the banned-word
  column of section 2.1.
- `docs/manual.md` is the user manual.
- `docs/feature-ideas.md` holds live ideas, not planned work.
- `docs/screenshots/README.md` is the screenshot set.
- `docs/open-items.md` lists what is known to be missing or limited.

The finished plans and mockups, and the code review (also issue #117), are
history only; do not restore them as open work.

## Start from master

Always fetch and base the work on the latest `origin/master` unless the
user explicitly asks for another base:

```bash
git fetch origin
git switch -c <area>/<topic> origin/master
```

If the branch has already fallen behind, rebase it onto `origin/master`
before starting. Do not continue on a stale or already-merged branch.

## Subagents

Never use a fast subagent unless the user explicitly asks for one.  A
fast model is one whose name ends in `-fast`.

## Commands

While editing, run the test you are changing:

```bash
cargo test --locked <filter>
```

Run the app with `cargo run --release`.

A change is done when these exit 0:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
```

The hardware tests read the devices from the environment: `RIGWERK_IN`,
`RIGWERK_OUT`, optionally `RIGWERK_OUT_BACKEND` and `RIGWERK_BRIDGE_BUFFER`
(see `src/audio/hardware_tests.rs`).

CI (`.github/workflows/ci.yml`) also compiles the ignored hardware tests
(`cargo test --locked --no-run -- --ignored`) and runs
`cargo build --locked --release`. That release build is CI's gate, not
the local one.  A `Deny` job runs cargo-deny against `deny.toml`, whose
`allow` list takes a deliberate edit when a dependency licence changes.
A pull request that only touches markdown or `docs/` runs nothing: no
test reads those files.
CodeQL (`.github/workflows/codeql.yml`) scans the Rust
code and the workflows on pushes to `master` and weekly, not on pull
requests.  Its alerts land in the repository's Security tab.

System libraries are installed by `.cursor/install.sh`. The package list
CI uses is in `.github/actions/linux-deps/action.yml`.

`Cargo.lock` is committed. Change it only by running Cargo. Hardware
tests stay `#[ignore]` and only play silence.

Windows-only code, when you are not running the Windows CI job:
`cargo check --target x86_64-pc-windows-gnu`.

### A PipeWire graph without hardware

A container has no sound card, but PipeWire runs with a null sink.  This
is how the sink monitor was checked; use it for any ignored test that
needs a live graph.  The tone goes to a virtual sink, never to a device,
so the "hardware tests only play silence" rule still holds for committed
tests.

```bash
apt-get install -y pipewire pipewire-bin wireplumber dbus
export XDG_RUNTIME_DIR=$(mktemp -d) && chmod 700 "$XDG_RUNTIME_DIR"
pipewire &
dbus-run-session -- wireplumber &   # without D-Bus it exits; nothing links
pw-cli create-node adapter '{ factory.name=support.null-audio-sink
  node.name=test_sink media.class=Audio/Sink audio.position=[FL FR]
  object.linger=true }'
wpctl status                        # test_sink is the default (*) sink
pw-cat -p tone.wav &                # e.g. a 300 Hz sine at 0.5
cargo test --locked sink_monitor_reads_the_default_sink -- --ignored --nocapture
```

The test prints `Some(~0.5)` while the tone plays.  An idle sink is
suspended and reads `None`, not `0.0`.  `pw-link -l` shows whether
WirePlumber linked a stream.

### Real VST3 plugins

The `vst3_fixture` tests load Steinberg's VST3 SDK samples (`again`,
`adelay`, `host-checker`).  `scripts/vst3-fixtures.sh <dir>` clones the
SDK at a pinned tag and builds them into `<dir>/plugins`; they are never
committed.  Without `RIGWERK_VST3_FIXTURES` the tests pass without doing
anything.  With it set, a missing bundle fails.  CI runs them on Linux
and Windows.  The editor test needs an X server on Linux, so run it
under `xvfb-run`:

```bash
scripts/vst3-fixtures.sh .vst3-fixtures
RIGWERK_VST3_FIXTURES=.vst3-fixtures/plugins \
  xvfb-run -a cargo test --locked vst3_fixture -- --nocapture
```

The Linux build needs the VSTGUI packages in `.cursor/install.sh`.  The
editor test opens AGain's VSTGUI editor in an X11 window, so on Linux it
needs `xvfb-run` (or a real X server).  `audio/vst3` scans for bundles
(from `moduleinfo.json`, without loading them), and loads, processes and
opens them.

### Real LV2 plugins

The `lv2_fixture` tests load the LV2 repository's examples (`eg-amp`,
`eg-params`, `eg-sampler`).  `scripts/lv2-fixtures.sh <dir>` clones the
repository at a pinned tag, builds the plugins with meson and copies the
three bundles into `<dir>/plugins`; they are never committed.  Without
`RIGWERK_LV2_FIXTURES` the tests pass without doing anything.  With it
set, a missing bundle fails.  CI runs them on Linux; the script is POSIX
and does not build these on Windows yet.

`eg-amp` covers load, process, a control port and the panel; `eg-params`
covers state (it has only atom ports, so the test loads it directly
through `audio/lv2`); `eg-sampler` covers `state:loadDefaultState` and a
sample loaded through the worker (it has no audio input, so it is loaded
the same way).  It needs libsndfile and libsamplerate to build.

```bash
scripts/lv2-fixtures.sh .lv2-fixtures
RIGWERK_LV2_FIXTURES=.lv2-fixtures/plugins \
  cargo test --locked lv2_fixture -- --nocapture
```

This needs `meson` and `ninja`.

### The NAM migration null test

`a_migrated_stage_nulls_against_the_old_build` checks that a saved single
NAM stage, migrated to one block, plays what the old `AmpStage` played.
`scripts/nam-null-test.sh <dir>` checks out e429fee (the last build with
`AmpStage`) as a worktree in `<dir>/old`, appends a reference render to
its tests there, writes the output to `<dir>/reference` and then runs the
new test against it.  Without `RIGWERK_NAM_REFERENCE` the test passes
without doing anything; with it set, a missing file fails.
`RIGWERK_NAM_CAPTURE=<file.nam>` adds a 48 kHz capture of your own.  The
difference must be at most 2e-5: the vendored nam-rs's vectorized `tanh`
moves a standard WaveNet capture by up to 1.2e-5 when it is driven near
0 dBFS (4.3e-6 on the test's signal), the synthetic one by 2e-7 and the
ReLU one not at all.  The bypass case skips the fades, which went from
5 ms to 10 ms.  The old tree builds in `<dir>/target`; give the new one a
target directory no other worktree builds into, since the test binary has
the same name in every tree.

```bash
CARGO_TARGET_DIR=$PWD/target scripts/nam-null-test.sh .nam-null
```

### The vendored nam-rs

`vendor/nam-rs` is nam-rs 0.4.0 with faster kernels, used through
`[patch.crates-io]`; `vendor/nam-rs/RIGWERK.md` lists what changed.  The
folder and every change in it stay MIT, like the crate, so the changes can
go back upstream: do not copy RigWerk's GPL code into it.  Keep its
upstream style (inline `mod tests`, its own `//!`).  `Cargo.toml` builds
it at opt-level 3 in every profile; its conv tiles only vectorise there.
RigWerk's `cargo test` does not run its tests, and RigWerk's `cargo fmt`
does not reach `vendor/`.  `scripts/nam-rs-parity.sh <dir>` clones
upstream at the tag, puts the vendored `src/` in its place and runs
upstream's checks: rustfmt, clippy, and the whole suite in RigWerk's two
profiles for it, dev at opt-level 3 (debug assertions on, so the real-time
safety tests' allocator check is installed) and release at opt-level 3
without LTO.  CI runs it on Linux.

```bash
scripts/nam-rs-parity.sh .nam-rs-parity
```

## When a test fails

- Fix the source. Change the assertion only when the behavior change is
  intentional.
- Do not delete a test to make the suite pass, and do not weaken it.
- `#[ignore]` is only for a test that opens a real device or a live
  PipeWire graph. Those are `src/audio/hardware_tests.rs` and the ignored
  test in `src/audio/linux/duplex/tests.rs`.
- Unit tests live in a `tests.rs` beside the code they test, declared
  as `#[cfg(test)] mod tests;`. `src/a/b.rs` uses `src/a/b/tests.rs`;
  `src/a/mod.rs` uses `src/a/tests.rs`. Do not write inline
  `mod tests { ... }` blocks. New DSP behavior gets tests there.
- There is no `tests/` directory. The crate is a binary, so integration
  tests could not reach its `pub(crate)` items.

## Layout

- `src/audio`: the bridge, DSP, device backends, and the plugin hosts
  (`audio/vst3`, `audio/clap`, `audio/lv2`). `audio.rs` is the
  map of that crate.
- `src/app`: window state, startup, preferences, device selection, and
  bridge control. It does not draw. The frame shell lives in
  `ui/frame.rs`.
- `src/ui`: egui. The title bar, status bar, banner and the sheet and
  drawer hosts live in `ui/shell.rs`. Menus are `ui/menus`, sheets
  `ui/sheets`, drawers `ui/drawers`. The console is the home screen
  (`ui/pages/console.rs`, one file per strip under `console/`). Setup
  replaces Devices and Guide (`ui/pages/setup.rs`). The tuner is a screen
  of its own (`ui/pages/tuner.rs`). There are no classic pages left.
  Shared controls live in `ui/widgets`. Colours and type scale live in
  `ui/theme.rs`.
- Platform code stays in `audio/linux` and `audio/win`, behind `cfg`.
  The `linux` module is also compiled for tests on other targets
  (`alsa`, `pipewire`). `duplex`, `hotplug`, and `native` stay
  Linux-only. The `win` module is Windows-only.
- New items are `pub(crate)` unless `audio.rs` already re-exports that
  kind of type. The UI holds plugin descriptors and published views,
  never the plugin instances.

## Comments

A comment says what the code cannot.  That is the why, special handling
the types do not express, or the source of an algorithm that is not a
common one.  Write it for an engineer who can already read the function.
Terse.  Two spaces between sentences.  The same rule covers `//!`, `///`,
and `//`, in tests as well as in the implementation.

Do not restate the code underneath.  Do not narrate the control flow, the
order of the steps, or what a function returns when its name and body
already say it.  Do not record history: what a field used to be called,
what an older build did, how the design got here.  A `serde` alias, a
generation counter, or a second buffer is the handling.  When a comment
is still needed, it states the constraint that handling exists for, in
one or two lines.

A module doc is the constraint that is easy to miss, not a tour of the
file.  Read it before editing the file, and change it when that
constraint changes.  Name an uncommon algorithm and where it comes from.
The NAM normalised-loudness target cites NeuralAmpModelerPlugin's
`_SetOutputGain`; a bubble sort, a clamp, a fade, or a ring pop needs
nothing.  Do not copy a longer comment beside the code you are editing.
Cut it to this section.  `vendor/` keeps its upstream comments.

Cut a true comment down to the why and the handling.  The subnormal
guard's module doc also walked through what `DenormalGuard` already
does:

```rust
//! Flushing subnormal floats to zero on the audio threads.
//!
//! When a signal decays into silence, IIR filter states (the EQ biquads) and
//! envelope extra outputs (the gate) pass through the subnormal range on their way
//! to zero.  On x86 every operation on such a value takes a slow microcode
//! path, often a hundred times slower than a normal one, and a decay can stay
//! there for seconds.  At small buffers that is enough to miss a deadline.
//!
//! [`DenormalGuard`] switches the current thread to flush-to-zero (and, on
//! x86, denormals-are-zero) for as long as it lives and restores the previous
//! mode when it is dropped.  The audio callbacks hold one for the length of a
//! block, so the plugin effects processed inside that block see the same mode
//! a DAW would give them, while the driver's own code on that thread runs
//! with whatever mode it had before.  Reading and writing the control register
//! is a couple of instructions and never enters the kernel.
//!
//! Changing the floating-point mode is formally outside Rust's model, which
//! assumes the default environment.  The asm blocks below are not `nomem` or
//! `readonly`, so the compiler cannot move memory accesses (and the float work
//! that feeds them) across the switch; arithmetic between the switches is what
//! the mode is meant to affect, and nothing here depends on subnormal results.
//!
//! On 32-bit x86 only FTZ is set: DAZ is missing on the earliest SSE CPUs,
//! where setting it faults.  Every x86_64 CPU has it.
```

That becomes:

```rust
//! FTZ/DAZ for one audio block.  A decaying IIR or envelope sits in the
//! x86 subnormal microcode path long enough to miss a short buffer.
//!
//! The asm is not `nomem`, so float work cannot move across the mode
//! switch.  32-bit x86 sets FTZ only: DAZ faults on early SSE.
```

The first paragraph is why the guard exists.  The second is handling the
type cannot express: float work must not move across the mode switch, and
DAZ faults on early 32-bit SSE.

Delete a comment that only describes the next item.  `sanitized` trims
and clamps:

```rust
/// Trims the path and puts every value in range.
pub fn sanitized(&self) -> Self {
```

## Separation

Dependencies point one way: `ui` → `app` → `audio`.  `audio` does not
import `app` or `ui`, and it does not import egui.

- `audio` is DSP, the bridge thread, device backends, and plugin
  instances.  A page holds `PluginView`, never an instance.
  `PluginGuiWindow` (`app/plugin_gui.rs`) is the editor, and that file
  is the only place outside `audio` that holds a `PluginHandle`; the
  editor reaches the plugin through it on the UI thread.  Do not put a
  handle or an instance on a page, and do not move that window into
  `audio` (the audio crate does not own a window).  That editor window
  is the only window `app` owns.
- `app` is state, startup, preferences, device selection, and bridge
  control.  It does not build widgets and does not paint, and its
  non-test code does not import `ui`: widgets and painting stay in `ui`.
  It may use an `egui::Context` for the window work that is not drawing
  (the theme and zoom it applies and saves, the pixel scale it reads, the
  repaint timing it asks for, and the screenshot run's capture and
  viewport commands), and a raw window handle it copies from the frame,
  which the `rfd` folder dialogs it opens are parented to.  A method that
  changes the engine, a preference, or path state is implemented in
  `app/`.
- `ui` draws.  A page calls a `RigWerkApp` method.  It does not read
  `engine.shared`.  It does not call `engine.set_*` for a value an app
  field already stores (gains, bridge buffer, path mix): add the setter on
  `RigWerkApp` and call that.

These splits are already right.  Do not merge them and do not wrap
them in a new type:

- `app::calibration` decides when to arm; `audio::calibration` is the
  click, the capture, and the correlator.  The estimate stays in
  `app::budget`.
- `app::bridge` starts, stops, and publishes the numbers the screens
  show; `audio::bridge` is the bridge thread.
- `app::tuner` is the saved settings and the auto-mute; `audio::tuner`
  is the pitch detector.
- `app::plugins` scans and inserts; `audio::plugins` is one chain per
  path.
- `app::devices` selects and watches; `audio::devices` discovers.
- `app::record` picks the folder and the take name and arms;
  `audio::record` is the writer thread, the only place that opens the WAV.
- Preferences store the audio types (`EqSettings`, `GateSettings`,
  `OutputDeviceSettings`, `PluginPref`).  Do not add a parallel struct.

`RigWerkApp` is the composition root.  Its methods are implemented from
`app/` and from `ui/pages`.  That split stays.

## Keep it simple

- A long file that is one algorithm or one page stays one file.
  `audio/tuner.rs`, `audio/shared.rs`, `audio/engine.rs`,
  `app/issues.rs`, and the routing matrices are not split for length.
- Do not add a trait, a newtype, or a wrapper that exists for one call
  site.
- One numeric setting touches the preference field,
  `sanitize_preferences` (or that type's `sanitized`), the `RigWerkApp`
  field, an engine setter, and the widget.  That is the path.  Clamp in
  `sanitize_preferences`, not in the widget.
- `audio.rs` is the in-crate map for `app` and `ui`.  There is no
  library API.  New items are `pub(crate)` unless that file already
  re-exports the same kind of type.

## Audio thread

This applies to an input callback, an output callback, an ASIO
`bufferSwitch`, and a PipeWire process callback.

- No allocation, no blocking lock, no file or network I/O, no plugin
  load or destroy, and no logging that allocates.
- Rings are lock-free and single-producer single-consumer (`ring.rs`,
  and the same pattern on `Shared`). One side writes `head`, the other
  writes `tail`. Do not put a `Mutex` on the sample path.
- The callback `try_lock`s the EQs and the gate. If the lock is busy,
  keep the last settings and try again next block. Never call `lock()`
  from the callback.
- The plugin chain has no lock.  The mixer holds each path's runtime
  (`plugins/chain.rs`) and applies the commands the UI's model queued
  at the start of a block.  A slot it lets go of goes back through the
  return queue: the audio side never destroys one.
- The deferred slot queue and the parks belong to the engine (one set per
  engine, shared by every run it starts), not to a static, so a stopped
  drain cannot leak or block another engine.  `release_slot` is the one
  teardown path for a slot that may have run; a slot built but never run is
  dropped directly.  A stranded slot (a pause that stayed unquiet for
  `PAUSE_STRANDED`, so a block never came out of `process`) is leaked by
  `release_slot` and by the park's drain and drop, on any thread: no
  processor call follows it.  Never destroy, stop or deactivate a plugin
  under a call that never returned.
- Plugin instances have no lock the callback takes.  The chain's slot
  owns each plugin's processor; the UI reaches the rest through its
  `PluginHandle`.  Values cross in the transfer queues (`transfer.rs`).
  A restart or reactivation asks for a pause (`pause.rs`) and does its
  work only once the audio side has stopped the processor, or once no
  block is inside it.
- Load, scan, and destroy plugins on the UI thread.  A structural edit
  goes through the chain model (`PathControls::edit_chain`,
  `add_plugin`), which builds and loads the slot before it sends it.
  The audio thread does not build plugin views.
- Hold a `DenormalGuard` for the whole block so the EQ, the gate, and
  the effects run in flush-to-zero. Drop restores the previous mode.
- The resampler kernel is preallocated through `PREALLOCATED_SOURCE_RATE`
  in `resampler.rs`. A rate change inside that range must not allocate.
- Gains, mute, routes, and drift move continuously. Level and route
  changes ramp (`mixer/ramp.rs`). Drift is the slow PI on ring fill in
  `mixer`, capped at `DRIFT_LIMIT_PPM`. The EQ and the gate are
  per-sample and add no buffering.
- An extra output that fails to open is marked missing and retried. It does
  not stop the clock bridge.
- ASIO sample copies happen inside `bufferSwitch`. Do not call
  `outputReady`.
- Show the rate and the buffer the driver actually opened. A requested
  size is not a measurement.

Each `AudioEngine::start` replaces `Shared`. An old bridge may only
touch the run it was started with. Do not open a new run while
a lingering teardown still owns the device.

## Preferences

Settings are RON in eframe storage (`app/settings`).

- New fields use `#[serde(default)]`, or a type that already defaults.
  Bump `PREFERENCES_SCHEMA` only when an old save cannot load from field
  defaults. When the file is newer than this build, keep the original
  bytes.
- One unreadable session, plugin, or extra output is skipped. A file that
  does not parse at all is kept under the backup key and the app starts
  from defaults. Do not throw away the original text.
- `sanitize_preferences` is what keeps a NaN gain or an illegal rate out
  of the audio math. Extend it when you add a numeric setting.

## UI

- Engine and persistence changes go in `app` or `audio`. A page calls
  those. It does not grow its own copy of the bridge state machine.
- Take colours from `ui/theme.rs` and text sizes from `ts()`. Reuse
  `ui/widgets` before adding a one-off control.
- Meters, scopes, and latency figures come from the live snapshot.
- Screenshot runs use `--screenshot <file>` and `RIGWERK_PAGE`
  (`console`, `tuner`, `setup`). `devices` and `guide` open Setup; anything
  else opens the console. `RIGWERK_SHEET` (`eq`, `gate`, `nam`, `cab`,
  `effect`, `path`, `output`, `match-paths`, `scope`, `diagnostics`) opens a
  sheet on the selected path and
  `RIGWERK_DRAWER` (`browser`, `issues`) a drawer.
  Screenshot mode drops Wayland so the requested window size sticks.
  The README set and how to retake it: `docs/screenshots/README.md`.

## Commits

- One commit per logical change. Review fixes are not separate commits:
  fold them into the commit they fix (`git commit --fixup <sha>`, then
  `git rebase -i --autosquash`) before the PR is opened or merged.
- Rewriting history is fine on your own unmerged branch (use
  `git push --force-with-lease`). Never rewrite `master` or someone
  else's branch.
- Subject: imperative, sentence case, no trailing period, under ~70 chars.
- Body: a few terse bullets describing the basic change. No essays, no
  test logs, no review history.

```
Flush subnormal floats in the audio callbacks

- Add DenormalGuard (FTZ/DAZ on x86, FZ on aarch64), restored on drop
- Hold it in OutputMixer::render and the input callbacks
```

## Pull requests

- Branch: always a descriptive name, `<area>/<topic>` (e.g. `ci/codeql`,
  `docs/refresh-readme-screenshots`).  Push a generated session branch
  under such a name before opening the PR.
- Title: same style as a commit subject.
- Body: short bullets only:
  - **What**: the basic change, a few bullets.
  - **Testing**: what was run (and what was not, e.g. no hardware).
  - **Open**: known limits, only if any.
- No restating the diff, no review transcript.
