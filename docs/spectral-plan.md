# Spectral engine: resolution without latency

The spectral engine's latency is `Ns − 1`; its resolution is that of the
analysis length `Na`.  Every mode used `Na = 4·Ns`, but nothing ties the
two.  A scoring bench over the harness cases (−12, −5, +7, +12) showed:

| Variant (48 kHz)         | Latency | Pitch p95 | Wobble  | Roughness | Gate fails |
|--------------------------|---------|-----------|---------|-----------|------------|
| Fast 1536 / 384          | 8 ms    | 11.4 c    | 29.2 c  | 2.12 dB   | 6          |
| Fast 3072 / 384          | 8 ms    | 2.5 c     | 6.6 c   | 1.03 dB   | 5          |
| Balanced 2048 / 512      | 11 ms   | 20.0 c    | 16.7 c  | 1.85 dB   | 6          |
| Balanced 4096 / 512      | 11 ms   | 3.1 c     | 2.3 c   | 0.72 dB   | 5          |
| Quality 4096 / 1024      | 21 ms   | 3.0 c     | 1.7 c   | 0.62 dB   | 5          |

A long `Na` fixes sustained notes and low power chords (`e5`, `b1`), but
keeps the last note in view longer: palm-mutes gain pre-echo (`chugs200`
about −30 → −22 dB) and stops release later (`stops` 23 → 34 ms).  A short
`Na` fails the other way.

## 1. Window switching per frame (done)

Use the long analysis window while the input is steady and the short one
while it is not.  Each frame's synthesis window is the Hann of `Ns` divided
by that frame's analysis window, so every product is the same Hann and the
overlap-add stays exact whatever sequence of windows the frames used
(Mauler and Martin's low-delay, switchable analysis-synthesis windows).

- The short window is zero-padded to the long length, so both share the
  frame start, `t_ref`, the rotation table and the FFT size; phases stay
  continuous across a switch.
- The choice is made in the time domain before the frame is windowed: the
  mean power of the newest `2·Ns` samples against the rest of the long
  window, 6 dB either way.  A newest stretch of `Ns/2` took a beating
  chord's troughs for stops (a held 110/165/220 Hz chord flickered short
  a third of the time); the whole short window noticed a stop late
  (*stops* release 16 → 24 ms in Fast).
- A switch's own edge is the onset or the drop: the spectral flux tests
  skip a frame whose window differs from the last one's.
- Sizes: Fast 3072/1536, Balanced 4096/2048.  Quality and Ultra do not
  switch (see 2).

| Mode (48 kHz) | Pitch p95     | Wobble        | Roughness       | *stops* release | Gate fails |
|---------------|---------------|---------------|-----------------|-----------------|------------|
| Fast          | 11.4 → 2.5 c  | 29.2 → 9.7 c  | 2.12 → 1.03 dB  | 16–18 → 15–17 ms | 6 → 3     |
| Balanced      | 20.0 → 2.4 c  | 16.7 → 2.2 c  | 1.85 → 0.70 dB  | 22–24 → 17–20 ms | 6 → 3     |

The remaining failures are attack rise times on the chug cases, which
both builds fail.  Memory: 6.2 → 7.7 MB at 192 kHz.

## 2. Mode sizes

- **Quality.**  Switched at 8192/4096 it holds notes as well as Ultra
  (pitch 0.3 c, wobble 0.3 c) at 21 ms, but palm-mutes gain pre-echo
  (*chugs200* −24 → −18 dB) because its own `Ns` of 1024 smears them under
  either window.  That is a compromise with a clear downside, so it is a
  setting, not a default: for example a *Sustain* choice on Quality, or
  Quality keeps its place and the long window becomes what Ultra is.
  Decide by listening.
- **Ultrafast (about 5 ms).**  The old 1024/256 Fast was poor (13 gate
  fails, 37 c wobble).  Switched at 3072/1536 with `Ns` 256 and a hop of 64
  it scored 2 fails and 9 c wobble with the half-window detector; tune it
  with the final detector before adding the mode.

## 3. Per-bin transient handling (Röbel, DAFx 2003)

An onset now resets every phase and moves the whole spectrum by regions,
so a new note disturbs a string that keeps ringing.  Röbel detects and
handles transients per bin (from the time centre of gravity of each
peak's energy), so steady partials stay locked through another note's
attack.  Measure on legato over a held chord.

## 4. Real-input FFT (tried, not taken)

Window switching costs about 1.7 times the CPU in Fast and Balanced (a
looped strum on this machine: Fast 2.7 → 4.5 %, Balanced 2.0 → 3.5 % of one
core; Quality and Ultra unchanged).  `realfft` did not win it back: a
profile puts both transforms at about 5 % of the engine's time, so halving
them saves 2–3 % and 0.6 MB, not worth a dependency.  The time is in the
per-frame peak work inside `tick` (the lobe fits, the per-bin loops).  If
the CPU matters, profile those first: fewer fit sweeps for weak peaks, or
a cheaper `lobe()` interpolation.

## 5. Later, if wanted

- Formant preservation for large upward shifts: the spectral envelope
  (Röbel and Rodet, true envelope) kept in place under the shifted
  partials.  A feature, one more FFT pair per frame.
- Fuzzy sine/noise/transient classification per bin (Damskägg and
  Välimäki 2017) for the residual's phases.  Suits mixed material more
  than a DI guitar.
