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
- **Honest stereo routing.** Left/Right into Out 1/2 — the pair the engine
  actually plays.
- **Sessions.** Save the whole patch (devices, gains, routes, EQ, gate,
  plugins) and load it again.

![Mixer](docs/screenshots/mixer.png)

![Signal Routing](docs/screenshots/routing.png)

![Devices](docs/screenshots/devices.png)

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
