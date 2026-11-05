# RigWerk code review

**Date:** 2026-11-04
**Scope:** full repository (`Cargo.toml`, `Cargo.lock`, all of `src/`)
**Method:** static review, split across seven independent read-only domain reviews
(real-time core, Windows backends, Linux backends, plugin hosting, application
state/devices, UI/UX, build/repo). Findings were cross-checked between domains and
against the source; only findings supported by code are kept here.
**Verification limits:** there is no Rust toolchain in the review container, no
audio hardware, and no display. No build, lint, unit test, hardware test, or
instrumented test was run. Line numbers refer to the tree at the review commit.

---

## Verdict

RigWerk is an ambitious and well-structured audio-bridge prototype. The
backend-neutral audio code is separated cleanly from the Windows and Linux
backends, the in-code explanations are unusually good, and driver mismatches are
reported instead of hidden. It is **not release-ready**: the audio callback is
not real-time safe, bridge and plugin lifecycles lack ownership, several
platform paths can be torn down by their own diagnostics, and parts of the UI
present synthetic data as measurements.

The six items under **Must fix before further feature work** are blocking. The
rest are grouped by domain and severity.

---

## Must fix before further feature work

### M1. The audio callback is not real-time safe: per-frame ring mutex

**Evidence:** `src/audio/shared.rs:15,512-534` stores the bridge ring as
`Mutex<VecDeque<f32>>`. `push_frame` and `pop_frame` lock it per stereo frame.
Producers: `src/audio/input.rs:113`, `src/audio/win/asio/input.rs:176,347`,
`src/audio/win/asio/duplex.rs:179`. Consumer: `src/audio/mixer.rs:15-23`.
`ring_frames()` locks the same mutex three more times per output block
(`src/audio/mixer.rs:151,164,170`), and the UI `snapshot()` takes it too
(`src/audio/shared.rs:546-568`).

**Why it matters:** `std::sync::Mutex` has no priority inheritance. A
normal-priority UI or input thread preempted inside the critical section can
block the MMCSS/RT output callback for a scheduler quantum, causing an xrun at
small buffers. The lock traffic is also roughly 200k lock/unlock pairs per
second at 48 kHz stereo.

**Fix:** replace the `VecDeque` + `Mutex` with a preallocated single-producer /
single-consumer ring using atomic read/write indices. Push and pop whole blocks,
read the fill once per block, and never take a blocking lock from an audio
callback.

### M2. The plugin chain mutex is held across UI work, and plugin loading happens under it

**Evidence:** `Shared::add_plugin` claims "Load outside the lock so a slow plugin
does not block playback" (`src/audio/shared.rs:159-173`), but calls
`chain.push()` -> `Slot::loaded` -> `load_plugin` / `start_processing` while
holding the chain mutex. The output callback takes the same mutex with a blocking
`lock()` (`src/audio/shared.rs:280-288`). The UI takes it every frame for
`plugin_views()` and `service_plugins()` (`src/audio/shared.rs:253-265`,
`src/audio/plugins.rs:477-489`), cloning every parameter view and running
arbitrary CLAP `on_main_thread` / timer callbacks. Editor servicing also holds
each plugin's own mutex (`src/app/plugin_gui.rs:228-251`).

**Why it matters:** adding an insert, opening an editor, or syncing parameters can
stall the audio callback for the duration of a dynamic-library load or plugin
callback. This is audible priority inversion, not a rare race.

**Fix:** build a fully loaded slot before touching the chain, splice it in under a
short lock or an `ArcSwap`, publish immutable plugin views to the UI, and use
`try_lock` in the audio path with a bypass-on-contention policy. Never load,
destroy, or enumerate parameters while the audio callback can need the lock.

### M3. Bridge sessions have no ownership identity

**Evidence:** `AudioEngine::stop` (`src/audio/engine.rs:230-259`) sets the stop
flag, hands the join handle to a detached waiter, waits 900 ms, and then returns
regardless. The bridge closure (`src/audio/engine.rs:210-218`) later writes
`shared.running = false` and `set_status("Bridge stopped")` unconditionally.
The next `start()` calls `shared.reset()` and spawns a new thread on the same
`Arc<Shared>` (`src/audio/engine.rs:175-228`). Slow paths that ignore the stop
flag during teardown include `src/audio/win/asio/output.rs:131` (6 s open
timeout) and `src/audio/win/wasapi.rs:117-133` (4 s open timeout).

