# LV2 host plan

A plan for hosting LV2 effects next to VST3 and CLAP, on Linux and
Windows.  It is written for whoever implements it.  When the work has
landed, delete this file in the last commit (finished plans live in git
history only, see `AGENTS.md`) and move anything still missing to
`docs/open-items.md`.

The main reason is Linux: much of the free guitar DSP there ships only as
LV2 (the Guitarix plugins, rkrlv2, MOD's pitch shifters, x42, Calf).  On
Windows the same code builds and loads the few LV2-only plugins there.
Neither platform gets a special case beyond paths and binary extensions.

## Decisions already made

- **Our own host, on raw bindings.**  `audio/lv2` is written the way
  `audio/clap` is, on `lv2-sys` for the C types.  No `lilv`, no `livi`.
  livi was read and rejected: it keeps one `Instance` for both threads
  (its `run` and `state`/`set_control_input` sit on the same object), its
  URID map is `Mutex<HashMap<CString, u32>>` locked on every call, and its
  worker is a spawned thread that polls every 100 ms.  Its source is still
  a useful reference for the feature plumbing.
- **Turtle with `oxttl`, no system library.**  The scan reads the bundle's
  `.ttl` files with `oxttl` and never loads a binary, as the VST3 scan
  does.  lilv would need `liblilv-dev` in CI and a lilv build bundled on
  Windows.
- **Effects only.**  A plugin is listed when it has at least one audio
  input and one audio output.  Instruments, MIDI-only plugins and
  analysers without audio out are left out.  The class (`lv2:EffectPlugin`
  and its subclasses) is only used for the category tag, never to decide
  what is listed.
- **Variable block sizes, up to `MAX_BLOCK`.**  The chain cuts a bridge
  block into chunks of at most 512 frames (`plugins::MAX_BLOCK`), and the
  last chunk is shorter.  RigWerk offers `buf-size:boundedBlockLength`
  with min 1 and max `MAX_BLOCK`.  A plugin that requires
  `buf-size:fixedBlockLength` or `buf-size:powerOf2BlockLength` is listed
  with a scan error and refuses to load.  Re-blocking it would add
  latency; that is a later decision, not part of this plan.
- **No GTK or Qt editors.**  Only `ui:X11UI` (Linux) and `ui:WindowsUI`
  (Windows) are embedded, and only in phase 2.  Every other plugin uses
  RigWerk's own parameter panel, which covers most guitar LV2s since
  their controls are plain control ports.

## Crates

Both were built in a scratch crate on the pinned 1.95.0 toolchain, for
`x86_64-unknown-linux-gnu` and `x86_64-pc-windows-gnu`, before this plan
was written.

| Crate | Version | Licence | Notes |
|---|---|---|---|
| `lv2-sys` | 2.0.0 | MIT OR Apache-2.0 | Pre-generated bindings for LV2 1.18.0, no build script, no dependencies.  Has `LV2_Feature`, `LV2_Descriptor`, `LV2_URID_Map`, `LV2_Options_Option`, `LV2_Worker_*`, `LV2_State_*` (with map/make/free path), `LV2_Atom_Sequence`, `LV2UI_Descriptor` and the `LV2_BUF_SIZE__*` URIs.  Linux and Windows bindings by `cfg`. |
| `oxttl` | 0.2.5 | MIT OR Apache-2.0 | `TurtleParser::new().with_base_iri(..)?.for_slice(..)` yields triples, with prefixes and relative IRIs resolved against the base.  Use `default-features = false`.  Pulls `oxrdf`, `oxiri`, `oxilangtag`, `memchr`, `thiserror` 2 and (through `oxrdf`) `rand` 0.9. |

`libloading` is already a dependency and opens the plugin binary, as it
does for CLAP.  Check `cargo tree` for duplicate versions after adding the
two crates, and add a comment above each in `Cargo.toml` as the other
entries have.  Before relying on `lv2-sys` on aarch64 Linux, check that
its `linux` bindings are not x86-only.

## Where it goes

```
src/audio/lv2/
  mod.rs        module doc (the contract), re-exports
  ttl.rs        a small triple store over oxttl: load a file, look up by
                subject/predicate, IRIs to paths
  scan.rs       roots, bundle discovery, Lv2Candidate
  module.rs     open the binary, lv2_descriptor(index), keep it loaded
  host.rs       the features RigWerk supplies: URID map/unmap, options,
                bounded block length, worker schedule, log, state paths
  urid.rs       the URID map (see "URID map")
  ports.rs      port table from the TTL, buffers, the control ranges
  instance.rs   Lv2Instance: the UI-thread half
  processor.rs  Lv2Processor: the audio-thread half
  worker.rs     the worker rings and the worker thread
  state.rs      LV2 state to and from RigWerk's blob
  tests.rs      unit tests
  testdata/     .ttl fixtures for the scan tests (text only, no binaries)
```

The format-neutral layer gains a third variant everywhere it has two:

- `audio/plugins/pref.rs`: `PluginFormat::Lv2`, serialised as `"lv2"`,
  label `"LV2"`.  An older build skips an effect it cannot read (the
  preference rules already say so), so `PREFERENCES_SCHEMA` stays.
- `audio/plugins/handle.rs`: `PluginInstance::Lv2(Lv2Instance)` and
  `PluginProcessor::Lv2(Box<Lv2Processor>)`, `LoadedPlugin::lv2`,
  `is_lv2`, and the `Lv2` arms of `refresh_parameters`, `set_parameter`,
  `reset_parameters`, `sync_full`, `service` and `capture_state`.
- `audio/plugins/load.rs`: a `PluginFormat::Lv2` arm in `load_plugin`.
  `path` is the bundle folder, `class_id` the plugin URI (required, like
  the CLAP plugin id).
- `audio/plugins/link.rs`: `ProcessFailure::Lv2(Lv2Failure)` with its own
  packed kind, as `ClapFailure` has.  LV2 `run` returns nothing, so the
  failures are activation ones (`NotActive`, `ActivateFailed`).
- `app/plugins.rs`: `candidate_from_lv2`, `lv2::scan_library()` in
  `scan_library`, `lv2::scan_roots()` in `plugin_scan_folders`, a
  `.lv2` folder in `candidates_from_path` (so "Add from path" works),
  `show_lv2` in `PluginBrowser` and `format_shown`.
- `ui/drawers/browser.rs`: the LV2 format toggle beside VST3 and CLAP.
- `app/plugin_gui.rs` (phase 2 only): `GuiKind::Lv2`.

Keep the split rules in `AGENTS.md`: the UI holds `PluginView` and
`PluginHandle`, never `Lv2Instance` or `Lv2Processor`; `audio/lv2` does not
import `app` or `ui`; new items are `pub(crate)`.

## Threads

LV2 has one `LV2_Handle` per instance, not two objects.  RigWerk still
splits it the way it splits CLAP: `Lv2Instance` owns the handle's
lifetime and every non-audio call, and `Lv2Processor` owns the port
buffers and makes the audio calls.  Both reach the same handle; the
threading classes in the LV2 headers decide who may call what.  Read
`lv2.h`, `state.h` and `worker.h` for the exact text before writing the
calls; the summary here is what the design relies on.

- **Instantiation class** (`instantiate`, `activate`, `deactivate`,
  `cleanup`, and state `restore` unless the plugin has
  `state:threadSafeRestore`): UI thread, and never while a block is inside
  `run`.  At load that is automatic (no block has started).  Later it goes
  through the pause handshake, exactly as a CLAP reactivation does
  (`drive_pause`, `PauseStep::Ready`, then resume).
- **Audio class** (`connect_port`, `run`, and the worker's
  `work_response` and `end_run`): audio thread only.  `connect_port` is
  called again by the processor whenever a buffer pointer may have
  changed, never from the UI.  Since the processor's buffers are
  allocated once at load and never move, connecting once after
  `instantiate` and before the first `run` is enough.  Do it on the UI
  thread at load, before the processor is handed to the slot: the audio
  class only forbids overlapping another audio-class call, and no `run`
  can have started yet.
- **State `save`**: may run while `run` does, per `state.h`.  Check that
  wording; if it holds, `capture_state` calls it under the UI lock with no
  pause, as CLAP's save does.  If not, a capture pauses first.
- **Worker `work`**: on the worker thread, never the audio thread.

Every `Lv2Processor` call runs inside the same audio-thread marker the
CLAP host uses (`clap::host::AudioThreadScope`).  Move that marker to
`audio/plugins` (or share it from there) rather than copying it, since the
URID map and the worker both ask "am I on the audio thread?".

## Scan

`lv2::scan_roots()`:

- `LV2_PATH` when set, which by LV2 convention replaces the defaults.
- Otherwise Linux: `~/.lv2`, `/usr/local/lib/lv2`, `/usr/lib/lv2`,
  `/usr/lib64/lv2`, and the multiarch folder
  (`/usr/lib/<triplet>/lv2`) when it exists.  Windows: `%APPDATA%\LV2`
  and `%COMMONPROGRAMFILES%\LV2`.
- `RIGWERK_LV2_PATH` appended in both cases, like `RIGWERK_CLAP_PATH`.

A bundle is a folder whose name ends in `.lv2` directly under a root.  For
each bundle:

1. Parse `manifest.ttl` with the file's own `file://` URI as the base.
   Every subject with `a lv2:Plugin` is a plugin; its `rdfs:seeAlso`
   files are parsed into the same store.  A file that does not parse
   skips that bundle with one log line.
2. For each plugin read: `doap:name` (name), `doap:maintainer` →
   `foaf:name` or the project's (vendor), its `rdf:type` classes besides
   `lv2:Plugin` (category: the class's local name, e.g. `Amplifier`,
   `Delay`), `lv2:binary`, `lv2:requiredFeature`, `lv2:optionalFeature`,
   `lv2:extensionData`, `ui:ui`, and every `lv2:port` with
   `lv2:index`, `lv2:symbol`, `lv2:name`, its types (`lv2:InputPort`,
   `lv2:OutputPort`, `lv2:AudioPort`, `lv2:ControlPort`,
   `atom:AtomPort`, `lv2:CVPort`), `lv2:default`, `lv2:minimum`,
   `lv2:maximum`, `lv2:portProperty`, `lv2:scalePoint`, `units:unit` and
   `lv2:designation`.
3. Turn `lv2:binary` into a path and check that it exists and has this
   platform's extension (`.so` on Linux, `.dll` on Windows).  A bundle
   built for another platform is left out, as the VST3 scan leaves out a
   bundle without a loadable binary.
4. List it when it has an audio input and an audio output.  List it with a
   `scan_error` (the browser already draws those in the error colour) when
   it needs something RigWerk does not supply: a required feature not in
   `host::SUPPORTED`, a `lv2:CVPort` that is not
   `lv2:connectionOptional`, or more audio ports than the CLAP host's
   `MAX_AUDIO_PORTS` (move that limit to `audio/plugins` to share it).
   The message names the feature, e.g. "Needs a fixed block length".

`file://` URIs become paths through one helper that percent-decodes and
handles Windows drive letters (`file:///C:/Program%20Files/...`).  The
URI is never used as a path directly.

The candidate keeps the bundle folder as `path` and the plugin URI as
`class_id`.  `load_plugin` parses the bundle again; nothing from the scan
is cached across runs.

## Loading

`Lv2Instance::load(bundle, uri, sample_rate, MAX_BLOCK, &restore, &shared)`
on the UI thread:

1. Parse the bundle, find the plugin and build the port table.
2. Open the binary with `libloading`, call `lv2_descriptor(i)` until the
   URI matches.  Keep the library open for as long as any instance from
   it lives (one shared module per binary, as `clap::module` does).
3. Build the features (see "Host features") and call `instantiate` with
   the sample rate and the bundle path (with its trailing separator, as
   the spec requires).  Null is a load error.
4. Allocate every port buffer: `MAX_BLOCK` floats per audio port, one
   float per control port, a sized `LV2_Atom_Sequence` per atom port.
   Connect them all.
5. Restore: the state blob if there is one (see "State"), then the control
   values `Restore::params` returns, written straight into the control
   input buffers (no block is running yet).
6. `activate`.  Read the latency port (see "Latency") into
   `PluginShared`.
7. Return `Lv2Loaded` with the same fields `ClapLoaded` has, so
   `LoadedPlugin::lv2` mirrors `LoadedPlugin::clap`.

A sample-rate change reloads the effect, as it does for the other formats.
Drop order matches CLAP: the processor stops, the slot goes back through
the return queue, and the last `PluginHandle` deactivates and cleans up on
the UI thread.  Before `cleanup`, the instance's worker is stopped and
drained (see "Worker").

## Ports and parameters

Audio: the main stereo in and out pair, by the same rules
`clap::ports::port_layout` uses for one or two channels.  LV2 audio ports
are mono, so "stereo" is two input and two output audio ports, and "mono"
one of each.  A port marked `lv2:connectionOptional` beyond the first two
gets a silent buffer.  Port groups (`pg:`) are not read in phase 1; the
first audio inputs and outputs in index order are the main pair.

Control inputs are the parameters.  `PluginParamView.id` is a stable hash
of the port `lv2:symbol` (32-bit FNV-1a), not the port index: the symbol is
what LV2 promises to keep across plugin versions.  Two symbols that hash
alike in one plugin fall back to the index for the later one, with a log
line.  `ParamIds` maps the id to the index in the processor's control
table, as it does for CLAP.

Normalisation to 0..1, in one immutable range table both halves read
(`Lv2Range`, the twin of `ClapRange`):

- `lv2:minimum`/`lv2:maximum`, defaulting to 0 and 1; `lv2:default`, else
  the minimum.
- `lv2:sampleRate`: the bounds are multiplied by the sample rate.
- `pprops:logarithmic` with a positive minimum: log scale.
- `lv2:integer` and `lv2:enumeration`: stepped, `step_count` is
  `max - min`; with scale points the display is the label of the nearest
  point.
- `lv2:toggled`: one step.
- `units:unit`: the unit's `units:symbol` (or the local name for the
  common units: `db`, `hz`, `ms`, `s`, `pc`).  `display` is the value
  formatted to the unit, or the scale point label.
