# Follow-up: review findings

This file records the work done after `REVIEW.md` was committed. The six
blocking findings (M1-M6) were implemented by independent agents on
`review-fixes`; each fix was checked by an independent read-only reviewer.
The remaining findings then continued on `fix-remaining`.

## Must-fix commits

| Finding | Commit | Subject |
| --- | --- | --- |
| M1 | `b6ab061` | Make the audio ring lock-free |
| M2 | `59992eb`, `f2770c9` | Never block the audio callback on plugin locks; keep the CLAP audio path lock-free and rate-safe |
| M3 | `da24c64`, `9ae88ac` | Give every bridge start its own shared session; give preview its own shared session |
| M4 | `82af36b`, `84c6e5b` | Make bridge stop asynchronous and detect dead sessions; harden stopping against late errors and hung teardown |
| M5 | `9b211fc` | Process ASIO buffers inside the driver callback |
| M6 | already fixed by `9b211fc` | ASIO input gain is read per callback; no separate commit was needed |

### Reviewer verdicts

- **M1:** PASS. SPSC ring memory ordering, full/empty conditions, wrapping,
  `ring_frames`/`ring_fill`/`reset`, and no per-frame ring lock all verified.
- **M2:** PASS after follow-up. Audio callback uses non-blocking locks end to
  end; CLAP host-state mutex removed from the audio path; `start_processing`
  moved to load time; `add_plugin` rate race closed.
- **M3:** PASS after follow-up. Real starts and preview each get a fresh
  `Arc<Shared>`; old bridge threads keep the old session and cannot write into
  the new one. Preview no longer resets a lingering session.
- **M4:** PASS after hardening. Stop is asynchronous; `Stopping`, 12 s start
  timeout, 4 s frames heartbeat, clean-exit detection, panic capture, 15 s
  teardown watchdog, and pending-restart cancellation are in place.
- **M5:** PASS. ASIO buffers are read/written only in
  `bufferSwitch`/`bufferSwitchTimeInfo` (plus the prescribed silence prefill
  before `driver.start()`); the channel/worker buffer path is gone; callback
  contexts are CAS-owned per session; no blocking mutex or allocation on the
  normal callback path.
- **M6:** PASS. Every ASIO input path reaches one per-callback gain read; no
  cached session gain remains.

## Second phase: remaining fixes

| Finding | Commit | Subject |
| --- | --- | --- |
| H10, M3/M4 residuals | `e4e06c6` | Service hidden windows and detect input stalls |
| H1, H2 | `8175398` | Use a band-limited resampler with continuous drift steering |
| H1 review follow-up | `14f8979` | Fix the resampler's last phase bin and design the filter off the audio thread |
| ASIO startup follow-up | `0bb1d75` | Open the ASIO output input first so its filter is designed off the callback |

### What changed

- **Hidden-window servicing (H10).** Non-painting maintenance now runs in
  `eframe::App::logic`, which eframe also calls while the window is hidden or
  minimized; `ui` is drawing plus screenshot capture only. Engine watchdogs,
  device watching, plugin scanning, CLAP main-thread/timer servicing and plugin
  editor events therefore keep running when minimized.
- **Input-side liveness.** Every input callback advances an `input_frames`
  counter (cpal and all ASIO paths). The running heartbeat now requires both the
  output frame and input frame counters to advance, so a dead input stream
  becomes a visible error instead of a silent "Running".
- **Late teardown errors.** While the engine is `Stopping`, the UI no longer
  promotes the old session's `error`/status into a red bridge error.
- **Preview teardown.** A preview start is refused while a previous real bridge
  thread is still releasing its device, so Preview cannot mask a live hardware
  session.
- **Resampler (H1).** The four-point cubic was replaced with a 32-tap,
  512-phase windowed-sinc interpolator, cutoff at
  `0.5 * min(output/source, 1) * 0.95`, per-phase normalized, with a fixed
  history ring and no allocation in `next`. Downsampling consumes every
  intervening source frame.
