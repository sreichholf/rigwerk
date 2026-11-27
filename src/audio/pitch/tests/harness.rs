//! The phase 0 harness (§4 of the plan), release only: with
//! `RIGWERK_PITCH_RENDER=<dir>` it renders every case at every shift,
//! writes the WAVs, prints the score table and the CPU score, and holds the
//! whole set to the gate.  Without it nothing runs and the
//! tests pass, like the plugin fixture tests.
//!
//! `RIGWERK_PITCH_DI=<dir>` adds the mono WAVs in that folder, rendered for
//! listening only.  `scripts/pitch-reference.sh <dir>` renders the inputs
//! through Rubber Band R3 into `<dir>/reference`; when those files are
//! there they are scored as well, as the best achievable.
//! `RIGWERK_PITCH_ONLY=<case,case>` limits the score table to those cases
//! and `RIGWERK_PITCH_SHIFTS=<-4,-12,…>` to those shifts, while tuning.
//!
//! ```bash
//! RIGWERK_PITCH_RENDER=.pitch-render \
//!   cargo test --locked --release audio::pitch -- --nocapture --test-threads 1
//! ```

use std::path::{Path, PathBuf};
use std::time::Instant;

use super::super::tight::spec;
use super::super::{PitchSettings, PitchShifter, DEFAULT_DELAY_MS};
use super::gate::{evaluate, failures, on, run, Engine, Row};
use super::material::{all_cases, render, Case, Kind};

/// The shifts of §4.1, and −4 (a common live setting).
const SHIFTS: [i8; 8] = [-1, -2, -4, -5, -7, -12, 5, 12];

/// Copied from `app/issues.rs` (`DSP_ERROR_SHARE`, `DSP_WARN_SHARE`):
/// `audio` does not import `app`.  The peak share that raises the error and
/// the smoothed share that raises the warning.
const DSP_ERROR_SHARE: f64 = 0.95;
const DSP_WARN_SHARE: f64 = 0.70;
/// The app's smoothing of the DSP time, per block (`mixer/path.rs`).
const DSP_SMOOTHING: f64 = 0.02;

fn render_dir() -> Option<PathBuf> {
    let dir = std::env::var_os("RIGWERK_PITCH_RENDER").filter(|dir| !dir.is_empty())?;
    let dir = PathBuf::from(dir);
    std::fs::create_dir_all(&dir).expect("create RIGWERK_PITCH_RENDER");
    Some(dir)
}

fn write_wav(path: &Path, samples: &[f32], rate: u32) {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(path, spec).expect("create wav");
    for &s in samples {
        writer.write_sample(s).expect("write wav");
    }
    writer.finalize().expect("finish wav");
}

/// A WAV as mono floats (channels averaged) and its rate.
fn read_wav(path: &Path) -> (Vec<f32>, u32) {
    let mut reader = hound::WavReader::open(path).expect("open wav");
    let spec = reader.spec();
    let channels = spec.channels.max(1) as usize;
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .map(|s| s.expect("sample"))
            .collect(),
        hound::SampleFormat::Int => {
            let scale = 1.0 / (1u64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.expect("sample") as f32 * scale)
                .collect()
        }
    };
    let mono = samples
        .chunks(channels)
        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
        .collect();
    (mono, spec.sample_rate)
}