- `pprops:notOnGUI` controls are not listed.

Values cross exactly as for CLAP: the UI pushes normalised `(index, value)`
pairs into the transfer (`ParamSender`), and at the start of each block
the processor drains them, denormalises with the range table and writes
the float into the port buffer.  Nothing is called on the plugin; LV2
reads its control ports during `run`.  Values the processor could not
apply (none: a write always succeeds) do not exist here, so `Retry` is
not needed, but `applied()` still advances so `Undelivered` settles.

While the plugin is not processing (no stream, or paused), the UI may
write the control buffer itself only from a pause (`drive_pause` ready),
the same rule as CLAP's direct flush.  Otherwise values stay queued and
the existing capture rules apply.

Control outputs (meters, gain reduction) are read after `run`, and only a
value that changed is sent back through the audio→UI transfer.  They are
read-only views.

Atom ports: phase 1 connects every atom input to an empty sequence and
every atom output to a cleared sequence of `ATOM_CAPACITY` bytes (8 KiB),
re-armed each block (`atom.size` set to the capacity before `run`).  No
MIDI, time position or `patch:` messages are sent.  A plugin whose only
parameters are `patch:writable` properties therefore has no panel
controls in phase 1; note it in `open-items.md`.

## Latency

A control output port with `lv2:designation lv2:latency` or
`lv2:portProperty lv2:reportsLatency` reports latency in frames.  Read it
after `activate` (run the plugin for one silent block of `MAX_BLOCK`
frames first if the value is still 0 and the spec's guidance says hosts
should; check `lv2.h`), clamp it with `plugins::clamp_latency` and store it
in `PluginShared`.  The processor also compares the port after every
`run` and flags a change through `PluginShared` (atomics only); the UI's
`service` republishes it, as CLAP's `reread_latency_if_changed` does.

