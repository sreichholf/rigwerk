# Screenshots

The images the [README](../../README.md) shows, and how to take them again.
Run the commands from the repository root.

`--screenshot <file>` renders a fixed 1440×1080 window and saves it. It
always uses X11/XWayland, because Wayland ignores the requested size. The
screenshots here all use the same `showcase` setup: two paths (Clean and
Room) with real plugins, two extra outputs, and a running bridge.

```bash
export RIGWERK_ROUTING_SETUP=showcase RIGWERK_SCREENSHOT_START=1
export RIGWERK_SCREENSHOT_PLUGINS="1=/usr/lib/clap/lsp-plugins-clap.clap#Compressor Stereo;\
2=/usr/lib/vst3/DragonflyRoomReverb.vst3;\
2=/usr/lib/clap/lsp-plugins-clap.clap#Limiter Stereo"
for page in live mixer routing plugins devices; do
  RIGWERK_PAGE=$page cargo run --release -- --screenshot docs/screenshots/$page.png
done
```

`RIGWERK_SCREENSHOT_START=1` opens the selected devices, so use a virtual
PipeWire graph, not real hardware. This set was taken under
`xvfb-run -a` with the null-sink graph from [`AGENTS.md`](../../AGENTS.md): sinks "Studio
Monitors" and "USB Headphones", and a virtual source "Guitar Interface" fed
a plucked-string loop through `pw-cat` and `pw-link`, with
`pw-metadata -n settings 0 clock.force-quantum 64`. The plugins come from
the Debian/Ubuntu `lsp-plugins-clap` and `dragonfly-reverb-vst3` packages.

Environment variables for screenshot runs:

| Variable | Values |
| --- | --- |
| `RIGWERK_PAGE` | `live`, `mixer`, `routing`, `devices`, `sessions`, `plugins`, `tuner`, `guide`, `diagnostics` |
| `RIGWERK_MIXER_STAGE` | `input`, `pre_eq`, `plugins`, `gate`, `post_eq`, `output`, `device` (the clock device) |
| `RIGWERK_ROUTING_SETUP` | `showcase`; `drywet`, `sum3`, `blend`, `dualamp-off` (alignment off, the red warning), `live3` (Blend plus an empty third path) |
| `RIGWERK_SCREENSHOT_PLUGINS` | `;` list of `<path number>=<bundle>[#<effect name>]`, used by `showcase` |
| `RIGWERK_SCREENSHOT_START` | `1` starts the bridge before the capture |
| `RIGWERK_SCROLL` | pixels to scroll the page down |

`blend`, `dualamp-off` and `live3` fake one path's plugin delay (no plugin
is loaded), so the alignment has something to show.
