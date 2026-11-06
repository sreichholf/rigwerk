# Multiple outputs: implementation plan

Working plan for PR #7. Read this before picking up any step.
Background: `dual-out-concept.md` (why). UI: `output-devices.html` and
`signal-routing.html` (mockup E, open in a browser; the bar at the top
switches platform and scenario). `dual-out-endpoints.html` (A–D) is history.

## Decisions (agreed)

- **One mix, N output devices.** The chain (EQ, inserts, gate) runs once.
- **Clock device + followers**, not Listen / Send / Publish.
  - The clock device is today's output (`output_id`). The chain runs on its
    callback.
  - Followers play the finished mix on their own clock: own ring,
    resampler, drift loop, cushion, health.
  - RigWerk Out (virtual) is a follower with an endpoint instead of a device
    picker. It can never be the clock device.
- **Mixer**: the Output stage stays the master (level + mute). Output devices
  are its children, like inserts under Plugins. One inspector per kind:
  master, clock device, follower, virtual, add list.
- **Signal Routing** has two layers, as the engine does:
  1. Input → Mix: today's matrix, columns renamed MIX L / MIX R.
  2. Mix → each device's channels: a small matrix per device, with presets
     and an "unused channels" option (Silent / Mono copy).
- **Live vs. restart**: adding/removing a follower, level, mute, cushion,
  routing and publish are live. Changing the clock device, or its
  device/rate/buffer, restarts the bridge.
- **Missing follower**: the bridge runs without it; it stays in the session.
  Missing clock device: the bridge can't start (as today).
- **Platforms**
  - Linux/PipeWire first: one node, one port pair per device, links per
    sink; PipeWire keeps the devices in step. RigWerk Out = a node with a
    virtual source `media.class`.
  - Linux ALSA `hw:` clock device: needs generic followers (step 3).
  - Windows: WASAPI followers only, VB-Cable (or similar) as publish.
    No ASIO followers (one driver per process).

## Steps

### Step 1: platform-neutral groundwork (this PR)

With one output device, everything looks and behaves exactly like today.

- **1a. Session model.** No behaviour change.
  - Keep `output_id` / `output_backend` / `output_gain` (master) /
    `mute` (master) as they are: they describe the clock device and master.
  - Add a per-device block for the clock device: level (dB), mute, channel
    map, unused-channel mode.
  - Add `followers: Vec<…>` (id, backend, display name, the same per-device
    block, cushion). Stored and round-tripped only; nothing plays them yet.
  - Channel map: Mix L and Mix R each to a set of device channels, up to a
    fixed maximum (8). Default L→1, R→2, unused = Mono copy, which is
    exactly today's `write_frame` output.
  - `routes` (2×4) stays in the file for compatibility; only columns 0/1
    mean anything. Don't migrate the file format, just stop using 2/3.
  - Both `Preferences` and `Preset`. Old files load unchanged (missing
    fields default); tests for old → new and round trips.
- **1b. Engine: the clock device's channel map and level.** Real-time code.
  - Replace the "extra channels = L/R average" rule in every write path
    (cpal `write_frame`, WASAPI, ASIO callback, PipeWire native, PipeWire
    duplex) with one shared function that applies the clock device's map,
    level and mute after the master.
  - The map lives in `Shared` atomics, like `routes`. Changes crossfade
    like route changes. No allocation or locks in callbacks.
  - Per-channel output meters for the clock device (up to 8) in `Shared`.
  - Default map gives bit-identical output to today (test it).
  - The mapping rule is specified in the module docs of
    `src/audio/output_map.rs` (both sides on one channel = average; mono
    device = average; unused = MonoCopy or Silent).
- **1c. Mixer UI.**
  - `ChainSelection::OutputDevice(n)`. The Output stage keeps the master
    inspector (level, mute, device list). Children: one row per device,
    for now only the clock device (CLOCK tag, level, mute button).
  - Clock device inspector: device, out clock, level/mute.
  - "+ add output device" is visible but disabled, with a short reason
    ("followers come in a later version"). No add list yet.