## Host features

`host::SUPPORTED` is the list the scan checks required features against.
Phase 1:

| URI | What RigWerk does |
|---|---|
| `urid:map`, `urid:unmap` | See "URID map". |
| `options:options` | `buf-size:minBlockLength` 1, `buf-size:maxBlockLength` `MAX_BLOCK`, `buf-size:nominalBlockLength` `MAX_BLOCK`, `buf-size:sequenceSize` `ATOM_CAPACITY`, `param:sampleRate`.  A static array built at load. |
| `buf-size:boundedBlockLength` | Supplied (the chunks are bounded). |
| `worker:schedule` | See "Worker". |
| `state:mapPath`, `state:freePath` | Identity mapping (absolute paths stay absolute).  `free_path` frees the strings RigWerk handed out. |

`log:log` is not supplied: its `printf` and `vprintf` callbacks are
variadic or take a `va_list`, which stable Rust can neither define nor
read.  A plugin that requires it gets a scan error.  Before phase 1 is
done, check how many of the target plugins (Guitarix, rkrlv2, MOD, x42)
require it rather than list it as optional.  Supplying it means a small C
shim built with `cc`, a new build-time dependency that needs its own
decision.

Not supplied in phase 1, so a plugin requiring them is listed with a scan
error: `log:log`, `buf-size:fixedBlockLength`,
`buf-size:powerOf2BlockLength`, `state:loadDefaultState`,
`state:makePath`, `instance-access`, `data-access`, `resize-port`, `ui:*`
(UI features belong to phase 2).

