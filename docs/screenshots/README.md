# Screenshots

The images for the [README](../../README.md) and the
[manual](../manual.md), and how to take them again.
Run the commands from the repository root.

The set in this folder is current: it shows the console, its sheets and the
browser, the tuner screen and Setup, with two NAM blocks and a cab IR on
Room.  It was taken on Ubuntu 24.04 with the Zam plugins (`ZamCompX2`,
`ZamVerb` and `ZaMaximX2`, from the `zam-plugins` package).  The captures
are the example models of
[NeuralAmpModelerCore](https://github.com/sdatkinson/NeuralAmpModelerCore)
(`lstm.nam` and `wavenet.nam`, MIT) with their `metadata.name` set to
*Drive Pedal* and *Crunch Amp*; the IR is a generated 2048-sample
*Closed Back 4x12*.  None of them is committed.

## Taking the set

The set is taken under `xvfb-run -a` with the null-sink graph below and
`RIGWERK_SCREENSHOT_START=1` so the meters, the scope and the extra outputs
show real signal. Take each file with a fresh `HOME` (no saved preferences),
so no saved page, sheet or theme leaks in. Use a virtual graph, not real
hardware.

```bash
export RIGWERK_ROUTING_SETUP=showcase RIGWERK_SCREENSHOT_START=1
export RIGWERK_SCREENSHOT_SIZE=1440x900
export RIGWERK_SCREENSHOT_PLUGINS="1=/usr/lib/vst3/ZamCompX2.vst3;\
2=/usr/lib/vst3/ZamVerb.vst3;\
2=/usr/lib/vst3/ZaMaximX2.vst3"
export RIGWERK_SCREENSHOT_NAM="2=$HOME/Music/NAM/Drive Pedal.nam;\
2=$HOME/Music/NAM/Crunch Amp.nam"
export RIGWERK_SCREENSHOT_CAB="2=$HOME/Music/IR/Closed Back 4x12.wav"

shot() {
  rm -rf "/tmp/shot-$1" && mkdir -p "/tmp/shot-$1"
  HOME="/tmp/shot-$1" LP_NUM_THREADS=1 \
    xvfb-run -a -s "-screen 0 3200x2000x24" cargo run --release -- \
    --screenshot "docs/screenshots/$1"
}
```

| File | Command | Shows |
| --- | --- | --- |
| `console.png` | `RIGWERK_PAGE=console shot console.png` | the console with two paths (Clean and Room) |
| `console-4-paths.png` | `RIGWERK_ROUTING_SETUP=showcase4 shot console-4-paths.png` | four paths, four outputs and a long chain |
| `console-light.png` | `RIGWERK_THEME=light shot console-light.png` | the same console in the light theme |
| `nam-sheet.png` | `RIGWERK_SHEET=nam shot nam-sheet.png` | the first NAM block's sheet on Room |
| `cab-sheet.png` | `RIGWERK_SHEET=cab shot cab-sheet.png` | the cab sheet on Room |
| `eq-sheet.png` | `RIGWERK_SHEET=eq shot eq-sheet.png` | the Pre EQ sheet on Room |
| `output-sheet.png` | `RIGWERK_SHEET=output shot output-sheet.png` | the clock output's sheet |
| `match-paths.png` | `RIGWERK_ROUTING_SETUP=showcase RIGWERK_MATCH_PATHS=1 RIGWERK_SHEET=match-paths shot match-paths.png` | the Match paths sheet after a measurement |
| `browser.png` | `RIGWERK_DRAWER=browser shot browser.png` | the effect browser |
| `tuner.png` | `RIGWERK_PAGE=tuner shot tuner.png` | the tuner screen |
| `setup.png` | `RIGWERK_PAGE=setup shot setup.png` | Setup |
| `console-1040x700.png` | `RIGWERK_SCREENSHOT_SIZE=1040x700 RIGWERK_PAGE=console shot console-1040x700.png` | the console at the minimum window size |

The large Xvfb screen keeps the pointer, which starts in its centre, off
the window, so no tooltip shows.  `LP_NUM_THREADS=1` keeps Mesa's software
renderer from starving the audio thread.  On a shared VM the DSP alarm can
still trip on scheduling jitter at the 64-sample buffer, and a capture's
load is measured once while the app starts: retake a shot that shows the
amber ▲ beside a path's DSP figure or an implausible block load.

The default size is 1440 × 1080; the set above asks for 1440 × 900 because
that is the size the console is designed at. `--screenshot <file>` renders
the window at that size and saves it, always through X11/XWayland, because
Wayland ignores the requested size. The plugin list matters for the console
and the sheets: `showcase` selects Room, whose chain holds the reverb and
limiter.

## A PipeWire graph without hardware

On a distro with the `pipewire`, `wireplumber` and `dbus` packages, run this
inside one D-Bus session, with a short runtime directory:

```bash
export XDG_RUNTIME_DIR=$(mktemp -d /tmp/pwXXXX) && chmod 700 "$XDG_RUNTIME_DIR"
dbus-run-session -- bash -c '
  pipewire & sleep 2
  wireplumber & sleep 2
  pw-cli create-node adapter "{ factory.name=support.null-audio-sink
    node.name=studio_monitors node.description=\"Studio Monitors\"
    media.class=Audio/Sink audio.position=[FL FR] object.linger=true
    priority.session=2000 }"
  pw-cli create-node adapter "{ factory.name=support.null-audio-sink
    node.name=usb_headphones node.description=\"USB Headphones\"
    media.class=Audio/Sink audio.position=[FL FR] object.linger=true
    priority.session=1000 }"
  pw-cli create-node adapter "{ factory.name=support.null-audio-sink
    node.name=desk_speakers node.description=\"Desk Speakers\"
    media.class=Audio/Sink audio.position=[FL FR] object.linger=true
    priority.session=1500 }"
  pw-cli create-node adapter "{ factory.name=support.null-audio-sink
    node.name=guitar_interface node.description=\"Guitar Interface\"
    media.class=Audio/Source/Virtual audio.position=[FL FR]
    object.linger=true priority.session=2000 }"
  pw-metadata -n settings 0 clock.force-quantum 64
  pw-cat -p --target 0 --volume 0.35 -P "{ node.name=guitar_feed }" pluck.wav &
  sleep 2
  pw-link guitar_feed:output_FL guitar_interface:input_FL
  pw-link guitar_feed:output_FR guitar_interface:input_FR
  wait
'
```

The runtime directory must be short: PipeWire's socket path is limited to
108 bytes. The app needs the X11 and GL packages under Xvfb (`xvfb`,
`mesa-dri-gallium`, `libxkbcommon-x11`). On Alpine install `pipewire`,
`pipewire-tools`, `wireplumber` and `dbus`; on Debian the equivalents in
`.cursor/install.sh`. The plugins come from `zam-plugins` (Ubuntu) or
`zam-plugins-vst3` (Alpine); a distro's equivalent VST3 compressor, reverb
and limiter work too. In a screenshot the tuner draws a fixed demo frame
(Drop C, strobe, all strings, auto-mute on), not the live input.

Environment variables for screenshot runs:

| Variable | Values |
| --- | --- |
| `RIGWERK_PAGE` | `console`, `tuner`, `setup` |
| `RIGWERK_SHEET` | `eq`, `gate`, `nam`, `cab`, `effect` (a stand-in effect when the chain is empty), `path`, `output` (the clock output), `match-paths`, `scope`, `diagnostics`; on the selected path, applied after the setup |
| `RIGWERK_DRAWER` | `browser`, `issues` |
| `RIGWERK_THEME` | `dark`, `light`: wins over the saved theme for the run |
| `RIGWERK_ROUTING_SETUP` | `showcase` (the README setup); `showcase4` (the showcase plus Drive with ten bypassed copies of the room's first effect and an empty Stream path); `drywet`, `sum3`, `blend`, `dualamp-off` (alignment off, the red warning) for the alignment and routing cases |
| `RIGWERK_SCREENSHOT_PLUGINS` | `;` list of `<path number>=<bundle>[#<effect name>]`, used by `showcase` |
| `RIGWERK_SCREENSHOT_NAM` | `;` list of `<path number>=<file.nam>`, one block each, before the effects, used by `showcase` |
| `RIGWERK_SCREENSHOT_CAB` | `;` list of `<path number>=<file.wav>`, the path's cab, used by `showcase` |
| `RIGWERK_SCREENSHOT_SIZE` | `<w>x<h>` window size in points, from `640x480` to `7680x4320` (default `1440x1080`); a blank, unparsable or out-of-range value is logged and the default used |
| `RIGWERK_SCREENSHOT_START` | `1` starts the bridge before the capture |
| `RIGWERK_SCROLL` | pixels to scroll the screen down |

`showcase` is the same setup on every screen: two extra outputs (one of them
RigWerk Out), Blend clean (Clean and Room) with the effects in
`RIGWERK_SCREENSHOT_PLUGINS`, the NAM blocks and cab in
`RIGWERK_SCREENSHOT_NAM` and `RIGWERK_SCREENSHOT_CAB`, and the metronome at 96 BPM in 4/4, clicking
on the extra headphones only. `blend`, `dualamp-off` and `sum3` fake one
path's plugin delay (no plugin is loaded), so the alignment has something
to show.