/// A comma-separated filter from the environment: empty keeps everything.
fn filter(name: &str) -> Vec<String> {
    std::env::var(name)
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Whether `semitones` passes `RIGWERK_PITCH_SHIFTS` (0 always does).
fn shift_kept(semitones: i8) -> bool {
    let keep = filter("RIGWERK_PITCH_SHIFTS");
    semitones == 0
        || keep.is_empty()
        || keep.iter().any(|k| k.parse::<i8>().ok() == Some(semitones))
}

/// Renders and scores one case at every shift; the input, each output and
/// (when there) the Rubber Band reference are scored.
fn score_case(dir: &Path, case: &Case, rate: u32, shifts: &[i8], write: bool, rows: &mut Vec<Row>) {
    let input = render(case, rate, 1.0);
    if write {
        write_wav(&dir.join(format!("{}_input.wav", case.name)), &input, rate);
    }
    for &semitones in shifts.iter().filter(|&&s| shift_kept(s)) {
        let (out, latency) = run(&input, rate, semitones, 0, 256);
        let mut raw = vec![0.0; latency as usize];
        raw.extend_from_slice(&out);
        if write {
            let name = format!("{}_tight_{semitones:+}.wav", case.name);
            write_wav(&dir.join(name), &out, rate);
        }
        rows.push(evaluate(
            case,
            Engine::Tight,
            semitones,
            rate,
            &input,
            &out,
            Some((&raw, latency)),
        ));
        let reference = dir
            .join("reference")
            .join(format!("{}_{semitones:+}.wav", case.name));
        if rate == 48_000 && reference.is_file() {
            let (out, _) = read_wav(&reference);
            rows.push(evaluate(
                case,
                Engine::Reference,
                semitones,
                rate,
                &input,
                &out,
                Some((&out, 0)),
            ));
        }
    }
}

#[test]
fn phase_0_scores() {
    let Some(dir) = render_dir() else {
        return;
    };
    let mut rows = Vec::new();
    let only = std::env::var("RIGWERK_PITCH_ONLY").unwrap_or_default();
    let cases: Vec<Case> = all_cases()
        .into_iter()
        .filter(|c| only.is_empty() || only.split(',').any(|o| o == c.name))
        .collect();
    for case in &cases {
        let shifts: Vec<i8> = if case.kind == Kind::Signal {
            [0].into_iter().chain(SHIFTS).collect()
        } else {
            SHIFTS.to_vec()
        };
        score_case(&dir, case, 48_000, &shifts, true, &mut rows);
    }
    // The other rates, for a subset.
    for rate in [44_100, 96_000, 192_000] {
        for case in cases
            .iter()
            .filter(|c| ["e2", "emaj7", "chugs", "clicks"].contains(&c.name))
        {
            score_case(&dir, case, rate, &[0, -2, -12], false, &mut rows);
        }
    }
    println!("\n{}", Row::HEADER);
    let mut failed = Vec::new();
    for row in &rows {
        println!("{row}");
        // The cluster is a listening case: scored, never pass/fail.
        if row.kind != Kind::Listening && !failures(row).is_empty() {
            failed.push(format!(
                "{} {} {:+} @{}",
                row.case, row.engine, row.semitones, row.rate
            ));
        }
    }
    println!("\nlatency: {}", latency_line());
    println!("gate failures: {}", failed.len());
    for line in &failed {
        println!("  {line}");
    }
    assert!(
        failed.is_empty(),
        "{} rows fail the phase 0 gate",
        failed.len()
    );
}

/// The reported latency at the rates the plan names.
fn latency_line() -> String {
    let per_rate: Vec<String> = [44_100u32, 48_000, 96_000, 192_000]
        .iter()
        .map(|&rate| {
            let latency = spec(rate, DEFAULT_DELAY_MS).latency;
            format!(
                "{latency} ({:.1} ms @{rate})",
                latency as f64 * 1000.0 / rate as f64
            )
        })
        .collect();
    per_rate.join(", ")
}

/// The DI recordings, rendered for listening.
#[test]
fn phase_0_di_renders() {
    let Some(dir) = render_dir() else {
        return;
    };
    let Some(di) = std::env::var_os("RIGWERK_PITCH_DI").filter(|d| !d.is_empty()) else {
        return;
    };
    let out_dir = dir.join("di");
    std::fs::create_dir_all(&out_dir).expect("create di folder");
    let mut entries: Vec<PathBuf> = std::fs::read_dir(PathBuf::from(di))
        .expect("RIGWERK_PITCH_DI is set but cannot be read")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("wav")))
        .collect();
    entries.sort();
    for path in entries {
        let (input, rate) = read_wav(&path);
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("di");
        for semitones in SHIFTS {
            let (out, _) = run(&input, rate, semitones, 0, 256);
            let name = format!("{stem}_tight_{semitones:+}.wav");
            write_wav(&out_dir.join(name), &out, rate);
        }
        println!("rendered {}", path.display());
    }
}

/// The CPU score of §4.2 (9): the stage on four staggered paths, at
/// 64- and 4096-frame blocks, steady and over a run that switches it off
/// and on; and one path at 48 and 192 kHz.  Wall-clock time on the machine
/// that runs it: read it with the per-hop cost next to it.
#[test]
fn phase_0_cpu() {
    if render_dir().is_none() {
        return;
    }
    let case = super::material::case("strum");
    let looped = |rate: u32, seconds: usize| -> Vec<f32> {
        render(&case, rate, 1.0)
            .into_iter()
            .cycle()
            .take(rate as usize * seconds)
            .collect()
    };
    for rate in [48_000, 192_000] {
        let ns = ns_per_sample(&looped(rate, 6), rate);
        let hop = spec(rate, DEFAULT_DELAY_MS).hop as f64;
        println!(
            "cpu: one path at {rate} Hz: {ns:.0} ns per sample, {:.0} µs per hop of \
             {hop} samples ({:.0} µs), {:.1} % of real time",
            ns * hop / 1000.0,
            hop * 1e6 / rate as f64,
            ns * rate as f64 / 1e7
        );
    }
    let mut over = Vec::new();
    let input = looped(48_000, 12);
    // (paths, whether it switches off and back on, rate, block)
    let runs = [
        (1, false, 48_000, 64),
        (4, false, 48_000, 64),
        (4, true, 48_000, 64),
        (4, true, 48_000, 4096),
        (4, false, 192_000, 256),
    ];
    for (count, switch, rate, block) in runs {
        let input = if rate == 48_000 {
            input.clone()
        } else {
            looped(rate, 6)
        };
        let run = paths(&input, rate, block, count, switch);
        let fits = run.worst < DSP_ERROR_SHARE && run.smoothed < DSP_WARN_SHARE;
        let block_us = block as f64 * 1e6 / rate as f64;
        println!(
            "cpu: {count} path(s) at {rate} Hz ({}), {block}-frame blocks \
             ({block_us:.0} µs): worst {:.1} % ({:.0} µs; {:.1} % in one run as measured), \
             99.9th percentile {:.1} %, smoothed {:.1} %, {:.0} ns per sample  {}",
            if switch { "off and on" } else { "steady" },
            run.worst * 100.0,
            run.worst * block_us,
            run.raw_worst * 100.0,
            run.p999 * 100.0,
            run.smoothed * 100.0,
            run.ns,
            if fits { "ok" } else { "OVER" }
        );
        // The gate: 48 kHz.  The rest is reported.
        if rate == 48_000 && !fits {
            over.push(format!("{count} path(s) at {block}-frame blocks"));
        }
    }
    assert!(over.is_empty(), "over the CPU budget: {over:?}");
}

