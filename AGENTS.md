# Agent guidelines

Defaults for coding agents (and humans) working on RigWerk.

The MSRV is Rust 1.95.0 (`rust-toolchain.toml`). Leave that pin unless the
dependency that forced it changes. The license is GPL-3.0-or-later.

`REVIEW.md` and `REVIEW-FIXES.md` record a finished review.  The
must-fix list in `REVIEW.md` is history.
`docs/mockups/dual-out-plan.md` records the multi-output work that has
landed. `docs/mockups/live-issues-plan.md` records the Live Issues strip; its
open items are done.  The sink monitor behind the silence row reads a
live PipeWire graph (see "A PipeWire graph without hardware"), but a
sink starving under Low latency has not been reproduced.

## Start from master

Always fetch and base the work on the latest `origin/master` unless the
user explicitly asks for another base:

```bash
git fetch origin
git switch -c <area>/<topic> origin/master
```

If the branch has already fallen behind, rebase it onto `origin/master`
before starting. Do not continue on a stale or already-merged branch.

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

CI (`.github/workflows/ci.yml`) also compiles the ignored hardware tests
(`cargo test --locked --no-run -- --ignored`) and runs
`cargo build --locked --release`. That release build is CI's gate, not
the local one.  CodeQL (`.github/workflows/codeql.yml`) scans the Rust
code and the workflows on pushes to `master`, on pull requests, and
weekly.  Its alerts land in the repository's Security tab.

System libraries are installed by `.cursor/install.sh`. The package list
CI uses is in `.github/workflows/ci.yml`.

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

- `src/audio`: the bridge, DSP, and device backends. `audio.rs` is the
  map of that crate.
- `src/app`: window state, startup, preferences, device selection, and
  bridge control. It does not draw. The frame shell still lives in
  `app.rs`; that is debt under Separation, and new paint does not go
  there.
- `src/ui`: egui. One module per sidebar page under `ui/pages`. Shared
  controls live in `ui/widgets`. Colours and type scale live in
  `ui/theme.rs`.
- Platform code stays in `audio/linux` and `audio/win`, behind `cfg`.
  The `linux` module is also compiled for tests on other targets
  (`alsa`, `pipewire`). `duplex`, `hotplug`, and `native` stay
  Linux-only. The `win` module is Windows-only.
- New items are `pub(crate)` unless `audio.rs` already re-exports that
  kind of type. The UI holds plugin descriptors and published views,
  never the plugin instances.

Read the `//!` at the top of a file before editing it. That comment is
the contract. Match the comment style around you: module docs say what
the file is for and the constraint that is easy to miss, and sentences
in those comments are separated by two spaces.

## Separation

Dependencies point one way: `ui` → `app` → `audio`.  `audio` does not
import `app` or `ui`, and it does not import egui.

- `audio` is DSP, the bridge thread, device backends, and plugin
  instances.  A page holds `PluginView`, never an instance.
  `PluginGuiWindow` (`app/plugin_gui.rs`) is the editor, and the only
  place outside `audio` that holds an instance lock.  Do not put another
  `Arc<Mutex<Plugin>>` on a page, and do not move that window into
  `audio` (the audio crate does not own a window).
- `app` is state, startup, preferences, device selection, and bridge
  control.  It does not draw, and it does not import `ui`.  A method
  that changes the engine, a preference, or path state is implemented
  in `app/`.
- `ui` draws.  A page calls a `RigWerkApp` method.  It does not read
  `engine.shared`.  It does not call `engine.set_*` for a value an app
  field already stores (gains, cushion, path mix): add the setter on
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
- Preferences store the audio types (`EqSettings`, `GateSettings`,
  `OutputDeviceSettings`, `PluginPref`).  Do not add a parallel struct.

`RigWerkApp` is the composition root.  Its methods are implemented from
`app/` and from `ui/pages`.  That split stays.  What does not stay is
`app` depending on `ui`.

Known debt.  Do not spread it, and do not fix it with a rewrite.  One
step is one PR.  Do not start the next step until the one before it is
on `master`.

1. Move `impl eframe::App` (the frame, the modals, `handle_panic_mute`)
   from `app.rs` into `ui`.  Leave `logic` and `save` in `app`.
2. Move `ChainSelection` to `app` (it is which stage is selected, and
   `app` already stores it).  Leave `stage_color` in `ui`.  Move
   `OutputDeviceRow` and `output_device_rows` to `app`, so `app::issues`
   stops importing `ui`.  The row is plain data; `device_state` keeps
   the colour and stays in `ui`.
3. The activity log stores a small tone enum, not a `Color32`.  `ui`
   maps the enum when Diagnostics draws the line.  A path colour is an
   index into the theme palette, not a `Color32` stored in `app`.
   `meters` pushes engine state and then smooths the meters: split
   those two, and the engine half must not take `egui::Context`.
4. Pages that call `engine.set_gains`, `engine.set_cushion`, or write
   `path().mix` directly go through one `RigWerkApp` setter each.
   `add_path` reads the mix through `AudioEngine`, not `engine.shared`.
   Tests may still touch `shared`.
5. One helper turns `wasapi_output_mode` / `low_latency_output` into
   the start-config flags.  `app/bridge.rs` and `app/latency.rs` both
   use it.

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
- `REVIEW.md` is a finished review.  Do not treat its must-fix list as
  open work.
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
- The callback `try_lock`s the plugin chain, the EQs, and the gate. If
  the lock is busy, skip that work or keep the last settings and try
  again next block. Never call `lock()` from the callback.
- Load, scan, and destroy plugins on the UI thread, then publish under
  a short lock (`Shared::edit_chain`, `add_plugin`). The audio thread
  does not build plugin views.
- Hold a `DenormalGuard` for the whole block so the EQ, the gate, and
  the inserts run in flush-to-zero. Drop restores the previous mode.
- The resampler kernel is preallocated through `PREALLOCATED_SOURCE_RATE`
  in `resampler.rs`. A rate change inside that range must not allocate.
- Gains, mute, routes, and drift move continuously. Level and route
  changes ramp (`mixer/ramp.rs`). Drift is the slow PI on ring fill in
  `mixer`, capped at `DRIFT_LIMIT_PPM`. The EQ and the gate are
  per-sample and add no buffering.
- A follower that fails to open is marked missing and retried. It does
  not stop the clock bridge.
- ASIO sample copies happen inside `bufferSwitch`. Do not call
  `outputReady`.
- Show the rate and the buffer the driver actually opened. A requested
  size is not a measurement.

Each `AudioEngine::start` replaces `Shared`. An old bridge may only
touch the session it was started with. Do not open a new session while
a lingering teardown still owns the device.

## Preferences

Settings are RON in eframe storage (`app/settings`).

- New fields use `#[serde(default)]`, or a type that already defaults.
  Bump `PREFERENCES_SCHEMA` only when an old save cannot load from field
  defaults. When the file is newer than this build, keep the original
  bytes.
- One unreadable session, plugin, or follower is skipped. A file that
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
- Screenshot runs use `--screenshot <file>` and `RIGWERK_PAGE` (`live`,
  `mixer`, `routing`, `devices`, `sessions`, `plugins`, `tuner`, `guide`,
  `diagnostics`). `RIGWERK_MIXER_STAGE` selects a Mixer stage.
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