## URID map

Plugins should map in `instantiate`, but some map in `run`.  The map must
therefore answer from the audio thread without a lock and without
allocating.

- An append-only table: entries are allocated on the UI or worker thread
  under a `Mutex`, then published by storing the new length with
  `Release`.  A lookup reads the length with `Acquire` and searches the
  published entries with no lock (a linear scan is fine for the few
  hundred URIs a host sees; a fixed open-addressing index is the upgrade
  if profiling ever says so).
- On the audio thread (`AudioThreadScope`) a URI that is not in the table
  returns 0, which the spec allows as failure, and bumps a counter that
  `service` logs once.  It never inserts.
- The table is seeded at startup with every URI RigWerk's own features
  use, so the common lookups never miss.
- `unmap` returns a pointer into the stored, never-moved `CString`.
- One map for the whole process, so URIDs agree between instances (state
  blobs store URIs, not numbers, so nothing depends on the numbers across
  runs).

## Worker

For plugins with `worker:interface` in `lv2:extensionData` (file loads,
IR swaps):

- Per instance, two preallocated SPSC byte rings (request and response),
  64 KiB each, with length-prefixed messages.  `schedule_work` (audio
  thread, inside `run`) copies into the request ring and returns
  `LV2_WORKER_ERR_NO_SPACE` when it does not fit.  No syscall, no lock on
  the audio side.
