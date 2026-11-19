# Screenshots

The images for the [README](../../README.md), and how to take them again.
Run the commands from the repository root.

## Needs retaking

Every image in this folder was taken on the previous sidebar UI. None shows
the console, the sheets, the browser or the new Setup screen yet. The set
below is what this UI needs. `tuner.png` exists, but it is the previous
tuner page and is overwritten by the `RIGWERK_PAGE=tuner` command below.

`live.png`, `mixer.png`, `metronome.png`, `routing.png`, `plugins.png` and
`devices.png` show pages that no longer exist. They stay in the folder until
the new set replaces them and are no longer linked from the README.

## Taking the set

The set is taken under `xvfb-run -a` with the null-sink graph below and
`RIGWERK_SCREENSHOT_START=1` so the meters, the scope and the extra outputs
show real signal. Take each page without saved preferences (a fresh `HOME`
per page, as before), so no saved preference picks a page, a sheet or a
theme. Use a virtual graph, not real hardware.

```bash
export RIGWERK_ROUTING_SETUP=showcase RIGWERK_SCREENSHOT_START=1
export RIGWERK_SCREENSHOT_SIZE=1440x900
export RIGWERK_SCREENSHOT_PLUGINS="1=/usr/lib/clap/lsp-plugins-clap.clap#Compressor Stereo;\
2=/usr/lib/vst3/DragonflyRoomReverb.vst3;\
2=/usr/lib/clap/lsp-plugins-clap.clap#Limiter Stereo"

shot() {
  xvfb-run -a cargo run --release -- --screenshot "docs/screenshots/$1"
}
```

| File | Command | Shows |
| --- | --- | --- |
| `console.png` | `RIGWERK_PAGE=console shot console.png` | the console with two paths (Clean and Room) |
| `console-4-paths.png` | `RIGWERK_ROUTING_SETUP=showcase4 shot console-4-paths.png` | the console with four paths and a long chain |
| `console-light.png` | `RIGWERK_THEME=light shot console-light.png` | the same console in the light theme |
| `eq-sheet.png` | `RIGWERK_SHEET=eq shot eq-sheet.png` | the EQ sheet |
| `output-sheet.png` | `RIGWERK_SHEET=output shot output-sheet.png` | the output sheet, on the clock output |
| `browser.png` | `RIGWERK_DRAWER=browser shot browser.png` | the effect browser |
| `tuner.png` | `RIGWERK_PAGE=tuner shot tuner.png` | the tuner screen |
| `setup.png` | `RIGWERK_PAGE=setup shot setup.png` | Setup |
| `console-1040x700.png` | `RIGWERK_SCREENSHOT_SIZE=1040x700 RIGWERK_PAGE=console shot console-1040x700.png` | the console at the minimum window size |

The default size is 1440 × 1080; the set above asks for 1440 × 900 because
that is the size the console is designed at. `--screenshot <file>` renders
the window at that size and saves it, always through X11/XWayland, because
Wayland ignores the requested size. The plugin list matters for the console
and the EQ sheet: `showcase` selects Room, whose chain holds the reverb and
limiter.

## A PipeWire graph without hardware

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
under Xvfb.  The plugins come from the Debian/Ubuntu `lsp-plugins-clap` and
`dragonfly-reverb-vst3` packages.  In a screenshot the tuner draws a fixed
demo frame (Drop C, strobe, all strings, auto-mute on), not the live input.

Environment variables for screenshot runs:

| Variable | Values |
| --- | --- |
| `RIGWERK_PAGE` | `console`, `tuner`, `setup` |
| `RIGWERK_SHEET` | `eq`, `gate`, `effect` (a stand-in effect when the chain is empty), `path`, `output` (the clock output), `scope`, `diagnostics`; on the selected path, applied after the setup |
| `RIGWERK_DRAWER` | `browser`, `issues` |
| `RIGWERK_THEME` | `dark`, `light`: wins over the saved theme for the run |
| `RIGWERK_ROUTING_SETUP` | `showcase` (the README setup); `showcase4` (the showcase plus Drive with ten stand-in effects, an empty Stream path and every other output device as an extra output); `drywet`, `sum3`, `blend`, `dualamp-off` (alignment off, the red warning) for the alignment and routing cases |
| `RIGWERK_SCREENSHOT_PLUGINS` | `;` list of `<path number>=<bundle>[#<effect name>]`, used by `showcase` |
| `RIGWERK_SCREENSHOT_SIZE` | `<w>x<h>` window size in points, from `640x480` to `7680x4320` (default `1440x1080`); a blank, unparsable or out-of-range value is logged and the default used |
| `RIGWERK_SCREENSHOT_START` | `1` starts the bridge before the capture |
| `RIGWERK_SCROLL` | pixels to scroll the screen down |

`showcase` is the same setup on every screen: two extra outputs (one of
them RigWerk Out), Blend clean (Clean and Room) with the effects in
`RIGWERK_SCREENSHOT_PLUGINS`, and the metronome at 96 BPM in 4/4, clicking
on the extra headphones only. `blend`, `dualamp-off` and `sum3` fake one
path's plugin delay (no plugin is loaded), so the alignment has something
to show.
