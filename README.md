# RigWerk

Independent audio bridge: take any input clock, play on any output clock.

On Linux that is usually PipeWire (or a direct ALSA `hw:` device). On Windows
it is ASIO and/or WASAPI. Between them sits an async ring, a short mixer rack
(EQ, inserts, noise gate), and a stereo route matrix.

![Live Bridge](docs/screenshots/live.png)

## Features

- **Two clocks.** Input and output keep their own rates; RigWerk resamples and
  steers drift instead of forcing one device to follow the other.
- **Mixer rack.** Pre/post EQ, VST3 + CLAP inserts, and a causal noise gate on
  the master mix.
- **Two-layer routing.** Inputs onto Mix L/R, then the finished mix onto any
  device channels, with presets; unused channels stay silent or get a mono
  copy.
- **Multiple signal paths.** Up to four chains on the same input, summed per
  output device and lined up for plugin latency (see below).
- **Sessions.** Save the whole patch (devices, gains, routes, EQ, gate,
  plugins, paths) and load it again.

![Mixer](docs/screenshots/mixer.png)

![Signal Routing](docs/screenshots/routing.png)

![Devices](docs/screenshots/devices.png)

## Multiple signal paths

A path is a full Mixer chain (routing, pre EQ, inserts, gate, post EQ, path
level). Up to four run on the same input, split right after the clock side,
so they cost no extra ring or cushion. Signal Routing shows them in three
layers: input onto each path, the paths, and each path onto output device
channels. Cells that meet on one channel add up. The master level and mute
are shared by every path and applied to each path before alignment, so a
master change reaches a delayed path up to its alignment delay later. The
master meters and the scope show the paths' plain stereo sum (not aligned).

Cards set everything up in one click and never delete inserts:

- **Single path**: today's routing.
- **Dry L / Wet R**: one path on the left channels, another on the right.
- **Blend clean**: a copy of the first path at −6 dB under the driven one.
- **Dual-amp**: two copies summed, the second with its polarity flipped; put a
  different amp sim on each.
- **Per-device mixes**: one path on the clock device, another on every
  follower (e.g. the stream gets a limiter the monitors don't).

**Alignment.** Plugins (amp sims, lookahead limiters, linear-phase EQs) delay
their path, and two paths that add up a few samples apart comb filter. So on
each output device every path waits for the slowest path that plays there;
muted paths count too, so muting never moves the latency. Each device has an
align switch (on by default); each path on each device has a level, a
polarity flip and a trim (±5 ms) on top. The cost is the slowest path's
latency on that device: Routing says what a device waits for ("Waits for
Drive, +2.7 ms") and Live shows the added time on the Output chip. Turn it
off on a device where nothing adds up and you want each path as fast as it
gets; Routing warns red when unaligned paths then add up on a channel.

Limits: alignment lines up the paths on one device, not devices with each
other (each device still has its own buffer, a follower its cushion). It
covers chain latency up to `MAX_ALIGN` (2¹⁵ = 32768 frames, about 0.7 s at
48 kHz); a longer chain plays late and Live Issues says so. A path cannot
take its input from another path; duplicate it instead. Diagnostics shows
each path's DSP time per block (average and peak), and Live Issues names an
unmuted path routed nowhere audible, an unaligned sum, or paths that
overload the block.

## Build and run

Rust **1.95** or newer (`rust-toolchain.toml` pins 1.95.0 — the lowest version
that builds egui/eframe 0.37).

```bash
cargo run --release
```

Linux needs PipeWire or ALSA development headers (`libpipewire-0.3-dev`,
`libasound2-dev`) and a display (X11 or Wayland). Windows needs the MSVC
toolchain; ASIO drivers are optional.

On Linux, direct ALSA streams run their audio threads at real-time priority
when your user may use it (an `RLIMIT_RTPRIO`, usually via the `audio` group
in `/etc/security/limits.d`). Without it they run at normal priority, as
before. PipeWire's own threads are real time either way. At real-time
priority, a plugin chain that overloads the CPU can starve the desktop.

```bash
cargo test
```

Screenshots for docs (fixed 1440×1080, no scrollbar):

```bash
# Wayland cannot honor client resize; --screenshot drops to X11/XWayland.
RIGWERK_PAGE=live cargo run --release -- --screenshot docs/screenshots/live.png
```

`RIGWERK_PAGE` may be `live`, `mixer`, `routing`, `devices`, `sessions`,
`plugins`, `guide`, or `diagnostics`.
`RIGWERK_MIXER_STAGE` selects a Mixer stage: `input`, `pre_eq`, `plugins`,
`gate`, `post_eq`, `output`, or `device` (the clock device).
`RIGWERK_ROUTING_SETUP` sets up paths: `drywet`, `sum3`, `blend`,
`dualamp-off` (alignment off, the red warning) or `live3` (Blend plus an
empty, unmuted third path). `blend`, `dualamp-off` and `live3` fake the plugin
latency of one path (no plugin is loaded), so the alignment has something to
show. `RIGWERK_SCROLL=<px>` scrolls the page down.

## License

RigWerk is free software under the [GNU General Public License v3](LICENSE)
(or, at your option, any later version).

Third-party crates keep their own licenses (mostly MIT / Apache-2.0). System
libraries such as PipeWire and ALSA are typically LGPL and are linked
dynamically.

### Bundled fonts (egui)

The UI embeds egui’s default fonts from `epaint_default_fonts`:

| Font | License |
| --- | --- |
| **Ubuntu Light** | [Ubuntu Font Licence 1.0](https://ubuntu.com/legal/font-licence) |
| **Hack Regular** | MIT (Source Foundry), with Bitstream Vera / DejaVu lineage |
| **egui icons** | subset of emoji-icon-font (MIT) |

Ubuntu Light remains under the Ubuntu Font Licence; Hack under MIT / Bitstream
Vera. The font files are not dual-licensed as GPLv3 — only the RigWerk source
around them is. Full texts ship with the `epaint_default_fonts` crate
(`OFL.txt`, `UFL.txt`, `Hack-Regular.txt`).

### Plugin formats

VST3 and ASIO involve Steinberg technology beyond the Rust crate SPDX lines.
Hosting a VST3 or talking to an ASIO driver may require agreeing to Steinberg’s
own terms; that is separate from this GPLv3 grant.
