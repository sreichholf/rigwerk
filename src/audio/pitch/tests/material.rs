//! The phase 0 test material: a small additive synth with known partials.
//!
//! Each note is a sum of sines (slight inharmonicity, a few ms attack,
//! per-partial exponential decay, higher partials faster) plus a short seeded
//! noise burst for the pick.  The same partial list scaled by the ratio
//! renders the reference at the target pitch, so every score knows exactly
//! which frequencies the output should hold.  Not Karplus–Strong: its
//! partials are not known in closed form.
//!
//! A partial can beat: a real string vibrates in two polarisations whose
//! frequencies differ slightly (the bridge's admittance is not the same
//! across and along the top), and unison strings couple through the bridge,
//! so a guitar partial is often a pair a fraction of a Hz to a few Hz apart,
//! at unequal levels, the weaker one decaying more slowly (a two-stage
//! decay).  [`beating`] turns a note's partials into such pairs; the values
//! follow the literature (Weinreich, "Coupled piano strings", JASA 1977;
//! Woodhouse, "On the synthesis of guitar plucks" and "Plucked guitar
//! transients", Acta Acustica 2004; Erkut, Välimäki, Karjalainen, Laurson,
//! "Extraction of physical and expressive parameters for model-based sound
//! synthesis of the classical guitar", AES 108th Convention, 2000): about
//! 0.1–0.5 Hz on the fundamental, growing with the partial number to a few
//! Hz, the second component 5–10 dB down and decaying 1.5–3 times more
//! slowly.  The scores count a pair as one partial at its power-weighted
//! centre ([`partials_at`]).

use std::f64::consts::TAU;

/// One sine of a note.
#[derive(Clone, Copy, Debug)]
pub(super) struct Partial {
    pub(super) freq: f64,
    pub(super) amp: f64,
    /// Exponential decay time constant, seconds.
    pub(super) tau: f64,
    pub(super) phase: f64,
    /// A second component this partial beats with.
    pub(super) beat: Option<Beat>,
}

/// The second component of a beating partial.
#[derive(Clone, Copy, Debug)]
pub(super) struct Beat {
    /// Its distance from the partial, Hz (either sign); scaled with the
    /// partial when a case is rendered at another pitch.
    pub(super) offset: f64,
    pub(super) amp: f64,
    pub(super) tau: f64,
    pub(super) phase: f64,
}

/// How a note's pitch moves.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Motion {
    Still,
    /// A bend up by `semitones`, starting `start` seconds after the onset
    /// and taking `seconds`.
    Bend {
        start: f64,
        seconds: f64,
        semitones: f64,
    },
    /// Vibrato of `depth` semitones (peak) at `rate` Hz.
    Vibrato {
        rate: f64,
        depth: f64,
    },
}