- **Drift steering (H2).** Drop/duplicate frame splices are gone. A PI
  controller on ring-fill error trims the resampler ratio continuously, clamped
  to ±500 ppm, and reports the applied trim through `Snapshot.drift_ppm`.
- **Resampler phase fix.** The original table wrapped the blend from the last
  phase (fraction 511/512) back to the fraction-0 row while keeping the same tap
  base, which returned the previous sample as the blend approached 1. The table
  now has a fraction-1.0 row at the same alignment, and `second_phase` is
  `first_phase + 1`. The new test
  `fractional_phase_blend_does_not_jump_a_sample` fails on the old wrap with a
  worst error of 1.18 and passes now with 3.1e-5.
- **Filter design off the audio callback.** `OutputMixer` configures the
  resampler before the stream/driver starts for cpal, WASAPI, ASIO duplex, and
  ASIO output with a cpal input. The ASIO output path now opens the cpal input
  first so the real source rate is known when the output worker is created.

### Reviewer verdicts for the second phase

- **`e4e06c6`:** PASS. Hidden-window servicing, input heartbeat, `Stopping`
  error suppression, and the preview guard were all verified; no double
  execution per frame and no start during `Stopping`.
- **`8175398`:** FAIL. The reviewer found the phase-table wrap described above;
  the drift PI controller, clamp, resets, and no-allocation claims were sound.
- **`14f8979`:** PASS. Re-measured worst interpolation error dropped from 1.18
  to 3.1e-5 at 1:1 +20 ppm; the same improvement holds for 44.1↔48 kHz and
  44.1→192 kHz. Kernel design is off the callback on cpal, WASAPI, ASIO duplex,
  and ASIO output with cpal input.
- **`0bb1d75`:** compile-verified only. It implements the reviewer's suggested
  ordering (open the input before the ASIO output worker), but no Windows
  desktop or ASIO hardware was available to execute it.

## Verification run here

Rust 1.99.1 (musl) and the Linux build dependencies were installed in this
container. During the must-fix phase the container policy forbade tests, so
M1-M6 were only compiled and built. The user then allowed `cargo test` for
RigWerk, and the second phase ran the suite.

```sh
source "$HOME/.cargo/env"
cargo fmt --check                                            # pass
cargo check                                                  # pass; 2 pre-existing dead-code warnings
cargo check --target x86_64-pc-windows-gnu                   # pass
cargo check --all-targets                                    # pass
RUSTFLAGS="-C target-feature=-crt-static" cargo build        # pass
RUSTFLAGS="-C target-feature=-crt-static" cargo test         # 43 passed, 0 failed, 1 ignored
```

The ignored test is `audio::hardware_tests::hardware_bridge`, which opens real
audio devices. A plain `cargo test` fails at link time on this Alpine image with
`cannot find -lasound` / `cannot find -lxcb`; the `-crt-static` override is an
environment packaging workaround, not a source change.

## Third phase: completing the partial fixes

A second round of seven independent reviewers (one per area, all judging the
cumulative change at `66fa9a2` rather than single commits) confirmed M1, M3,
M5, M6 and H10, rated M2, M4, H1 and H2 as partial, and found regressions
introduced by the fixes. This phase completes those four and fixes the
regressions. It is not committed yet.

### Session lifecycle (M4)