- One worker thread for all LV2 instances, `rigwerk-lv2-worker`.  The UI's
  `service` unparks it when any request ring is non-empty, so a request
  waits at most a UI frame; the thread parks again with a timeout as a
  safety net.  It calls `work` with a `respond` that writes into the
  response ring.  A `schedule_work` made outside `run` (during
  `restore`, say) may run `work` at once on the calling thread; check
  `worker.h` for what it allows there.
- The processor drains the response ring before each `run` (calling
  `work_response`) and calls `end_run` after it when the plugin has one.
- Removing an instance: the worker thread is told to forget it, and
  `cleanup` waits until the thread is not inside that instance's `work`.
  That wait is on the UI thread, never the audio thread.

## State

LV2 state is a set of keyed properties, not bytes.  `state.rs` turns it
into the bytes `PluginState` already stores (base64 in the preferences):

- `save`: call the plugin's `LV2_State_Interface.save` with a store
  function that appends `(key URI, type URI, flags, value bytes)` to a
  `Vec`.  Keep only values with `LV2_STATE_IS_POD`; refuse a non-portable
  one with a log line.  Serialise the list as a small versioned binary
  layout (a magic, a version, then length-prefixed records), documented
  in the module doc.  Control port values are not part of it; they are
  the saved parameters, which `Restore::params` already applies on top.
- `restore`: parse the layout, map the URIs back to URIDs and call
  `restore` with a retrieve function over the parsed records.  At load
  this is before `activate`; later (not needed in phase 1) it would need
  the pause handshake unless the plugin has `state:threadSafeRestore`.
- A plugin without `LV2_State_Interface` saves nothing: `StateSave`'s
  `Unavailable`, so its parameters alone describe it.
- The blob size cap is the existing `MAX_STATE_BLOB`.

LV2 presets (`pset:`) are not read in phase 1.

## Editors (phase 2)

Only after phase 1 has landed.

- From the scan, a plugin's `ui:ui` with type `ui:X11UI` (Linux) or
  `ui:WindowsUI` (Windows), its `ui:binary` and its required features.
  `PluginView.has_editor` is true only when one of those exists and its
  required features are supplied.