**Why it matters:** after a hung driver, an old bridge can push into a freshly
reset ring, clear the new bridge's `running` flag, write "Bridge stopped" over the
new status, or set an error/device-lost that the UI attributes to the new
session. Two output streams can drain the same ring. `stop()` also blocks the UI
thread for up to 900 ms, contradicting its comment.

**Fix:** give every start a generation/session identity (or a fresh `Shared`).
Ignore all `running` / `status` / `error` / `device_lost` writes from stale
generations. Make stopping an asynchronous state (`Stopping`) that the UI polls;
do not reset state or declare devices released until the old generation has
confirmed teardown.

### M4. `Starting` and `Running` can stick forever

**Evidence:** `AudioEngine::poll` (`src/audio/engine.rs:88-104`) only promotes
`Starting -> Running` when `shared.running` is true, and sets `Error` when an
explicit error exists. It never notices `shared.running == false`, a stopped
frame counter, a panic, or a clean thread exit. The bridge closure handles a
clean `Ok(())` by clearing `running` and setting "Bridge stopped"
(`src/audio/engine.rs:213-218`), but the engine state is untouched. Several ASIO
paths return `Ok(())` on a disconnected callback
(`src/audio/win/asio/input.rs:195,366`,
`src/audio/win/asio/output.rs:310`,
`src/audio/win/asio/duplex.rs:208`). `Starting` has no timeout at all.

**Why it matters:** a hung driver open leaves the header on "Stop bridge" /
"Linking" indefinitely, and a bridge that ends cleanly can show "Bridge live"
with no audio. The `JoinHandle` result is discarded.

**Fix:** publish a terminal result (`Ok` / `Err` / `Cancelled`) from the bridge
thread; check `JoinHandle::is_finished` and the join result; add a start timeout
and a frames heartbeat; transition to `Error` or `Idle` when the heartbeat
stops; guard the bridge thread with `catch_unwind`.

### M5. Windows ASIO buffers are accessed outside `bufferSwitch`

**Evidence:** the ASIO callbacks only lock a global mutex and
`try_send` a buffer index on a `sync_channel(2)` (`src/audio/win/asio/mod.rs:41-55`,
`110-119`). All actual reads/writes happen later on the bridge/worker thread
(`src/audio/win/asio/input.rs:101-115,144-180`,
`src/audio/win/asio/output.rs:231-295`,
`src/audio/win/asio/duplex.rs:102-192`). The channel has capacity 2 and
`try_send` results are ignored, so a late worker silently loses buffer switches.
In duplex mode the driver is started before the `OutputMixer` is created
(`src/audio/win/asio/duplex.rs:125` vs `:139`); the output worker has the same
ordering (`src/audio/win/asio/output.rs:254` vs `:278`).

**Why it matters:** ASIO guarantees a buffer index only until the next
`bufferSwitch`. A late worker can read an input buffer while the driver is
overwriting it, or write an output buffer the driver has already started
playing. This is the primary Windows use case (guitar in, interface out), so it
is a material risk even though it could not be reproduced without hardware.

**Fix:** do the sample copy and DSP inside `bufferSwitch` /
`bufferSwitchTimeInfo`, or copy into a preallocated lock-free block queue there.
Call `outputReady` only after the real output buffer is filled. Remove the
global mutex from the callback, prefill silence before `driver.start()`, and put
the worker in MMCSS "Pro Audio" if any work remains asynchronous.

### M6. ASIO input gain is sampled once

**Evidence:** `src/audio/win/asio/input.rs:141` reads
`let gain = shared.input_gain();` before the `while !stop` loop at `:144`.
The loop uses that value for every block (`:156,164`). The other ASIO paths read
the gain inside the loop (`src/audio/win/asio/input.rs:319`,
`src/audio/win/asio/duplex.rs:147`), and the cpal input callback reads it per
callback (`src/audio/input.rs:93`).

**Why it matters:** on the ASIO-input -> system-output path, moving the input
gain slider has no effect until the bridge is restarted.

**Fix:** read `shared.input_gain()` inside the block loop, or apply gain in the
mixer with a per-block ramp (preferred, because it also avoids zipper noise).

---

## High-severity findings after the blockers

### H1. The resampler aliases when downsampling