- **1d. Signal Routing UI.**
  - Layer ① Input → Mix (2×2, renamed columns) + the existing Mix card.
  - Layer ② Mix → outputs: device list (only the clock device for now),
    its matrix with real channel count, presets (Stereo 1/2, 3/4,
    1/2 + 3/4, Mono to all, Swap, Clear), unused-channel option,
    per-channel meters, "nothing routed" warning.
  - Device / playing-channel count lives on the Mix → Outputs header
    (no separate Active Patch strip — that duplicated the device list).

### Step 2: Linux/PipeWire followers and RigWerk Out

- Duplex/native PipeWire node gets one port pair per output device, linked
  to each sink. Only when the clock device is a PipeWire node.
- RigWerk Out: a virtual source node.
- Enable the add flow in the Mixer for PipeWire devices. Health tiles say
  "handled by PipeWire".
- Live: "Also playing on" list and the publish banner.

### Step 3: generic followers

- Copy hook in `OutputMixer::render` after the master: push the finished
  stereo mix into one SPSC ring per follower (allocated outside the
  callback).
- Follower = today's ring → resampler → drift loop, without the chain, on
  its own cpal stream. Its own life cycle: unplug, error and reconnect never
  stop the bridge.
- Enable for Linux ALSA first (testable here).

### Step 4: Windows

- WASAPI shared-mode followers, VB-Cable publish (detected by name), extra
  ASIO output channels on the open driver (routing layer ②).
- Warn in the add list when a WASAPI endpoint belongs to the open ASIO
  interface (some drivers lock it).

## Rules for every step

- `AGENTS.md`: one commit per logical change, fixups folded in, terse
  messages. Before pushing: `cargo fmt --check`, `cargo clippy --all-targets`,
  `cargo test`, and `cargo check --target x86_64-pc-windows-gnu`.
- Audio callbacks: no allocation, no locks, no logging; keep
  `DenormalGuard`. Settings reach callbacks through `Shared` atomics.
- Old preferences and presets must keep loading.
- Each sub-step is implemented and then reviewed by a separate agent before
  the next one starts.

## Status

- [x] Mockups and plan (this file)
- [x] 1a Session model (types in `src/audio/output_map.rs`)
- [x] 1b Engine: channel map, device level, per-channel meters (ASIO still opens at most 4 outputs; PipeWire fixed stereo until step 2)
- [x] 1c Mixer UI (screenshot stage via `RIGWERK_MIXER_STAGE`)
- [x] 1d Signal Routing UI (Routing keeps its own device selection; "Open in Mixer" jumps)
- Open follow-ups from step 1:
  - Re-take `docs/screenshots/routing.png` and `mixer.png` on real hardware.
  - Live chain strip's OUTPUT chip shows only the master level.
  - A missing clock device has no own state in the Mixer row (it shows the bridge state).
  - ASIO opens at most 4 outputs (step 4).
- [x] Step 2 · Linux/PipeWire followers and RigWerk Out
  - Duplex filter: preallocated stereo port pairs + live links for PW sinks
  - RigWerk Out: `Audio/Source/Virtual` filter, publish live
  - Mixer add/remove gated on native PipeWire clock; ALSA hw uses step 3
  - Routing layer ② edits follower/virtual maps; Live has Also playing on + publish banner
  - Open: native two-stream (non-duplex) PW playback does not yet fan out followers; ASIO still ≤4 outs
- [x] Step 3 · generic followers
  - `OutputMixer::finish_block` pushes finished stereo into per-follower SPSC rings
  - `FollowerMixer` + cpal stream per slot; supervisor add/remove/reconnect live
  - Mixer add enabled on Linux ALSA/cpal clocks; follower cushion UI; RigWerk Out stays PW-only
  - Open: Windows WASAPI followers (step 4); ASIO still ≤4 outs
- Related (docs only, not dual-out steps): Live Issues strip — `live-issues-plan.md`,
  mockup `live-issues.html`. Shared Warnings/Errors on Live; remediation must
  name real controls (Low latency, buffer, cushion), including per-follower
  cushion once followers play.
- [x] Step 4 · Windows
  - WASAPI shared-mode followers via the generic path; supervisor on ASIO bridges too
  - VB-Cable publish (name detect → CABLE Input); ASIO open channels up to 8
  - Soft ASIO/WASAPI conflict hint in the add list
  - Enabled by default (hand-tested: REAL-style clock + shared-mode headset)
