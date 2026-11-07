# Agent guidelines

Defaults for coding agents (and humans) working on RigWerk.

The MSRV is Rust 1.95.0 (`rust-toolchain.toml`). Leave that pin unless the
dependency that forced it changes. The license is GPL-3.0-or-later.

`REVIEW.md` and `REVIEW-FIXES.md` record a finished review.
`docs/mockups/dual-out-plan.md` records the multi-output work that has
landed. `docs/mockups/live-issues-plan.md` is still open: the Issues
strip is on Live, but the card warnings still need migrating into it and
later producers are not built.

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
the local one.

System libraries are installed by `.cursor/install.sh`. The package list
CI uses is in `.github/workflows/ci.yml`.

`Cargo.lock` is committed. Change it only by running Cargo. Hardware
tests stay `#[ignore]` and only play silence.

Windows-only code, when you are not running the Windows CI job:
`cargo check --target x86_64-pc-windows-gnu`.

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
  bridge control. Nothing in here draws.
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
  `mixer`, `routing`, `devices`, `sessions`, `plugins`, `guide`,
  `diagnostics`). `RIGWERK_MIXER_STAGE` selects a Mixer stage.
  Screenshot mode drops Wayland so the requested window size sticks.

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

- Title: same style as a commit subject.
- Body: short bullets only:
  - **What**: the basic change, a few bullets.
  - **Testing**: what was run (and what was not, e.g. no hardware).
  - **Open**: known limits, only if any.
- No restating the diff, no review transcript.