`src/audio/resampler.rs:37-63` is a four-point Catmull-Rom interpolator with no
anti-alias prefilter. At exact 2:1 (96 k -> 48 k) the phase is always zero, so
it is effectively sample dropping. Everything between the output Nyquist and the
source Nyquist folds into the audible band. Use a polyphase/windowed-sinc
resampler with a low-pass at `min(source, output) / 2`; keep the cubic only for
the 1:1 bypass. Add a test that a 30 kHz tone at 96 k -> 48 k is attenuated
rather than folded to 18 kHz.

### H2. Drift correction is a one-frame splice per block

`src/audio/mixer.rs:169-181` drops or duplicates one whole stereo frame per
output callback, with a deadband of `target / 2`. Every correction is a hard
time-domain discontinuity, and latency sawtooths inside the deadband before a
burst of corrections. For an app whose core feature is two independent clocks,
steer the resampler ratio continuously from a slow PI/PLL error on ring fill
(a few ppm per second), or at minimum crossfade each correction and shrink the
deadband.

### H3. Gain, mute, channel enable, and routing step per block

`src/audio/mixer.rs:121-132,204-259` read controls once per block and apply them
instantly. Dragging a level, toggling mute, or disabling a channel produces
zipper noise/clicks. Compute start/end gain per block and ramp across it;
crossfade mute/monitor and route changes over a few milliseconds.

### H4. Latency reporting uses requested values

`src/app/bridge.rs:203-220` uses `self.buffer_size` instead of
`snapshot.input_block_frames` while running, and reuses the last
`snapshot.target_frames` after stop. A device that delivers 480-frame blocks
while 128 was requested will show several milliseconds too little latency. The
"ROUND TRIP" label also excludes AD/DA, driver, and plugin latency. Use the
actual input block and label the value as a bridge-buffer estimate unless real
device/plugin latency is included.

### H5. CLAP module lifecycle is per instance and per scan

`ClapPlugin::load` calls `entry.init` for every instance
(`src/audio/clap.rs:261-284`); `Drop` calls `entry.deinit`
(`src/audio/clap.rs:878-900`); `scan_bundle` independently calls
`init`/`deinit` (`src/audio/clap.rs:1320-1377`). A rescan while a plugin is
loaded, or two instances of the same plugin, can deinit module state under a
live instance or call `init`/`deinit` concurrently with processing. Add a
process-wide module registry keyed by canonical path with refcounted `Arc`s and
serialised `init`/`deinit`; scan through that registry or a child process.

### H6. CLAP load can leak a created plugin

After `create_plugin` + plugin `init` succeed, `audio_channel_counts(plugin, ...)?`
at `src/audio/clap.rs:330` can return early for a >2-channel or no-main-port
plugin. The raw plugin is never `destroy`ed, `deinit` is not called, the host
`Box`/`HostState` is dropped, and the library is unloaded, potentially leaving
the leaked plugin with a dangling `clap_host*` and running threads. Use an RAII
guard that destroys the plugin on every early return and keeps the host/library
alive until destruction.

### H7. CLAP parameters and plugin state are not restored

`src/audio/plugins.rs:297-315` loads CLAP plugins without applying
`PluginPref.params`; VST3 does apply them in `load_vst3` (`:340-344`). Every
restart and every sample-rate-triggered reload resets CLAP controls. More
broadly, `PluginPref` stores only parameter values (`:61-90`); no opaque plugin
state is saved, and the CLAP state extension / VST3 `save_state`/`load_state` are
unused. Add a versioned state blob and apply parameters before audio starts.

### H8. Plugin lifecycle runs on bridge threads

`prepare_plugins` is called on the bridge thread at `src/audio/bridge.rs:71`,
`src/audio/win/asio/input.rs:67`, `output.rs:41`, and `duplex.rs:74`. When the
actual device rate differs from the planned rate, `PluginChain::refresh_rate`
destroys and recreates every plugin there (`src/audio/plugins.rs:393-414`).
`HostState.main_thread` records whichever thread loaded the plugin
(`src/audio/clap.rs:98`), while later `on_main_thread`/GUI servicing runs on the
eframe UI thread. Load, activate, deactivate, and destroy plugins only on one
dedicated main thread; pass the actual output rate back and reload there.

### H9. CLAP multi-port effects are malformed

