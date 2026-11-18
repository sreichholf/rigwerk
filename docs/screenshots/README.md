# Screenshots

The images the [README](../../README.md) shows, and how to take them again.
Run the commands from the repository root.

`--screenshot <file>` renders a 1440×1080 window (or the size in
`RIGWERK_SCREENSHOT_SIZE`) and saves it. It always uses X11/XWayland,
because Wayland ignores the requested size. The screenshots here all use
the same `showcase` setup: two paths (Clean and Room) with real plugins,
two extra outputs, a running bridge, and the metronome at 96 BPM in 4/4,
clicking on the extra headphones only.

```bash
export RIGWERK_ROUTING_SETUP=showcase RIGWERK_SCREENSHOT_START=1
export RIGWERK_SCREENSHOT_PLUGINS="1=/usr/lib/clap/lsp-plugins-clap.clap#Compressor Stereo;\
2=/usr/lib/vst3/DragonflyRoomReverb.vst3;\
2=/usr/lib/clap/lsp-plugins-clap.clap#Limiter Stereo"
for page in live mixer tuner routing plugins devices; do
  RIGWERK_PAGE=$page cargo run --release -- --screenshot docs/screenshots/$page.png
done
RIGWERK_PAGE=mixer RIGWERK_MIXER_STAGE=output \
  cargo run --release -- --screenshot docs/screenshots/metronome.png
```

`RIGWERK_SCREENSHOT_START=1` opens the selected devices, so use a virtual
PipeWire graph, not real hardware. This set was taken under
`xvfb-run -a` with the null-sink graph from [`AGENTS.md`](../../AGENTS.md)
and a fresh `HOME` per page (no saved preferences):

```bash
pw-cli create-node adapter '{ factory.name=support.null-audio-sink
  node.name=studio_monitors node.description="Studio Monitors"
  media.class=Audio/Sink audio.position=[FL FR] object.linger=true
  priority.session=2000 }'
pw-cli create-node adapter '{ factory.name=support.null-audio-sink
  node.name=usb_headphones node.description="USB Headphones"
  media.class=Audio/Sink audio.position=[FL FR] object.linger=true
  priority.session=1000 }'
pw-cli create-node adapter '{ factory.name=support.null-audio-sink
  node.name=guitar_interface node.description="Guitar Interface"
  media.class=Audio/Source/Virtual audio.position=[FL FR]
  object.linger=true priority.session=2000 }'
pw-metadata -n settings 0 clock.force-quantum 64
# A long plucked-string WAV, not linked by WirePlumber, then linked by hand:
pw-cat -p --target 0 --volume 0.35 -P '{ node.name=guitar_feed }' pluck.wav &
pw-link guitar_feed:output_FL guitar_interface:input_FL
pw-link guitar_feed:output_FR guitar_interface:input_FR
```

The runtime directory must be short (`mktemp -d /tmp/pwXXXX`): PipeWire's
socket path is limited to 108 bytes.  The app needs `libxkbcommon-x11-0`
under Xvfb.  The plugins come from the Debian/Ubuntu `lsp-plugins-clap`
and `dragonfly-reverb-vst3` packages.  In a screenshot the Tuner page draws
a fixed demo frame (Drop C), not the live input.

Environment variables for screenshot runs:

| Variable | Values |
| --- | --- |
| `RIGWERK_PAGE` | `live`, `mixer`, `routing`, `devices`, `sessions`, `plugins`, `tuner`, `guide`, `diagnostics` |
| `RIGWERK_MIXER_STAGE` | `input`, `pre_eq`, `plugins`, `gate`, `post_eq`, `path` (the selected path's level, with 2+ paths), `output` (also the metronome), `device` (the clock output); applied after the setup, so it wins over the effect `showcase` selects |
| `RIGWERK_ROUTING_SETUP` | `showcase`; `drywet`, `sum3`, `blend`, `dualamp-off` (alignment off, the red warning), `live3` (Blend plus an empty third path) |
| `RIGWERK_SCREENSHOT_PLUGINS` | `;` list of `<path number>=<bundle>[#<effect name>]`, used by `showcase` |
| `RIGWERK_SCREENSHOT_SIZE` | `<w>x<h>` window size in points, from `640x480` to `7680x4320` (default `1440x1080`); a blank, unparsable or out-of-range value is logged and the default used |
| `RIGWERK_SCREENSHOT_START` | `1` starts the bridge before the capture |
| `RIGWERK_SCROLL` | pixels to scroll the page down |

`blend`, `dualamp-off` and `live3` fake one path's plugin delay (no plugin
is loaded), so the alignment has something to show.