- **`Error` now stops the bridge thread.** `enter_error` sets the stop flag.
  Before, a thread that reported an error but stayed alive (a WASAPI or ASIO
  output that stopped, a device loss while an alert was already shown) kept its
  device. The first Start after such an error was then always refused ("still
  shutting down"), and Preview was blocked for good.
- **Driver opens that time out are joined, not detached.** On the WASAPI
  (4 s) and ASIO-output (6 s) open timeouts, the bridge thread now waits for
  the open thread instead of leaving it running. The engine's start timeout and
  teardown tracking cover the wait, so a late open can no longer claim the
  device after the cpal fallback or a new session has opened it.
- **ASIO output open errors are reported.** Failures in `create_driver` and
  `buffer_size` now go back through `ready`, so the user sees the real reason
  instead of "closed channel".

### ASIO callback (M5)

- **`outputReady` is no longer called.** It was issued later from the bridge
  thread, without checking that the driver supports it, and calls could merge.
  A late call can tell a driver a buffer is ready after it has started playing
  it. Leaving it out is the ASIO default and always correct: the filled buffer
  plays at the next switch.
- **A panic in the callback no longer aborts the app.** `dispatch` wraps
  `process` in `catch_unwind`. On a panic it silences the output buffers, stops
  touching that context, and the bridge ends with an error message.
- **Driver reset requests end the session cleanly.** `kAsioResetRequest`
  (rate or buffer size changed in the driver's control panel) is now accepted.
  The bridge stops with "the driver changed its sample rate or buffer size",
  instead of running on at a stale rate or reporting an unplug.
- **The callback selectors now match what is handled.** `RESYNC_REQUEST` and
  `LATENCIES_CHANGED` are no longer claimed as supported.
- **Hung callbacks are never freed.** When the watchdog reports no callback
  for 2 s, the context is detached and leaked instead of freed, because a
  callback stuck inside `process` may still be using it.

### Plugin chain and CLAP threading (M2)

- **Removed plugins are destroyed outside the chain lock.** `remove` and
  `restore` hand the old slots back; `Shared` drops them after unlocking.
- **CLAP main-thread work no longer takes the plugin's lock.** `on_main_thread`
  and timers (where most editors repaint) now run through `HostState` while
  the audio thread keeps processing. CLAP allows these calls concurrently with
  `process`.
  - The plugin pointer sits behind an `RwLock` that `Drop` clears before
    destroying the plugin.
  - Only a pending parameter flush or rescan takes the instance lock, and then
    with `try_lock`.
  - `service_plugins` collects the handles under the chain lock and services
    them after releasing it.
- **Parameter sync reads the plugin outside the chain lock.** The 250 ms
  editor sync takes the chain lock only to fetch the handle and to store the
  result, and stores it only if the slot still holds the same instance.
- **A slider drag refreshes one parameter, not all.** `set_parameter` used to
  rebuild the whole VST3 parameter list (twice) on every drag event; it now
  formats only the moved control. `has_editor` is cached at load, so drawing
  the insert list never locks a plugin.
- **CLAP `start_processing` runs on the audio thread.** The first `process`
  call makes it, inside the audio-thread scope; at load it had run on the main
  thread, against the CLAP spec. The CLAP main thread is now recorded once from
  the UI thread, so `is_main_thread` is right even for a plugin reloaded on a
  bridge thread.
- **The CLAP event list no longer allocates on the audio thread.** It is
  reserved to the parameter count after every scan.
- **The mixer scratch is sized before the stream starts.** The cpal and WASAPI
  outputs now call `OutputMixer::prepare` too, so their first callback and the
  first block after adding a plugin no longer allocate.

### Resampler (H1)

- **The kernel is a Kaiser-windowed sinc (80 dB stop band) whose length
  depends on the rate pair**, from 32 to 512 taps.
  - The passband is flat to 20 kHz, or 45 % of the lower rate.
  - The stop band starts where content would fold back onto the passband edge,
    so nothing aliases below 20 kHz.
  - Only content folding into the inaudible range between 20 kHz and the output
    Nyquist is partially attenuated.
- **Kernel lengths and the look-ahead they add:**

  | Rate pair | Taps | Look-ahead |
  | --- | --- | --- |
  | 1:1 at 48 kHz | 32 | 0.33 ms |
  | 44.1 ↔ 48 kHz | 56 | 0.58–0.63 ms |
  | 96 → 48 kHz | 64 | 0.33 ms |
  | 192 → 44.1 kHz | 224 | 0.58 ms |
- **No allocation for any source up to 192 kHz.** Storage for the longest
  kernel such a source needs is reserved when the mixer is created, so
  `configure` stays inside it and `next` never allocates.
- **New tests pin the behaviour.**
  - The passband is within 0.1 dB at 20 kHz for 44.1↔48, 96→48, 192→44.1, and
    1:1 at 44.1 and 48 kHz with drift on.
  - Tones that would fold below 20 kHz are below −70 dB: 96→48 at 28.5 and
    40 kHz, 192→44.1 at 24.5 and 30 kHz, 88.2→44.1 at 30 kHz.
  - A 192 kHz reconfigure reuses the reserved storage.

### Drift steering (H2)

- **The fill error is low-passed (0.5 s) before the PI controller, and the
  gains are an order of magnitude lower** (Kp 0.06, Ki 0.0012; damping about
  0.87).
  - The old gains turned the block-quantised fill sawtooth into a correction
    swinging between ±500 ppm.
  - A simulation with block-quantised clocks and jitter reproduced the
    reviewer's result (81 % of blocks saturated at 512/441). With the new gains
    the correction's standard deviation is about 15–65 ppm, with no saturation
    at offsets up to ±200 ppm.
- **An underrun keeps the learned clock offset.** Only the filtered fill starts
  over, so a small cushion does not underrun again while the loop relearns.
- **Switching drift on or off at 1:1 resets the resampler.** Turning it on no
  longer replays history from before the bypass. Turning it off skips the
  look-ahead frames once (about 0.33 ms at 48 kHz).

### Verification

All of the following were run on Windows, which has no Linux target installed:

- `cargo fmt --check`: pass.
- `cargo test`: 51 passed, 0 failed, 4 ignored.
- `cargo clippy --all-targets`: the same warning set as before this phase.

Not verified:
- The Linux build was not compiled. The touched code is platform-neutral apart
  from the Windows-only files.
- No ASIO hardware, CLAP plugin, or audio device was exercised.

## Remaining risks after the fixes

These are recorded so the fixes are not overclaimed:

- **M2:**
  - A chain-lock collision still passes that block through dry, without a
    crossfade. Remaining holds that can collide:
    - `refresh_rate` (bridge start or rate change)
    - `ensure_loaded` (opening an editor for a never-loaded slot)
    - `plugin_views` (clones the view list)
    - storing a parameter sync or a drag
    - VST3 editor servicing, which still holds that plugin's own lock. The
      vst3-host API offers no lock-free path.
  - A plugin processing error still formats a string on the audio thread, once
    per failure.
  - CLAP `stop_processing` still runs from `Drop`, off the audio thread.
- **M3/M4:**
  - A driver that never returns from teardown or open leaves a parked bridge
    thread and a visible error; the watchdog tells the user to restart RigWerk
    but cannot release a stuck driver.
  - Late teardown errors may still appear in the activity log/status tile.
- **M5:**
  - Callback timing, driver stop/dispose, and reset-request behaviour are
    compile-verified only.
  - `sampleRateDidChange` is deliberately ignored (some drivers send it during
    start); only `kAsioResetRequest` ends the session.
- **M6:** gain is applied per block, not ramped per sample, so large slider
  moves can still step; the H3 ramping issue remains.
- **H1:** content between the output Nyquist and `lower rate − 20 kHz` (e.g.
  24–28 kHz for 96→48) is only partially rejected. It folds to 20 kHz–Nyquist,
  above the audible band.
- **H2:**
  - ±500 ppm is a hard ceiling. Larger offsets eventually hit the excess purge
    or an underrun.
  - The loop settles over tens of seconds. A large offset at start-up, or a
    Low-latency cushion, can still underrun once before the integral has
    learned the offset; the cushion then grows as before.
- **H2 purge:** a large cushion reduction still pops `fill - target` frames,
  which the resampler turns into a short crossfade. This path is rare and
  pre-existing.
- **ASIO two-driver startup:** when the output and input are two different ASIO
  drivers, the output worker starts before the input driver reports its rate.
  Its first callback may then design the filter once, on the audio thread. This
  happens only at startup and is silent (the ring is priming), and it no longer
  allocates for sources up to 192 kHz. Removing it needs a prepare/start split
  for the ASIO input.

## Fourth phase: plugin lifecycle and state (H5-H7, H9, H16)

| Finding | Commit | Subject |
| --- | --- | --- |
| H5 | `2af8fba` | Share one init per CLAP module across instances and scans |
| H6 | `478eb70` | Never leak a CLAP plugin or module init on a failed load |
| H7 | `3eb2704` | Restore CLAP parameters and save plugin state for both formats |
| H9 | `3f42c70` | Give CLAP plugins one buffer per declared audio port |
| H16 | `04d9c62` | Keep preferences when one patch or plugin entry cannot be read |

H8 (plugin lifecycle on bridge threads) is not claimed as complete here.

### H9: multiple audio ports

- Every port the plugin declares through `clap_plugin_audio_ports` now gets its
  own `clap_audio_buffer`, in plugin index order, and `process` sets the real
  `audio_inputs_count` / `audio_outputs_count`. A sidechain or multi-output
  effect no longer sees a port count that contradicts its descriptor.
- The main port is the one flagged `CLAP_AUDIO_PORT_IS_MAIN` (index 0 when no
  port is flagged). A mono main port gets the mono sum and feeds both output
  sides; a main port with more than two channels gets left/right on its first
  two channels and silence on the rest, and only its first two output channels
  are read.
- Auxiliary input ports are silent; auxiliary output ports are allocated and
  ignored. Zero-channel ports get a null `data32` and `channel_count == 0`.
- The layout is read once, after state restore and before `activate`, because
  CLAP only allows port changes while the plugin is inactive. A later
  `clap_host_audio_ports.rescan` that changes the port list, channel counts, or
  main flag sets an atomic flag; the insert stops (without allocating or
  reallocating on the audio thread) and the panel says to remove and add it
  again. Rescans for names, port type, or in-place pairing are absorbed because
  they cannot invalidate these buffers.
- Malformed port descriptions are rejected before activation: no ports, more
  than 32 ports per direction, more than 64 channels per port, a main port with
  no channels, or a port that cannot be described. The existing RAII load guard
  still destroys the plugin and releases the module on that path.
- The buffer descriptions (pointers, channel counts, masks) are restored from
  the cached layout before every block, so a plugin that overwrites `data32`,
  `data64`, or `constant_mask` cannot leave the host pointing at stale memory.
  Zero-channel ports stay null.

### Reviewer verdict for H9

**PASS WITH FINDINGS**, all addressed in the follow-up:

- The ports-changed path no longer allocates a `String`: `process` returns a
  static message, and the chain skips a slot whose port flag is set before
  calling into the plugin.
- A zero-channel port now gets a null `data32` instead of a one-past-the-end
  pointer, and the placeholder `PortBuffers` no longer panics if its main port
  is asked for.
- The rescan callback ignores flags that cannot invalidate the buffers
  (names, port type, in-place pairing) while still stopping on a port list,
  channel count, flags, or unknown flag.
- A port change is visible in the insert list even while the bridge is stopped
  or the insert is bypassed, through the same cached host state the panel
  already holds.
- New tests cover a trailing zero-channel port, pointer/metadata tampering
  between blocks, multiple sub-blocks, harmless versus stopping rescan flags,
  and the panel-visible port-change error.

### Verification for the fourth phase

- `cargo fmt --check`: pass.
- `cargo test`: 99 passed, 0 failed, 4 ignored.
- `cargo clippy --all-targets`: no new warnings.
- Hardware smoke test on this Windows host: ASIO input (Valeton GP-5, 44.1 kHz)
  into the WASAPI High Definition Audio output at 48 kHz, Low-latency cushion,
  128-frame output step, three seconds, silence only: `Running`, 0 underruns,
  input and output frames advancing, clean stop back to `Idle`.

Not verified: a real multi-port CLAP plugin or CLAP validator was available, so
the port layout is covered by the in-process fake only.

## Fifth phase: mix ramping (H3)

`84ac304` added the ramp types and did not call them, so gain, mute, channel
enable and routing still stepped once per block. This phase wires those ramps
into the output mixer.

| Finding | Commit | Subject |
| --- | --- | --- |
| H3 | (this phase) | Crossfade gain, mute, and routing instead of stepping per block |

### H3: gain, mute, channel enable, and routing

- Each route, each output channel, mono, and the output level keeps its own
  per-sample ramp. A change takes 6 ms at any block size, and a ramp that
  crosses a callback boundary keeps its slope instead of restarting.
- Mute and monitor are not a hard gate. They fold into the level ramp, so
  muting, unmuting, and moving the fader in the same moment is one continuous
  move. The ramp lands on the exact target and then holds it, so a settled
  control adds no extra modulation.
- The first block adopts the live settings directly. Starting the bridge does
  not fade in from silence or from unity gain.
- Routing, output-channel enable, and mono still run before the pre EQ and the
  plugin inserts. The level ramp still runs after the post EQ, in the same
  place the master fader used to step.
- Input gain and the input-channel switches are unchanged: each input callback
  still reads them once and holds them for that block. A fast move of the
  input fader can still step at the input block boundary. That is the M6
  residual from the earlier phase; it is not the per-block output step H3
  describes.

### Verification for the fifth phase

On this Linux host, Rust 1.99.1:

- `cargo fmt --check`: pass.
- `cargo test`: 106 passed, 0 failed, 1 ignored
  (`audio::hardware_tests::hardware_bridge`, which opens real devices).
- `cargo clippy --all-targets`: no new warnings. The warnings that remain are
  the pre-existing dead-code and style notes outside the mixer.
- The new mixer tests cover a ramp that lands and holds, a repeated target
  that does not restart the slope, a retarget from the current value, the
  first block adopting a non-default level, a level move that continues across
  two callbacks, mute and monitor fades, a route crossfade that leaves the
  other taps alone, an output-channel fade, and a mono crossfade.

Not verified here: no audio device was exercised, so the ramps are covered by
the mixer tests only.

## Not fixed yet

The non-blocking findings still open in `REVIEW.md` are latency reporting
(H4), plugin lifecycle placement (H8), Linux classification and quantum
(H11, H17, H18), device identity and presets (H12-H15), UI data
honesty/layout/accessibility/diagnostics (U1-U7), and build/package hygiene.
H1, H2, H3, H5-H7, H9, H10, H16 and the M3/M4 runtime residuals listed above
are addressed by this branch. Input gain still steps once per input callback;
that residual is recorded in the H3 notes above and in the earlier M6 note.

The second review round re-verified every open finding at `66fa9a2`; at that
point all of them, including H5-H9, were still open. It also sharpened several
of them:

- **H7** loses data: after a reload, the next parameter sync overwrites the
  saved CLAP values with the plugin's defaults.
- **H12** applies to Linux `hw:` devices only. ASIO devices come from the
  registry and cannot drop out.
- **H16** also covers `PluginPref`: one bad plugin entry wipes all preferences.
- **H17:** a failed read-back can also clear a quantum the user had forced.
- **U5** overstated keyboard access: egui activates focusable rows with Enter
  and Space. The real gap is that focus is never shown.
- **U7:** a fix must keep a slow repaint tick (100–250 ms), because servicing
  the hidden window relies on it.