`src/audio/clap.rs:664-690` always passes `audio_inputs_count: 1` and
`audio_outputs_count: 1`, while `audio_channel_counts` only inspects the main
port (`:1176-1217`). A sidechain compressor or multi-output effect sees a port
count that contradicts its descriptor; a plugin indexing `audio_inputs[1]` can
read out of bounds. Enumerate all audio ports, allocate one `clap_audio_buffer`
per port, feed the main port plus silent auxiliary ports, and set the real
counts, or reject non-single-port effects explicitly.

### H10. Hidden-window servicing is missing

`src/app.rs:163-173` implements only `eframe::App::ui`. eframe 0.37 calls
`logic` instead of `ui` when the window is hidden, so minimising RigWerk stops
`service_plugins` (CLAP `on_main_thread`/timers/flush), `poll_plugin_gui`
(VST3/CLAP editor events), `engine.poll` (device-loss detection),
`watch_devices`, and driver probing while audio continues. Move all non-painting
work into `fn logic`; keep `ui` for drawing; optionally request
paint-while-hidden when a plugin editor needs frames.

### H11. Linux performs a device configuration query on the UI thread every frame

`src/app/bridge.rs:127-128` calls `refresh_quantum_actual` every frame. For every
non-PipeWire output, `self.quantum` stays `None`, so
`src/app/driver_check.rs:120-127` calls `refresh_quantum_check(false)`, which
calls `planned_output_period` (`src/audio/linux/alsa.rs:49-72`): parse the device
id, `host.device_by_id`, `default_output_config` - roughly 60 times per second
while running. On direct ALSA this can contend with the PCM the bridge already
holds. Gate the calculation on `is_pipewire_device`, cache the planned period,
and refresh only on state/device/settings transitions. The start path also calls
`refresh_quantum_check(true)` immediately after spawning the bridge
(`src/app/bridge.rs:85`), opening the same output while the bridge opens it.

### H12. Hot-plug scanning can false-unplug and stop the bridge

`src/app/devices.rs:186-190` scans every 2.5 s. `scan_devices` only adds a device
if `default_output_config()`/`supported_output_configs()` succeeds
(`src/audio/devices.rs:121-175`); errors are silently skipped. `apply_inventory`
treats a single missing scan as an unplug and calls `engine.stop()`
(`src/app/devices.rs:259-263,302-329`). On an exclusive ASIO or direct ALSA
`hw:` device, that config query can fail while the device is actually in use.
Separate presence from capability, treat an open failure as "unknown/busy",
debounce two consecutive misses, and pause probing while Starting/Running.

### H13. Device identity is display-name based

Preferences save only names (`src/app.rs:293-299`), startup matches by name
(`:349-375`), and presets store only names
(`src/app/presets.rs:15-18,87-111`). Outputs are deduped by display name
(`src/audio/devices.rs:216`); inputs by name+backend (`:228`). The same speaker
via WASAPI and ASIO, two identical interfaces, or `hw:`/`plughw:`/`front`
aliases can be confused or one can be hidden. Persist the stable `DeviceId` /
CLSID plus backend, match on id first, and use name only as a logged fallback.

### H14. Presets are partial, lossy, and unvalidated

`Preset` saves devices, rates, buffers, and EQs, but not gains, channel enables,
routes, mono/mute/monitor/drift, cushion, low-latency mode, or the plugin chain
(`src/app/presets.rs:10-25`, `src/app/settings.rs:151-171`). `apply_preset` logs
"Loaded patch" even when neither device was found (`:104-112`). Rates/buffers
are assigned raw with no check against `RATES`/`BUFFERS`;
`RigWerkApp::new` accepts raw `input_gain`/`output_gain` (`src/app.rs:404-424`),
so a corrupt NaN/inf value poisons `10^(dB/20)` and the mixer. Add one
validation boundary: finite gains clamped to slider ranges, known rates/buffers,
`output_buffer >= 16`, and explicit missing-device errors.

### H15. Saving after one launch without the saved device overwrites the selection

If the saved ASIO/output is absent, `RigWerkApp::new` silently selects the
default or demo placeholder (`src/app.rs:349-375`), and `save()` writes those
names back (`:293-299`). One launch without the interface permanently replaces
the saved pair. Track whether restore fell back and do not overwrite the saved
identity in that case.

### H16. `Preset` deserialisation is fragile

