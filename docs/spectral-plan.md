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
- Sizes: Fast 3072/1536, Balanced 4096/2048.  Quality does not switch
  (see 2).

| Mode (48 kHz) | Pitch p95     | Wobble        | Roughness       | *stops* release | Gate fails |
|---------------|---------------|---------------|-----------------|-----------------|------------|
| Fast          | 11.4 → 2.5 c  | 29.2 → 9.7 c  | 2.12 → 1.03 dB  | 16–18 → 15–17 ms | 6 → 3     |
| Balanced      | 20.0 → 2.4 c  | 16.7 → 2.2 c  | 1.85 → 0.70 dB  | 22–24 → 17–20 ms | 6 → 3     |

The remaining failures are attack rise times on the chug cases, which
both builds fail.  Memory: 6.2 → 7.7 MB at 192 kHz.

## 2. Mode sizes

- **Quality and Sustain (done).**  Switched at 8192/4096 Quality holds
  notes as well as the old 40 ms Ultra (pitch 0.28 c, wobble 0.27 c,
  7 gate fails each) at 21 ms, but palm-mutes gain pre-echo (*chugs200*
  −24 → −10 dB): its own `Ns` of 1024 smears them under either window.
  A compromise with a clear downside, so it is a mode of its own,
  *Sustain*; Quality is unchanged.  Ultra no longer earned its delay and
  is gone; a saved `ultra` loads as Sustain.
- **Three modes (done).**  Against the bench, Fast (8 ms) was no better
  than Ultrafast (wobble 9.7 against 8.7 c, release 15–17 against
  10–13 ms), and Quality (21 ms) held notes barely steadier than Balanced
  (1.7 against 2.2 c) with worse palm-mutes.  The set is Ultrafast,
  Balanced and Sustain; saved `fast` loads as Ultrafast, `quality` as
  Balanced, `ultra` as Sustain.
- **Saying what each mode is for (done).**  Hovering a mode shows a card
  with its delay, five-dot ratings for held notes, palm-mutes and feel,
  and its use; the chosen mode's use stays under the controls.
- **Ultrafast (about 5 ms, done).**  3072/1024 with `Ns` 256 and a hop of
  64: pitch p95 2.6 c, wobble 8.7 c, roughness 1.07 dB, *stops* release
  10–13 ms, 2 gate fails (the old 1024/256 Fast: 13 fails, 37 c).  The
  window choice's newest stretch is `2·Ns` but at least 16 ms: Ultrafast's
  10.7 ms took a 55 Hz beat's troughs for stops.  Held 4096/2048 steadies
  notes further (wobble 4.6 c) but releases in 19–22 ms; 2048/1024 does not
  resolve low power chords.  About 1.5 times Fast's CPU (more frames).

## 3. Per-peak transient handling (done)

An onset reset every peak's phase, so a note picked over a ringing string
dipped the string's shifted partials by 8–24 dB in Balanced and Sustain
while the old and new phases cross-faded (A2 held, F4 picked, −5).  The
whole-region shifting was harmless; the phase reset alone did it.
Dropping the reset fixes that but loosens palm-mutes (Balanced *chugs200*
pre-echo −30 → −19 dB).

Now an onset restarts only its own peaks, all in the onset's frames (the
set does not shrink, so one attack does not come apart): a peak with no
predecessor, one risen 1.6 dB over it, or one whose energy centre sits a
quarter `Ns` later than a steady sinusoid's.  The energy centre is
Röbel's (DAFx 2003): the time-weighted transform rides in the imaginary
part of the same complex FFT, so it costs no transform.  His band
statistics and his wait for the analysis window's centre are left out:
the global flux test already gates onsets, and here the reference is the
synthesis window's centre.

- Ringing string: dip ≤ 2.1 dB in every mode (was up to 22.4 dB), now a
  test.
- Balanced and Ultrafast: every chug attack and gate row as before.
- Sustain: *f8chugs* rise 21.4 → 16.4 ms, pre-echo −12.6 → −16.7 dB; two
  *chugs200* rows slower.
- Level rise alone missed chugs picked again before the last one died
  away (Balanced *chugs200* rise 5 → 9 ms); the energy centre alone left
  Sustain's palm-mutes worse.  Both are kept.

## 4. Real-input FFT (tried, not taken)

Window switching costs about 1.7 times the CPU in Fast and Balanced (a
looped strum on this machine: Fast 2.7 → 4.5 %, Balanced 2.0 → 3.5 % of one
core; Quality unchanged).  `realfft` did not win it back: a
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