/// One path's average cost, ns per sample.
fn ns_per_sample(input: &[f32], rate: u32) -> f64 {
    let mut shifter = PitchShifter::new(rate, 0);
    shifter.update(on(-5), rate);
    let mut buf = vec![[0.0f32; 4]; 256];
    let start = Instant::now();
    for chunk in input.chunks(256) {
        for (frame, &x) in buf.iter_mut().zip(chunk) {
            *frame = [x, x, 0.0, 0.0];
        }
        shifter.begin_block();
        shifter.process(&mut buf, chunk.len());
    }
    start.elapsed().as_nanos() as f64 / input.len() as f64
}

/// One run's figures.
struct CpuRun {
    /// The worst block's share of its duration (each block's fastest of
    /// three runs, which takes the machine's own stalls out: the work per
    /// block is the same every run), the same over one run as measured,
    /// and the 99.9th percentile of the first.
    worst: f64,
    raw_worst: f64,
    p999: f64,
    /// The largest smoothed share once it has settled.
    smoothed: f64,
    /// The cost per sample of all four paths, ns.
    ns: f64,
}

/// How often a run repeats.
const CPU_RUNS: usize = 3;

/// `count` staggered paths on, switched off a third of the way through and
/// on again at two thirds when `switch`.  The first second is left out of
/// the worst block: it warms the caches.
fn paths(input: &[f32], rate: u32, block: usize, count: usize, switch: bool) -> CpuRun {
    let blocks = input.len() / block;
    let mut fastest = vec![f64::INFINITY; blocks];
    let mut raw_worst = 0.0f64;
    let block_ns = block as f64 * 1e9 / rate as f64;
    let warm = rate as usize / block;
    for run in 0..CPU_RUNS {
        let mut paths: Vec<PitchShifter> = (0..count)
            .map(|slot| PitchShifter::new(rate, slot))
            .collect();
        for path in &mut paths {
            path.update(on(-5), rate);
        }
        let mut buf = vec![[0.0f32; 4]; block];
        for (index, best) in fastest.iter_mut().enumerate() {
            if switch && (index == blocks / 3 || index == 2 * blocks / 3) {
                let settings = PitchSettings {
                    enabled: index != blocks / 3,
                    ..on(-5)
                };
                for path in &mut paths {
                    path.update(settings, rate);
                }
            }
            let chunk = &input[index * block..(index + 1) * block];
            let start = Instant::now();
            for path in &mut paths {
                for (frame, &x) in buf.iter_mut().zip(chunk) {
                    *frame = [x, x, 0.0, 0.0];
                }
                path.begin_block();
                path.process(&mut buf, block);
            }
            let ns = start.elapsed().as_nanos() as f64;
            *best = best.min(ns);
            if run == 0 && index > warm {
                raw_worst = raw_worst.max(ns / block_ns);
            }
        }
    }
    let mut smoothed = 0.0f64;
    let mut out = CpuRun {
        worst: 0.0,
        raw_worst,
        p999: 0.0,
        smoothed: 0.0,
        ns: fastest.iter().sum::<f64>() / (blocks * block) as f64,
    };
    let mut shares = Vec::with_capacity(blocks);
    for (index, ns) in fastest.iter().enumerate() {
        let share = ns / block_ns;
        smoothed = if smoothed == 0.0 {
            share
        } else {
            smoothed + (share - smoothed) * DSP_SMOOTHING
        };
        if index > warm {
            out.worst = out.worst.max(share);
            out.smoothed = out.smoothed.max(smoothed);
            shares.push(share);
        }
    }
    shares.sort_by(f64::total_cmp);
    if !shares.is_empty() {
        out.p999 = shares[((shares.len() - 1) as f64 * 0.999) as usize];
    }
    out
}