- The UI is its own binary with `lv2ui_descriptor`.  It is instantiated
  on the UI thread with `ui:parent` (the X11 window or HWND
  `app/plugin_gui.rs` already makes for CLAP), `ui:resize`, `urid:map`,
  `options:options` and `ui:idleInterface`, and with a `write_function`
  that turns a control write into `PluginHandle::set_parameter`-style
  pushes (the same path a VST3 editor's `performEdit` takes through
  `ParamOut`).
- `port_event` feeds the UI the current control values when it opens and
  every changed value afterwards, from the views the handle already
  keeps.  Output ports come from the audio→UI transfer.
- `ui:idleInterface.idle` runs once per UI frame from the editor's
  `service`.  A non-zero return closes the window.
- `instance-access` and `data-access` are not supplied: a UI that needs
  them is not offered, and the panel is used instead.

## Tests

Unit tests in `src/audio/lv2/tests.rs`, from text fixtures in
`src/audio/lv2/testdata/` (TTL files, never binaries):

- Scan: manifest plus `rdfs:seeAlso`, prefixes and relative IRIs, several
  plugins in one bundle, an instrument left out, a required unsupported
  feature listed with its error, a binary for the other platform left out.
- `file://` URIs to paths, Linux and Windows forms, percent-encoding.
- Ranges: linear, logarithmic, `lv2:sampleRate`, integer, enumeration with
  scale points, toggled, `notOnGUI`.
- Parameter ids: the symbol hash is stable and a collision falls back.
- URID map: lookups from inside `AudioThreadScope` never insert and never
  block (hold the insert mutex on another thread while looking up), unmap
  round-trips.
- Worker rings: a message that does not fit is refused, messages keep
  their order and length.
- State layout: round trip, an unknown version and a truncated blob are
  rejected without panicking, non-POD values are dropped.
- The browser: `show_lv2` filters, a `.lv2` path goes through
  `candidates_from_path`.

Real plugins, the way `vst3_fixture` works (`audio/plugins/fixtures.rs`):
`RIGWERK_LV2_FIXTURES` names a folder of bundles; unset, the tests pass
without doing anything; set, a missing bundle fails.
`scripts/lv2-fixtures.sh <dir>` builds the example plugins of the LV2
repository (`github.com/lv2/lv2`, pinned tag, meson) into `<dir>/plugins`.
`eg-amp` covers load, process, a control port and the panel; `eg-sampler`
covers the worker and state (it is loaded directly through `audio/lv2` in
the test, since it has no audio input and the browser would not list it).
Wire both into CI on Linux first; add Windows once the script builds there.
Add the steps to `AGENTS.md` beside "Real VST3 plugins".

The gate in `AGENTS.md` (fmt, clippy with `-D warnings`, `cargo test
--locked`) holds after every commit.  Also run `cargo check --target
x86_64-pc-windows-gnu`.

## Commits

One logical change per commit, each passing the gate:

1. Add `lv2-sys` and `oxttl`; `audio/lv2` with `ttl.rs` and `scan.rs`;
   `PluginFormat::Lv2`; the browser lists LV2 effects (adding one fails
   with "LV2 loading is not available yet" until commit 3).
2. The URID map, the host features and the worker rings, with their unit
   tests (not yet wired to a plugin).
3. Load and process: `module.rs`, `ports.rs`, `instance.rs`,
   `processor.rs`, the handle and loader arms, latency.  Parameters
   through the panel.
4. State save and restore.
5. The worker thread wired to instances.
6. The fixture script, the fixture tests and CI.
7. Docs: README features ("VST3/CLAP/LV2 effects"), the manual's effects
   section, `AGENTS.md` (layout line for `audio/lv2`, the fixture
   section), `docs/open-items.md` for what phase 1 leaves out; delete this
   file unless phase 2 is still to come, in which case trim it to phase 2.

Phase 2 (editors) is a separate PR.

## Out of scope

- Fixed or power-of-two block lengths (re-blocking).
- MIDI, time position, `patch:` parameters, CV ports.
- LV2 presets, `state:loadDefaultState`, `state:makePath`.
- GTK, Qt and other non-native editors; `instance-access`.
- macOS (RigWerk does not build there).