Only `output_rate` and `output_buffer` have `#[serde(default)]`
(`src/app/settings.rs:159-170`); a single missing/renamed `name`, `note`,
`input`, `output`, `rate`, or `buffer` makes the whole `Preferences`
deserialise fail. `eframe::get_value` then returns `None`, the app starts from
defaults and later saves the empty session list over the existing file
(`src/app.rs:329-337`). Add per-field defaults, skip bad presets while keeping
good ones, add a schema version, and show a load-failure notice.

### H17. PipeWire quantum force can leak

`src/audio/linux/pipewire.rs:140-146` writes `clock.force-quantum`, then
requires a successful read-back; if the write succeeds but the read fails,
`force_low_quantum` returns `Err`, and `src/audio/engine.rs:184-197` sets
`quantum_forced = false`. Stop then never restores the previous value, leaving
the whole desktop pinned at 64 frames. Mark the force as owned immediately
after a successful write, restore on every failure path, distinguish "unknown
previous" from `0`, and consider startup recovery after a crash.

### H18. Linux backend classification overclaims

- `is_pipewire_endpoint` treats any existing `pipewire-0` socket plus the
  `default` PCM as PipeWire (`src/audio/linux/pipewire.rs:43-63`); a
  PulseAudio-backed `default` can be forced to a 64-frame quantum for no reason.
- With no PipeWire/JACK, `low_latency_label = "ALSA"`; the final backend check
  in `is_low_latency_device` (`src/audio/devices.rs:389-403`) then marks every
  ALSA PCM low-latency, including `pulse`/`default`/`sysdefault`, contradicting
  the UI text.
- JACK is advertised in the guide and has code branches, but `cpal = "0.18"` is
  built without its `jack` feature and `Cargo.lock` contains no `jack` crate.
  Those branches are unreachable. Enable the feature or remove the claims.

---

## UI/UX findings

### U1. Synthetic data is presented as measurement

`src/ui/scope.rs:50-85` draws a time-driven sine mixture and ignores
`_in_r`/`_out_r`; the mini scope does the same (`:104-140`). The Routing page's
"OUTPUT TAPS" bars are computed from `meter_out_l` with arbitrary per-tap
offsets (`src/ui/pages/routing.rs:198-215`). The UI labels them "LIVE SIGNAL",
"PEAK", and "ROUND TRIP". Either feed real samples/per-tap levels, or rename
them "level preview" / "estimated latency" and use `peak_in_l`/`peak_out_l` for
PEAK.

### U2. Routing is hard-coded to an ASIO 2-in/4-out interface

`src/ui/pages/routing.rs:114` says "2 IN -> 4 OUT"; `:146-150` hard-codes
"ASIO 1 - guitar" and "ASIO 2 - spare" regardless of OS or selected input. On
Linux/PipeWire or with a system microphone, the screen names the wrong
protocol. Derive labels and enabled cells from the active device
name/backend/channel count.

### U3. Routing page overflows at the minimum window size

At 1040 px, central content is `1040 - 224 sidebar - 52 margins = 764 px`.
`src/ui/pages/routing.rs:99-102` clamps the left column to 520 and the right to
280, plus an 8 px gap = 808 px, so the right card is clipped by about 44 px.
Lower the minimums or stack the columns below about 1100 px.

### U4. Cushion controls are clipped at the minimum width

At 1040 px, the bridge card is about 180 px outer / 150 px inner
(`src/ui/pages/live.rs:21-32`). `src/ui/pages/live/bridge_card.rs:117-138` puts
"Safe", "Low latency", and "Manual" in one non-wrapping row needing about
190 px at `FONT_SCALE` 1.2. `card_fixed` clips silently
(`src/ui/widgets.rs:51-58`). Use a combo/vertical group or stack the Live page
below a breakpoint.

### U5. Accessibility is weak

Custom painted nav/device/preset/route/plugin rows use
`allocate_exact_size(... Sense::click())` without `widget_info` or keyboard
activation (`src/ui/widgets.rs:195-243`, `src/ui/pages/devices.rs:19-86`,
`src/ui/pages/sessions.rs:18-134`, `src/ui/pages/routing.rs:18-60`). The
gain/EQ/plugin-mix slider is `Sense::click_and_drag()` only
(`src/ui/widgets.rs:347-390`): no focus, arrows, numeric entry, or accesskit
value. `DIM = #59667A` (`src/ui/theme.rs:24`) is only 3.14:1 against
`CARD #10151F`, below WCAG AA 4.5:1, and is used at 8-10 px. Use standard
widgets or add `Response::widget_info`, visible focus, keyboard handling, and a
lighter DIM.

