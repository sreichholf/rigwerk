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

## Remaining risks after the fixes

These are recorded so the fixes are not overclaimed:

- **M2:** UI-side chain holds now cause a dry bypass rather than an audio block.
  `refresh_rate`/`ensure_loaded`/`plugin_views`/`service_plugins` can still hold
  the chain for a long time; a plugin processing error still formats a string on
  the audio thread; `vst3-host`'s internal level mutex remains untouched.
- **M3/M4:** a driver that never returns from teardown leaves a parked thread
  and a visible error; the 15 s watchdog tells the user to restart RigWerk but
  cannot release a stuck driver. Late teardown errors are suppressed for the
  state machine but may still appear in the activity log/status tile.
- **M5:** no ASIO hardware or Windows desktop was available, so callback
  timing, `outputReady` behavior, and driver stop/dispose semantics are
  compile-verified only. `outputReady` is issued one wake-up later and can
  coalesce; a driver that requires it from the callback thread may behave
  differently.
- **M6:** gain is applied per block, not ramped per sample, so large slider
  moves can still step; the H3 ramping issue remains.
- **H1 transition band:** with 32 taps the anti-alias transition is wide. For
  96→48 kHz the measured response is about −9.5 dB at 24 kHz, −18 dB at 26 kHz
  and −55 dB at 30 kHz, so content just above the output Nyquist (24-28 kHz) is
  only partially rejected and aliases into 20-24 kHz. 192→44.1 kHz is worse.
  More taps would narrow this at higher CPU cost.
- **H1/H2 1:1 filtering:** drift is enabled by default, so a 1:1 bridge runs
  through the 0.475·Fs lowpass even at 0 ppm (about −1.5 dB at 20 kHz and
  −4.3 dB at 22 kHz at 48 kHz). A true bypass only happens with drift off.
- **H2 limits:** ±500 ppm is a hard correction ceiling; a larger clock offset
  eventually hits the excess purge or an underrun. Look-ahead is now 16 source
  frames (about 0.36 ms at 44.1 kHz). `drift_ppm` is instantaneous, so the UI
  drift readout responds to block-level fill jitter.
- **H2 purge:** a large cushion reduction still pops `fill - target` frames;
  not resetting the resampler turns that into a short crossfade rather than an
  instant reset. This path is rare and pre-existing.
- **ASIO two-driver startup:** when the output and input are two different ASIO
  drivers, the output worker still starts before the input driver reports its
  rate, so its first callback may design the filter once. This is startup-only
  and silent (the ring is priming), but it is the one remaining kernel build on
  an audio thread. Fixing it needs a prepare/start split for the ASIO input.

## Not fixed yet

The non-blocking findings still open in `REVIEW.md` are plugin lifecycle/state
(H5-H9), device identity and presets (H12-H16), latency reporting (H4), Linux
classification and quantum (H11, H17, H18), UI data honesty/layout/
accessibility/diagnostics (U1-U7), and build/package hygiene. H1, H2, H10 and
the M3/M4 runtime residuals listed above are addressed by this branch.