impl Motion {
    /// The pitch offset in semitones `t` seconds after the onset.
    pub(super) fn semitones(self, t: f64) -> f64 {
        match self {
            Motion::Still => 0.0,
            Motion::Bend {
                start,
                seconds,
                semitones,
            } => {
                let x = ((t - start) / seconds).clamp(0.0, 1.0);
                semitones * (0.5 - 0.5 * (std::f64::consts::PI * x).cos())
            }
            Motion::Vibrato { rate, depth } => {
                // Vibrato starts after the attack.
                let ramp = (t / 0.15).clamp(0.0, 1.0);
                depth * ramp * (TAU * rate * t).sin()
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct Note {
    pub(super) onset: f64,
    /// Where the note stops dead, if it does.
    pub(super) stop: Option<f64>,
    pub(super) partials: Vec<Partial>,
    pub(super) motion: Motion,
    pub(super) pick: f64,
    pub(super) seed: u64,
}

/// What a case is scored on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    /// Single notes and power chords: pitch, leftovers, level.
    Notes,
    /// Full chords: also roughness.  Reported, not held (§4.3).
    Chord,
    /// A cluster the scoring cannot resolve: rendered and scored, never
    /// pass/fail.
    Listening,
    /// The pitch moves: tracked, not scored on fixed partials.
    Moving,
    /// Palm-muted chugs: attack smear and pre-echo.
    Chugs,
    /// Notes that stop dead: the release.
    Stops,
    /// The sine sweep, the click train and the impulse.
    Signal,
}

#[derive(Clone, Debug)]
pub(super) struct Case {
    pub(super) name: &'static str,
    pub(super) kind: Kind,
    pub(super) seconds: f64,
    pub(super) notes: Vec<Note>,
    /// The sustain window the partials are measured in, seconds.
    pub(super) sustain: (f64, f64),
    /// A special signal instead of notes.
    pub(super) signal: Option<Signal>,
    /// Partials of different notes closer than this (Hz, at the input's
    /// pitch) count as one, at their power-weighted centre; 0 for none.
    pub(super) merge_hz: f64,
}

/// How close two unison strings' partials are merged: a fretted note a few
/// cents sharp is about 7 Hz off the open string by the tenth partial.
const UNISON_MERGE_HZ: f64 = 10.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Signal {
    /// A log sweep from `from` to `to` Hz at −12 dBFS.
    Sweep { from: f64, to: f64 },
    /// Single-sample clicks every `period` seconds.
    Clicks { period: f64 },
    /// One sample at 0.5.
    Impulse,
}

/// The frequency of MIDI note `note`.
pub(super) fn midi(note: i32) -> f64 {
    440.0 * 2f64.powf((note as f64 - 69.0) / 12.0)
}

pub(super) const E2: i32 = 40;
const F2: i32 = 41;
const FS2: i32 = 42;
const G2: i32 = 43;
const FS1: i32 = 30;
const CS2: i32 = 37;
const A1: i32 = 33;
const B1: i32 = 35;
const A2: i32 = 45;
const B2: i32 = 47;
const C3: i32 = 48;
const D3: i32 = 50;
const DS3: i32 = 51;
const E3: i32 = 52;
const G3: i32 = 55;
const A3: i32 = 57;
const GS3: i32 = 56;
const B3: i32 = 59;
const C4: i32 = 60;
const CS4: i32 = 61;
const D4: i32 = 62;
const DS4: i32 = 63;
const E4: i32 = 64;
const F4: i32 = 65;
const FS4: i32 = 66;
const G4: i32 = 67;
const A4: i32 = 69;
const F3: i32 = 53;
const FS3: i32 = 54;

/// A small deterministic generator for phases and the pick noise.
pub(super) struct Lcg(pub(super) u64);

impl Lcg {
    pub(super) fn next(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 11) as f64) / ((1u64 << 53) as f64)
    }
}

/// A plucked string at `f0`: partials up to 6 kHz (at most 20), amplitude
/// falling with the partial number, decay `tau` for the fundamental and
/// faster above.
pub(super) fn string(f0: f64, onset: f64, level: f64, tau: f64, seed: u64) -> Note {
    string_with(f0, onset, level, tau, seed, 8e-5)
}

/// [`string`] with its own inharmonicity coefficient (a wound or a heavier
/// string has a larger one).
fn string_with(f0: f64, onset: f64, level: f64, tau: f64, seed: u64, inharmonicity: f64) -> Note {
    let mut rng = Lcg(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0x5DEE_CE66);
    let mut partials = Vec::new();
    for k in 1..=20u32 {
        let k_f = k as f64;
        let freq = k_f * f0 * (1.0 + inharmonicity * k_f * k_f).sqrt();
        if freq > 6000.0 {
            break;
        }
        // A pluck near the bridge: a gentle tilt, no notches.
        let amp = level / k_f.powf(0.9);
        partials.push(Partial {
            freq,
            amp,
            tau: tau / (1.0 + 0.12 * (k_f - 1.0)),
            phase: rng.next() * TAU,
            beat: None,
        });
    }
    Note {
        onset,
        stop: None,
        partials,
        motion: Motion::Still,
        pick: 0.15 * level,
        seed,
    }
}

/// Makes every partial of `note` a beating pair (see the module docs): the
/// offset is 0.12–0.45 Hz on the fundamental and grows by about 0.25 Hz per
/// partial number, capped at 4 Hz, with a random sign; the second component
/// is 0.3–0.6 of the first (−10 … −4.4 dB) and decays 1.5–3 times more
/// slowly.
pub(super) fn beating(mut note: Note, seed: u64) -> Note {
    let mut rng = Lcg(seed ^ 0xBEA7_0000_1234_5678);
    for (k, partial) in note.partials.iter_mut().enumerate() {
        let spread = 0.6 + 0.8 * rng.next();
        let offset = (spread * (0.2 + 0.25 * k as f64)).min(4.0);
        let sign = if rng.next() < 0.5 { -1.0 } else { 1.0 };
        partial.beat = Some(Beat {
            offset: sign * offset,
            amp: partial.amp * (0.3 + 0.3 * rng.next()),
            tau: partial.tau * (1.5 + 1.5 * rng.next()),
            phase: rng.next() * TAU,
        });
    }
    note
}

/// A chord of MIDI notes struck together (or strummed `spread` seconds
/// apart, low string first), each string at the same level.
fn chord(notes: &[i32], onset: f64, spread: f64, tau: f64, seed: u64) -> Vec<Note> {
    let level = 0.5 / (notes.len() as f64).sqrt();
    notes
        .iter()
        .enumerate()
        .map(|(i, &n)| {
            string(
                midi(n),
                onset + spread * i as f64,
                level,
                tau,
                seed + i as u64 * 17,
            )
        })
        .collect()
}

fn notes_case(name: &'static str, kind: Kind, notes: Vec<Note>, seconds: f64) -> Case {
    Case {
        name,
        kind,
        seconds,
        notes,
        sustain: (0.25, seconds - 0.1),
        signal: None,
        merge_hz: 0.0,
    }
}

/// Every case of §4.1.
pub(super) fn all_cases() -> Vec<Case> {
    let mut cases = vec![
        notes_case(
            "e2",
            Kind::Notes,
            vec![string(midi(E2), 0.05, 0.5, 1.5, 1)],
            1.1,
        ),
        notes_case(
            "a2",
            Kind::Notes,
            vec![string(midi(A2), 0.05, 0.5, 1.5, 2)],
            1.1,
        ),
        notes_case(
            "e4",
            Kind::Notes,
            vec![string(midi(E4), 0.05, 0.5, 1.0, 3)],
            1.1,
        ),
        notes_case(
            "b1",
            Kind::Notes,
            vec![string(midi(B1), 0.05, 0.5, 1.8, 4)],
            1.1,
        ),
        notes_case(
            "e5",
            Kind::Notes,
            chord(&[E2, B2, E3], 0.05, 0.0, 1.5, 5),
            1.1,
        ),
    ];
    let mut bend = string(midi(G3), 0.05, 0.5, 1.5, 6);
    bend.motion = Motion::Bend {
        start: 0.25,
        seconds: 0.25,
        semitones: 2.0,
    };
    cases.push(notes_case("bend", Kind::Moving, vec![bend], 1.1));
    let mut vibrato = string(midi(B3), 0.05, 0.5, 1.5, 7);
    vibrato.motion = Motion::Vibrato {
        rate: 5.5,
        depth: 0.3,
    };
    cases.push(notes_case("vibrato", Kind::Moving, vec![vibrato], 1.1));

    // Palm-muted E5 chugs at 16ths, 160 bpm.
    let sixteenth = 60.0 / 160.0 / 4.0;
    let mut chugs = Vec::new();
    for i in 0..8 {
        chugs.extend(chord(
            &[E2, B2, E3],
            0.05 + i as f64 * sixteenth,
            0.0,
            0.018,
            100 + i,
        ));
    }
    cases.push(Case {
        name: "chugs",
        kind: Kind::Chugs,
        seconds: 0.05 + 8.0 * sixteenth + 0.15,
        notes: chugs,
        sustain: (0.0, 0.0),
        signal: None,
        merge_hz: 0.0,
    });

    cases.push(notes_case(
        "emaj7",
        Kind::Chord,
        chord(&[E2, B2, DS3, GS3, B3, E4], 0.05, 0.0, 1.8, 10),
        1.1,
    ));
    cases.push(notes_case(
        "cadd9",
        Kind::Chord,
        chord(&[C3, E3, G3, D4, E4], 0.05, 0.0, 1.8, 11),
        1.1,
    ));
    cases.push(notes_case(
        "e7s9",
        Kind::Chord,
        chord(&[E2, GS3, D4, G4], 0.05, 0.0, 1.8, 12),
        1.1,
    ));
    cases.push(notes_case(
        "dropa",
        Kind::Chord,
        chord(&[A1, E2, A2, C3, E3], 0.05, 0.0, 1.8, 13),
        1.1,
    ));
    // A fast strum: three chords, strings 8 ms apart; the last one rings.
    let mut strum = chord(&[E2, B2, E3, GS3, B3, E4], 0.05, 0.008, 0.25, 14);
    strum.extend(chord(&[A2, E3, A3, CS4, E4], 0.30, 0.008, 0.25, 15));
    strum.extend(chord(&[E2, B2, E3, GS3, B3, E4], 0.55, 0.008, 1.8, 16));
    cases.push(Case {
        name: "strum",
        kind: Kind::Chord,
        seconds: 1.45,
        notes: strum,
        sustain: (0.75, 1.35),
        signal: None,
        merge_hz: 0.0,
    });
    cases.push(notes_case(
        "cluster",
        Kind::Listening,
        chord(&[E2, F2, G2, B3, E4], 0.05, 0.0, 1.8, 17),
        1.1,
    ));

    // Beating partials: a single note, a chord, and an open chord with a
    // unison (the G string fretted at the fourth fret against the open B,
    // a few cents sharp as a fretted note is, on a heavier string).  Long,
    // so the slow beats show in the sustain.
    cases.push(notes_case(
        "a2beat",
        Kind::Notes,
        vec![beating(string(midi(A2), 0.05, 0.5, 2.0, 30), 30)],
        2.5,
    ));
    cases.push(notes_case(
        "gbeat",
        Kind::Chord,
        chord(&[G2, B2, D3, G3, B3, G4], 0.05, 0.0, 2.2, 31)
            .into_iter()
            .enumerate()
            .map(|(i, n)| beating(n, 31 + i as u64))
            .collect(),
        2.5,
    ));
    let level = 0.5 / 6f64.sqrt();
    let mut unison: Vec<Note> = [E2, B2, E3, B3, E4]
        .into_iter()
        .enumerate()
        .map(|(i, n)| {
            let seed = 40 + i as u64;
            beating(string(midi(n), 0.05, level, 2.2, seed), seed)
        })
        .collect();
    unison.push(beating(
        string_with(
            midi(B3) * 2f64.powf(4.0 / 1200.0),
            0.05,
            level,
            2.2,
            46,
            1.4e-4,
        ),
        46,
    ));
    let mut unison = notes_case("unison", Kind::Chord, unison, 2.5);
    // The unison strings' partials are one partial each for the scores.
    unison.merge_hz = UNISON_MERGE_HZ;
    cases.push(unison);

    // Barre chords, the shapes a rhythm part is made of: E-shape (six
    // strings) and A-shape (five), major, minor and a dominant 7.  A
    // downstroke 4 ms a string, every partial beating, ringing.
    for (i, (name, notes)) in [
        ("fbarre", &[F2, C3, F3, A3, C4, F4][..]),
        ("fmbarre", &[F2, C3, F3, GS3, C4, F4][..]),
        ("abarre", &[A2, E3, A3, CS4, E4, A4][..]),
        ("ambarre", &[A2, E3, A3, C4, E4, A4][..]),
        ("a7barre", &[A2, E3, G3, CS4, E4, A4][..]),
        ("bbarre", &[B2, FS3, B3, DS4, FS4][..]),
        ("bmbarre", &[B2, FS3, B3, D4, FS4][..]),
    ]
    .into_iter()
    .enumerate()
    {
        let seed = 60 + 10 * i as u64;
        let notes = chord(notes, 0.05, 0.004, 2.0, seed)
            .into_iter()
            .enumerate()
            .map(|(j, n)| beating(n, seed + j as u64))
            .collect();
        let mut case = notes_case(name, Kind::Chord, notes, 2.0);
        case.sustain = (0.3, 1.9);
        cases.push(case);
    }

    // Low tuning: a drop-A power chord (root, fifth, octave: a common
    // period of 27.5 Hz) ringing, and palm-muted B1 power chords at
    // sixteenths, 200 bpm.
    cases.push(notes_case(
        "lowpower",
        Kind::Notes,
        chord(&[A1, E2, A2], 0.05, 0.0, 1.5, 80),
        1.1,
    ));
    let sixteenth = 60.0 / 200.0 / 4.0;
    let mut fast = Vec::new();
    for i in 0..12 {
        fast.extend(chord(
            &[B1, FS2, B2],
            0.05 + i as f64 * sixteenth,
            0.0,
            0.018,
            90 + i,
        ));
    }
    cases.push(Case {
        name: "chugs200",
        kind: Kind::Chugs,
        seconds: 0.05 + 12.0 * sixteenth + 0.15,
        notes: fast,
        sustain: (0.0, 0.0),
        signal: None,
        merge_hz: 0.0,
    });

    // An eight-string's low F#: its power chord (a common period of
    // 23 Hz) ringing, and palm-muted at sixteenths, 180 bpm.
    cases.push(notes_case(
        "f8power",
        Kind::Notes,
        chord(&[FS1, CS2, FS2], 0.05, 0.0, 1.5, 85),
        1.1,
    ));
    let sixteenth = 60.0 / 180.0 / 4.0;
    let mut eight = Vec::new();
    for i in 0..10 {
        eight.extend(chord(
            &[FS1, CS2, FS2],
            0.05 + i as f64 * sixteenth,
            0.0,
            0.02,
            110 + i,
        ));
    }
    cases.push(Case {
        name: "f8chugs",
        kind: Kind::Chugs,
        seconds: 0.05 + 10.0 * sixteenth + 0.15,
        notes: eight,
        sustain: (0.0, 0.0),
        signal: None,
        merge_hz: 0.0,
    });

    // Notes that stop dead.
    let mut stops = Vec::new();
    for (i, n) in [E2, A2, E3].into_iter().enumerate() {
        let mut note = string(midi(n), 0.05 + 0.4 * i as f64, 0.4, 1.5, 20 + i as u64);
        note.stop = Some(0.05 + 0.4 * i as f64 + 0.25);
        stops.push(note);
    }
    cases.push(Case {
        name: "stops",
        kind: Kind::Stops,
        seconds: 1.35,
        notes: stops,
        sustain: (0.0, 0.0),
        signal: None,
        merge_hz: 0.0,
    });

    for (name, signal, seconds) in [
        (
            "sweep",
            Signal::Sweep {
                from: 60.0,
                to: 6000.0,
            },
            2.0,
        ),
        ("clicks", Signal::Clicks { period: 0.1 }, 1.0),
        ("impulse", Signal::Impulse, 0.3),
    ] {
        cases.push(Case {
            name,
            kind: Kind::Signal,
            seconds,
            notes: Vec::new(),
            sustain: (0.0, 0.0),
            signal: Some(signal),
            merge_hz: 0.0,
        });
    }
    cases
}

/// The case called `name`.
pub(super) fn case(name: &str) -> Case {
    all_cases()
        .into_iter()
        .find(|case| case.name == name)
        .unwrap_or_else(|| panic!("no case {name}"))
}

/// The attack length of every note.
const ATTACK_SECONDS: f64 = 0.003;
/// How long a stopped note takes to go silent.
const STOP_SECONDS: f64 = 0.001;

/// Renders `case` at `rate` with every partial at `ratio` times its
/// frequency (1 for the input, the shift for the reference).
pub(super) fn render(case: &Case, rate: u32, ratio: f64) -> Vec<f32> {
    let len = (case.seconds * rate as f64) as usize;
    let mut out = vec![0.0f64; len];
    let fs = rate as f64;
    if let Some(signal) = case.signal {
        match signal {
            Signal::Sweep { from, to } => {
                let seconds = case.seconds;
                let k = (to / from).ln() / seconds;
                for (i, s) in out.iter_mut().enumerate() {
                    let t = i as f64 / fs;
                    let phase = TAU * from * ratio * ((k * t).exp() - 1.0) / k;
                    let fade = (t / 0.01).min(1.0) * ((seconds - t) / 0.01).clamp(0.0, 1.0);
                    *s = 0.25 * fade * phase.sin();
                }
            }
            Signal::Clicks { period } => {
                let step = (period * fs) as usize;
                let mut i = step / 2;
                while i < len {
                    out[i] = 0.5;
                    i += step;
                }
            }
            Signal::Impulse => out[len / 4] = 0.5,
        }
        return out.into_iter().map(|s| s as f32).collect();
    }
    for note in &case.notes {
        let start = (note.onset * fs) as usize;
        let end = note
            .stop
            .map(|stop| ((stop + STOP_SECONDS) * fs) as usize)
            .unwrap_or(len)
            .min(len);
        let components = note.partials.iter().flat_map(|p| {
            let second = p.beat.map(|b| (p.freq + b.offset, b.amp, b.tau, b.phase));
            std::iter::once((p.freq, p.amp, p.tau, p.phase)).chain(second)
        });
        for (freq, amp, tau, phase) in components {
            let mut phase = phase;
            let freq = freq * ratio;
            if freq >= fs * 0.45 {
                continue;
            }
            for (i, s) in out[start..end].iter_mut().enumerate() {
                let t = i as f64 / fs;
                let attack = if t < ATTACK_SECONDS {
                    0.5 - 0.5 * (std::f64::consts::PI * t / ATTACK_SECONDS).cos()
                } else {
                    1.0
                };
                let release = match note.stop {
                    Some(stop) => {
                        let since = t + note.onset - stop;
                        if since <= 0.0 {
                            1.0
                        } else {
                            0.5 + 0.5
                                * (std::f64::consts::PI * since / STOP_SECONDS)
                                    .min(std::f64::consts::PI)
                                    .cos()
                        }
                    }
                    None => 1.0,
                };
                *s += amp * attack * release * (-t / tau).exp() * phase.sin();
                let f = freq * 2f64.powf(note.motion.semitones(t) / 12.0);
                phase += TAU * f / fs;
            }
        }
        // The pick: a short burst of filtered noise, the same in the input
        // and the reference.
        let mut rng = Lcg(note.seed ^ 0xA5A5_5A5A);
        let burst = (0.004 * fs) as usize;
        let mut low = 0.0;
        for i in 0..burst.min(end.saturating_sub(start)) {
            let t = i as f64 / fs;
            let white = rng.next() * 2.0 - 1.0;
            low += (white - low) * 0.3;
            out[start + i] += note.pick * low * (-t / 0.0012).exp();
        }
    }
    out.into_iter().map(|s| s as f32).collect()
}

/// The target partials of `case` at `ratio` with their amplitude at time
/// `t` (seconds), and the source partials (ratio 1).  Only still notes.  A
/// beating pair is one partial at its power-weighted centre, with the
/// pair's whole power; so are the partials of different notes within the
/// case's `merge_hz` (a unison).
pub(super) fn partials_at(case: &Case, ratio: f64, t: f64) -> Vec<(f64, f64)> {
    partials_spread_at(case, ratio, t)
        .into_iter()
        .map(|p| (p.freq, p.amp))
        .collect()
}

/// One partial as the scores see it: its power-weighted centre, its
/// amplitude (the root of its components' summed power), and its width: the
/// distance between its lowest and highest component, Hz (the fastest beat
/// it makes; 0 for a plain partial).
#[derive(Clone, Copy, Debug)]
pub(super) struct Merged {
    pub(super) freq: f64,
    pub(super) amp: f64,
    pub(super) width: f64,
}

/// Whether any partial of `case` beats.
pub(super) fn beats(case: &Case) -> bool {
    case.notes
        .iter()
        .any(|n| n.partials.iter().any(|p| p.beat.is_some()))
}

/// [`partials_at`] with each partial's width.
pub(super) fn partials_spread_at(case: &Case, ratio: f64, t: f64) -> Vec<Merged> {
    // Every component: (frequency, power, group).  A pair is its own group.
    let mut groups: Vec<Vec<(f64, f64)>> = Vec::new();
    for note in &case.notes {
        if t < note.onset || note.stop.is_some_and(|stop| t > stop) {
            continue;
        }
        let age = t - note.onset;
        for partial in &note.partials {
            let mut group = vec![(
                partial.freq * ratio,
                (partial.amp * (-age / partial.tau).exp()).powi(2),
            )];
            if let Some(beat) = partial.beat {
                group.push((
                    (partial.freq + beat.offset) * ratio,
                    (beat.amp * (-age / beat.tau).exp()).powi(2),
                ));
            }
            groups.push(group);
        }
    }
    let centre = |group: &[(f64, f64)]| {
        let power: f64 = group.iter().map(|c| c.1).sum();
        group.iter().map(|c| c.0 * c.1).sum::<f64>() / power.max(1e-300)
    };
    if case.merge_hz > 0.0 {
        groups.sort_by(|a, b| centre(a).total_cmp(&centre(b)));
        let mut merged: Vec<Vec<(f64, f64)>> = Vec::new();
        for group in groups {
            match merged.last_mut() {
                Some(last) if centre(&group) - centre(last) < case.merge_hz * ratio => {
                    last.extend(group)
                }
                _ => merged.push(group),
            }
        }
        groups = merged;
    }
    groups
        .iter()
        .map(|group| {
            let lowest = group.iter().map(|c| c.0).fold(f64::INFINITY, f64::min);
            let highest = group.iter().map(|c| c.0).fold(0.0, f64::max);
            Merged {
                freq: centre(group),
                amp: group.iter().map(|c| c.1).sum::<f64>().sqrt(),
                width: highest - lowest,
            }
        })
        .collect()
}