### U6. Diagnostics disappear

The log is capped at six entries (`src/app.rs:535-545`) and only the last one is
shown (`src/ui/pages/live.rs:109-148`); `Inventory.notes` is collected for ASIO
scan failures but never displayed (`src/audio/types.rs:174-176`,
`src/audio/devices.rs:59`); `Shared.status` is written but never reaches the UI
(`src/app/bridge.rs:254-262`). A failed ASIO scan or quantum restore disappears
before the user can see it. Keep a bounded 200-500-entry log with a Diagnostics
page and copy/export, and surface inventory/status notes.

### U7. The app repaints at 60 Hz even when idle

`src/app.rs:173` unconditionally calls `request_repaint_after(16 ms)`. With the
bridge stopped and no Preview, nothing changes except pointer hover. Use 16 ms
only while active/Preview/animation; otherwise repaint on interaction.

---

## Build and repository hygiene

- **No LICENSE, README, or CI.** There are 59 tracked files, only Cargo files
  and `src/**`. Add a license, `license`/`repository`/`readme` package metadata,
  and a Windows + Linux CI matrix (`fmt`, `clippy`, `test`,
  `build --release`; compile the ignored hardware tests with
  `cargo test --no-run`).
- **No `rust-version` or toolchain pin.** Add one consistent with eframe/cpal.
- **Windows release opens a console window.** `src/main.rs` has no
  `windows_subsystem = "windows"` attribute. Add it for release builds and keep
  a console/log path for `--screenshot`.
- **`serde_json` is unused** in `Cargo.toml:11` (it is only a transitive
  dependency of `vst3-host`). Remove the direct entry.
- **Debug builds run DSP at opt-level 0.** Add `[profile.dev] opt-level = 1`
  and `[profile.dev.package."*"] opt-level = 2`; otherwise `cargo run` will
  underrun at small buffers.
- **No release assets.** No icon, `.desktop`, Windows resource/version metadata,
  or packaging recipe. Consider `with_icon`/`with_app_id`, a `build.rs` for
  Windows resources, and AppImage/MSI/portable-zip output.
- **`--screenshot` is not actually headless.** It still calls
  `eframe::run_native` and needs a display (`src/app/screenshot.rs:1`,
  `src/main.rs:27-39`). Document `xvfb-run` or add a real offscreen renderer;
  reject `--screenshot=` and use `args_os()`.

---

## Strengths

- Clean separation between backend-neutral audio code (`shared`, `mixer`,
  `resampler`, `eq`) and platform backends; the module docs are genuinely
  useful.
- Live controls use atomics, and the EQ audio path uses `try_lock` rather than
  blocking the callback.
- The cushion state machine, EQ, resampler speed, and quantum/device
  classification have meaningful unit tests (60 `#[test]` functions; hardware
  tests are correctly `#[ignore]`d).
- ASIO sample-format conversion covers every PCM/float type with correct
  endianness and bit alignment; WASAPI format/event/COM lifetimes are handled
  with RAII.
- Driver mismatch reporting is unusually good: it reports the real rate/step,
  explains what to change, and falls back instead of failing silently.
- The recent 512-frame plugin chunking fix is sound: resizing the VST3 buffers
  to the chunk length prevents plugins from advancing by a full 512 frames on
  short callbacks.

---

## Suggested order of work

1. Real-time safety: lock-free SPSC ring, off-thread plugin-chain publication,
   `try_lock`/bypass in the audio callback, process ASIO buffers in the
   callback.
2. Session lifecycle: generation tokens, asynchronous stop, terminal bridge
   result, start timeout and frame watchdog.
3. Signal path: band-limited resampler, continuous drift steering,
   gain/mute/route ramps, honest latency reporting.
4. Plugins: CLAP module registry/refcount, single-main-thread lifecycle,
   parameter/state restore, multi-port handling, hidden-window servicing via
   `logic`.
5. Devices/persistence: non-invasive/debounced scans, stable device ids,
   validated preferences/presets, complete session schema.
6. UI/repo: real or honestly labelled meters, min-width layout fixes,
   accessibility, diagnostics log, license/README/CI.
