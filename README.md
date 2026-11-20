# RigWerk

Run an audio interface through effects and out to any other device, even
when the two don't share a clock. Guitar into one interface, EQ, plugins
and a gate, then out to monitors, headphones and a stream at once.

Linux (PipeWire, ALSA) and Windows (ASIO, WASAPI). Alpha: expect rough
edges.

![The console](docs/screenshots/console.png)

## Features

- **Any input to any output**, across clocks and sample rates, with drift
  correction.
- **Up to four paths**, each with EQ, VST3/CLAP effects and a noise gate.
  Paths stay time-aligned on every output.  An effects return runs one
  reverb or delay on every path's sends.
- **NAM blocks.** Load Neural Amp Modeler `.nam` captures straight into a
  path and stack them, e.g. a drive pedal into an amp, with automatic level
  matching. No plugin needed.
- **Cab IRs.** Load a `.wav` impulse response per path, with level, low
  and high cut and polarity. No added latency.
- **Several outputs at once**, plus a virtual output other apps can record.
- **Low latency**, showing the rates and buffers the drivers really opened.
- **Safety limiter** on every output (−1 dBFS).
- **Tuner, metronome and recorder** built in.
- **Sessions** save and restore the whole setup.

The full tour of every screen is in the [manual](docs/manual.md).

## Screenshots

### EQ

![The EQ sheet](docs/screenshots/eq-sheet.png)

### Output settings

Channel map, per-path level, polarity and timing on one output.

![The output sheet](docs/screenshots/output-sheet.png)

### Tuner

![The tuner](docs/screenshots/tuner.png)

## Build and run

No release builds yet. With Rust 1.95 or newer:

```bash
cargo run --release
```

- **Linux:** X11 or Wayland, plus `libpipewire-0.3-dev` and
  `libasound2-dev` (Debian/Ubuntu). For the lowest latency with ALSA
  devices, join the `audio` group for real-time threads.
- **Windows:** MSVC toolchain. ASIO drivers are optional.

## Keys

| Key | Does |
| --- | --- |
| Ctrl+Shift+M | Mute the master |
| Ctrl+T | Tuner |
| Ctrl+, | Setup |
| Ctrl+S / Ctrl+Shift+S | Save session / save as new |
| Ctrl+F | Effect browser |
| Ctrl++ / Ctrl+− / Ctrl+0 | UI size |
| Esc | Close the top popup, sheet or screen |

## Development

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
```

Guidelines: [`AGENTS.md`](AGENTS.md). Known gaps:
[`docs/open-items.md`](docs/open-items.md). Screenshots:
[`docs/screenshots`](docs/screenshots/README.md).

## License

[GPL-3.0-or-later](LICENSE). Bundled fonts: IBM Plex subsets (renamed
RigWerk Sans/Mono) under the [OFL 1.1](assets/fonts/OFL.txt), and egui's
fallback fonts (Ubuntu Font Licence, MIT). VST3 and ASIO are Steinberg
technology with their own terms.
