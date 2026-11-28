use super::{apply_tail_fade, CAB_MIN_HIGH_CUT_HZ};
use super::{
    cab_note, load_cab_channels, prepare_cab_file, prepare_cab_samples, scan_cab_folder, CabFile,
    CabKernel, CabNote, CabSettings, CabStage,
};
use crate::audio::path::PathControls;
use crate::audio::{CabControls, CAB_MAX_MS, CAB_MAX_TAPS};

fn controls(enabled: bool) -> PathControls {
    let controls = PathControls::new(48_000);
    controls.set_cab(CabControls {
        enabled,
        ..CabControls::default()
    });
    controls
}

fn kernel(taps: &[f32]) -> CabKernel {
    CabKernel::from_taps(taps, taps, 48_000)
}

/// Runs `blocks` blocks of `frames` constant frames and returns the last
/// block, left then right.
fn run(
    stage: &mut CabStage,
    controls: &PathControls,
    blocks: usize,
    frames: usize,
    left: f32,
    right: f32,
) -> Vec<[f32; 2]> {
    stage.reserve(frames);
    let mut last = Vec::new();
    for _ in 0..blocks {
        stage.begin(controls, 48_000, 64.0);
        let mut block = vec![[left, right, 0.0, 0.0]; frames];
        stage.process_block(&mut block, frames);
        last = block.iter().map(|frame| [frame[0], frame[1]]).collect();
    }
    last
}

fn ready(file: CabFile) -> CabKernel {
    match file {
        CabFile::Ready(kernel) => kernel,
        CabFile::Failed(error) => panic!("expected a kernel, got {error}"),
    }
}

#[test]
fn a_unit_kernel_passes_samples_through_once_enable_has_snapped() {
    let controls = controls(true);
    let mut stage = CabStage::new(48_000);
    stage.install_prepared(kernel(&[1.0]));
    let mut block = vec![[0.0; 4]; 8];
    for (index, frame) in block.iter_mut().enumerate() {
        frame[0] = index as f32 * 0.05;
        frame[1] = -0.2 - index as f32 * 0.01;
    }
    stage.begin(&controls, 48_000, 64.0);
    stage.process_block(&mut block, 8);
    for (index, frame) in block.iter().enumerate() {
        assert!(
            (frame[0] - index as f32 * 0.05).abs() < 1.0e-6,
            "left {}",
            frame[0]
        );
        assert!(
            (frame[1] + 0.2 + index as f32 * 0.01).abs() < 1.0e-6,
            "right {}",
            frame[1]
        );
    }
}

#[test]
fn a_delayed_impulse_is_the_kernel_not_extra_latency() {
    let controls = controls(true);
    let mut stage = CabStage::new(48_000);
    stage.install_prepared(CabKernel::from_taps(&[0.0, 1.0], &[1.0], 48_000));
    stage.begin(&controls, 48_000, 64.0);
    let mut block = [[0.0; 4]; 4];
    block[0][0] = 0.5;
    block[0][1] = -0.25;
    stage.process_block(&mut block, 4);
    assert_eq!(block[0][0], 0.0, "the impulse itself is one sample late");
    assert!((block[1][0] - 0.5).abs() < 1.0e-6);
    assert_eq!(block[2][0], 0.0);
    assert!(
        (block[0][1] + 0.25).abs() < 1.0e-6,
        "the right kernel is not the left one"
    );
}

#[test]
fn a_kernel_built_for_another_rate_stays_out_of_the_signal() {
    let controls = controls(true);
    let mut stage = CabStage::new(48_000);
    stage.install_prepared(kernel(&[0.25]));
    stage.begin(&controls, 48_000, 64.0);
    // The open rate moved.  The taps are for 48 kHz, so the block stays dry
    // and the old history is not waiting for the next kernel.
    stage.begin(&controls, 96_000, 64.0);
    let mut block = vec![[0.4, -0.2, 0.0, 0.0]; 8];
    stage.process_block(&mut block, 8);
    for frame in &block {
        assert!((frame[0] - 0.4).abs() < 1.0e-6, "left {}", frame[0]);
        assert!((frame[1] + 0.2).abs() < 1.0e-6, "right {}", frame[1]);
    }
}

#[test]
fn bypass_once_the_fade_has_finished_returns_the_dry_input() {
    let controls = controls(true);
    let mut stage = CabStage::new(48_000);
    stage.install_prepared(kernel(&[0.5]));
    let wet = run(&mut stage, &controls, 1, 64, 0.4, -0.2);
    assert!(
        (wet[63][0] - 0.2).abs() < 1.0e-5,
        "the kernel is in the path"
    );
    controls.set_cab(CabControls {
        enabled: false,
        ..CabControls::default()
    });
    // 5 ms at 48 kHz is 240 samples.  Eight blocks of 64 finish the fade.
    let dry = run(&mut stage, &controls, 8, 64, 0.4, -0.2);
    assert!((dry[0][0] - 0.4).abs() < 1.0e-5);
    assert!((dry[0][1] + 0.2).abs() < 1.0e-5);
    assert!((dry[63][0] - 0.4).abs() < 1.0e-5);
}

#[test]
fn a_mono_impulse_loads_at_its_own_rate_with_unit_energy() {
    let dir = std::env::temp_dir().join(format!("rigwerk-cab-impulse-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp folder");
    let path = dir.join("impulse.wav");
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 48_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(&path, spec).expect("wav");
    writer.write_sample(0i16).expect("lead");
    writer.write_sample(16_384i16).expect("impulse");
    writer.finalize().expect("close");
    let kernel = ready(prepare_cab_file(
        path.to_str().expect("utf8"),
        48_000,
        0.0,
        0.0,
        false,
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let energy: f32 = kernel
        .left
        .iter()
        .chain(kernel.right.iter())
        .map(|sample| sample * sample)
        .sum();
    assert!((energy - 1.0).abs() < 1.0e-3, "{energy}");
    let first = kernel
        .left
        .iter()
        .position(|sample| sample.abs() >= 1.0e-5)
        .expect("a tap");
    assert_eq!(first, 0, "the dead preamble is trimmed");
    // One sample, stored on both channels: each carries half the energy.
    assert!((kernel.left[first] - 0.5f32.sqrt()).abs() < 1.0e-3);
    assert_eq!(kernel.left, kernel.right, "a mono file is stored twice");
    assert_eq!(kernel.rate, 48_000);
}

#[test]
fn a_file_longer_than_50_ms_truncates_to_rate_over_20() {
    let dir = std::env::temp_dir().join(format!("rigwerk-cab-long-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp folder");
    let path = dir.join("long.wav");
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 48_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(&path, spec).expect("wav");
    for _ in 0..6_000 {
        writer.write_sample(8_192i16).expect("sample");
    }
    writer.finalize().expect("close");
    let kernel = ready(prepare_cab_file(
        path.to_str().expect("utf8"),
        48_000,
        0.0,
        0.0,
        false,
    ));
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(kernel.left.len(), 48_000 / 20);
    assert!(kernel.left.len() <= CAB_MAX_TAPS);
}

#[test]
fn resampling_to_twice_the_file_rate_changes_the_length() {
    let dir = std::env::temp_dir().join(format!("rigwerk-cab-rate-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp folder");
    let path = dir.join("slow.wav");
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 24_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(&path, spec).expect("wav");
    for _ in 0..100 {
        writer.write_sample(8_192i16).expect("sample");
    }
    writer.finalize().expect("close");
    let file = path.to_str().expect("utf8");
    let (left, _) = load_cab_channels(file, 48_000).expect("resampled");
    let kernel = ready(prepare_cab_file(file, 48_000, 0.0, 0.0, false));
    let _ = std::fs::remove_dir_all(&dir);
    assert_ne!(left.len(), 100, "2× resampling changes the length");
    assert!(left.len() > 100);
    assert_eq!(kernel.rate, 48_000);
    assert_ne!(kernel.left.len(), 100);
}

#[test]
fn a_low_cut_on_a_dc_heavy_kernel_drives_the_sum_near_zero() {
    let kernel = ready(prepare_cab_samples(
        &vec![1.0; 64],
        &vec![1.0; 64],
        48_000,
        120.0,
        0.0,
        false,
    ));
    let sum: f32 = kernel.left.iter().sum();
    assert!(
        sum.abs() < 1.0e-2,
        "a high-pass removes DC, sum {sum} over {} taps",
        kernel.left.len()
    );
}

#[test]
fn cab_note_follows_the_gear_type_and_whether_the_ir_is_running() {
    let cases = [
        (Some("amp"), false, Some(CabNote::NoCabinet)),
        (Some(" pedal "), false, None),
        (Some("amp_cab"), true, Some(CabNote::TwoCabinets)),
        (Some("full-rig"), true, Some(CabNote::TwoCabinets)),
        (Some("amp"), true, None),
        (None, false, None),
        (None, true, None),
        (Some("preamp"), false, Some(CabNote::NoCabinet)),
        (Some("pedal_amp"), false, Some(CabNote::NoCabinet)),
        (Some("pedal_amp_cab"), true, Some(CabNote::TwoCabinets)),
        (Some("ir"), true, Some(CabNote::TwoCabinets)),
        (Some("outboard"), false, None),
        (Some("space"), true, None),
        (Some("studio"), false, None),
    ];
    for (gear, active, note) in cases {
        assert_eq!(cab_note(gear, active), note, "{gear:?} active={active}");
    }
}

#[test]
fn above_192_khz_the_kernel_stops_at_the_tap_cap() {
    assert_eq!(super::max_taps(192_000), CAB_MAX_TAPS);
    assert_eq!(super::max_taps(384_000), CAB_MAX_TAPS);
    assert!(super::max_taps(384_000) < ((384_000.0 * CAB_MAX_MS / 1000.0) as usize));
}

#[test]
fn settings_sanitize_and_an_unset_cabinet_is_the_default() {
    assert_eq!(CAB_MAX_MS, 50.0);
    assert_eq!(CAB_MIN_HIGH_CUT_HZ, 1_000.0);
    assert!(!CabControls::default().enabled);
    assert!(CabSettings::default().is_unset());
    let cleaned = CabSettings {
        enabled: true,
        level_db: f32::NAN,
        polarity: true,
        low_cut_hz: 900.0,
        high_cut_hz: 40.0,
        file: "  cab.wav  ".to_owned(),
    }
    .sanitized();
    assert!(cleaned.enabled);
    assert!(cleaned.polarity);
    assert_eq!(cleaned.level_db, 0.0);
    assert_eq!(cleaned.low_cut_hz, 500.0);
    assert_eq!(cleaned.high_cut_hz, 1_000.0);
    assert_eq!(cleaned.file, "cab.wav");
    assert!(!cleaned.is_unset());
    assert_eq!(cleaned.controls().low_cut_hz, 500.0);

    let blank = CabSettings {
        file: "   ".to_owned(),
        ..CabSettings::default()
    }
    .sanitized();
    assert!(blank.file.is_empty());
    assert!(blank.is_unset());

    let off = CabControls {
        high_cut_hz: 0.0,
        low_cut_hz: f32::NAN,
        ..CabControls::default()
    }
    .sanitized();
    assert_eq!(off.high_cut_hz, 0.0);
    assert_eq!(off.low_cut_hz, 0.0);
}

#[test]
fn a_missing_file_and_a_text_file_fail_with_the_short_sentences() {
    match prepare_cab_file("/no/such/cabinet.wav", 48_000, 0.0, 0.0, false) {
        CabFile::Failed(error) => assert_eq!(error, "Couldn't read the impulse response."),
        CabFile::Ready(_) => panic!("a missing file is not a kernel"),
    }
    let dir = std::env::temp_dir().join(format!("rigwerk-cab-text-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp folder");
    let path = dir.join("notes.wav");
    std::fs::write(&path, b"not a wav").expect("text");
    match prepare_cab_file(path.to_str().expect("utf8"), 48_000, 0.0, 0.0, false) {
        CabFile::Failed(error) => assert_eq!(error, "This file is not a WAV."),
        CabFile::Ready(_) => panic!("text is not a kernel"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_kernel_queued_after_enable_fades_in() {
    let controls = controls(true);
    let mut stage = CabStage::new(48_000);
    // On, with no file yet.  The wet target stays down.
    let dry = run(&mut stage, &controls, 1, 64, 0.4, -0.2);
    assert!((dry[0][0] - 0.4).abs() < 1.0e-6);
    controls.queue_cab(kernel(&[0.5]));
    let block = run(&mut stage, &controls, 1, 64, 0.4, -0.2);
    assert!(
        (block[0][0] - 0.4).abs() > 1.0e-6,
        "the arriving kernel has to start the wet fade, got {}",
        block[0][0]
    );
}

#[test]
fn a_clear_then_an_install_does_not_play_the_old_history() {
    let controls = controls(true);
    let mut stage = CabStage::new(48_000);
    // Past the 5 ms fade, which still writes history while the old kernel runs.
    let mut taps = vec![0.0; 400];
    taps[399] = 1.0;
    controls.queue_cab(kernel(&taps));
    let _ = run(&mut stage, &controls, 8, 64, 0.8, 0.8);
    controls.clear_cab();
    let _ = run(&mut stage, &controls, 6, 64, 0.0, 0.0);
    controls.queue_cab(kernel(&taps));
    let block = run(&mut stage, &controls, 1, 64, 0.0, 0.0);
    assert!(
        block[0][0].abs() < 1.0e-5,
        "stale history leaked into the new kernel: {}",
        block[0][0]
    );
}

#[test]
fn a_kernel_that_cannot_be_parked_is_not_dropped_for_the_next_install() {
    let controls = controls(true);
    let mut stage = CabStage::new(48_000);
    controls.queue_cab(kernel(&[1.0]));
    let _ = run(&mut stage, &controls, 1, 8, 0.0, 0.0);
    // The park holds 16 and the stage holds 2.  One more outgoing has
    // nowhere to go, so the Install after that has to wait.
    for _ in 0..19 {
        controls.queue_cab(kernel(&[1.0]));
        let _ = run(&mut stage, &controls, 5, 64, 0.0, 0.0);
    }
    controls.queue_cab(kernel(&[0.25]));
    stage.begin(&controls, 48_000, 64.0);
    let waiting = controls.pending_cab_mut().expect("lock").is_some();
    assert!(
        waiting,
        "the callback accepted an Install while a kernel was still outgoing"
    );
}

#[test]
fn leading_silence_is_trimmed_before_the_tail_is_cut() {
    let mut samples = vec![0.0; 480];
    samples.extend(std::iter::repeat_n(0.5, 3_000));
    samples[480] = 1.0;
    let kernel = ready(prepare_cab_samples(&samples, &[], 48_000, 0.0, 0.0, false));
    assert_eq!(kernel.left.len(), 2_400);
    assert!(kernel.left[0] > kernel.left[100]);
    assert!(kernel.left[0] > 0.0);
}

#[test]
fn the_decode_window_keeps_the_tail_past_a_dead_preamble() {
    let dir = std::env::temp_dir().join(format!("rigwerk-cab-lead-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp folder");
    let path = dir.join("lead.wav");
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 48_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(&path, spec).expect("wav");
    for _ in 0..480 {
        writer.write_sample(0i16).expect("silence");
    }
    for _ in 0..2_400 {
        writer.write_sample(8_192i16).expect("body");
    }
    writer.finalize().expect("close");
    let kernel = ready(prepare_cab_file(
        path.to_str().expect("utf8"),
        48_000,
        0.0,
        0.0,
        false,
    ));
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(kernel.left.len(), 2_400, "the dead preamble spent the tail");
    assert!((kernel.left[0] - kernel.left[1]).abs() < 1.0e-4);
    assert!(kernel.left[0] > 0.0);
}

#[test]
fn a_folder_scan_lists_wav_files_and_does_not_walk_down() {
    let dir = std::env::temp_dir().join(format!("rigwerk-cab-scan-{}", std::process::id()));
    let nested = dir.join("nested");
    std::fs::create_dir_all(&nested).expect("temp folder");
    std::fs::write(dir.join("Zebra.WAV"), b"x").expect("wav");
    std::fs::write(dir.join("alpha.wav"), b"x").expect("wav");
    std::fs::write(dir.join("skip.nam"), b"x").expect("nam");
    std::fs::write(nested.join("hidden.wav"), b"x").expect("nested");
    let entries = scan_cab_folder(&dir.to_string_lossy());
    let _ = std::fs::remove_dir_all(&dir);
    let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
    assert_eq!(names, ["alpha", "Zebra"]);
}

#[test]
fn resampling_a_sine_keeps_the_passband_and_rejects_images() {
    // 44.1 → 48 and 48 → 96.  A 1 kHz sine should come through at its own
    // level, and everything that is not that sine (the images) stays under
    // the window's stopband.
    check_resampled_sine(44_100, 48_000);
    check_resampled_sine(48_000, 96_000);
}

fn check_resampled_sine(src: u32, dst: u32) {
    let hz = 1_000.0;
    let count = src as usize / 4;
    let input: Vec<f32> = (0..count)
        .map(|index| (2.0 * std::f64::consts::PI * hz * index as f64 / f64::from(src)).sin() as f32)
        .collect();
    let output = super::resample(&input, src, dst);
    let skip = super::SINC_TAPS * 2;
    assert!(output.len() > skip * 2 + 1_000);
    let body = &output[skip..output.len() - skip];
    let mut ss = 0.0f64;
    let mut cc = 0.0f64;
    let mut sc = 0.0f64;
    let mut ys = 0.0f64;
    let mut yc = 0.0f64;
    for (index, sample) in body.iter().enumerate() {
        let phase = 2.0 * std::f64::consts::PI * hz * index as f64 / f64::from(dst);
        let sine = phase.sin();
        let cosine = phase.cos();
        ss += sine * sine;
        cc += cosine * cosine;
        sc += sine * cosine;
        ys += f64::from(*sample) * sine;
        yc += f64::from(*sample) * cosine;
    }
    let det = ss * cc - sc * sc;
    let amplitude = ((ys * cc - yc * sc) / det).hypot((yc * ss - ys * sc) / det);
    let gain_db = 20.0 * amplitude.log10();
    assert!(
        gain_db.abs() < 0.05,
        "{src} → {dst}: passband {gain_db:.3} dB"
    );
    let mut error = 0.0f64;
    let mut energy = 0.0f64;
    for (index, sample) in body.iter().enumerate() {
        let phase = 2.0 * std::f64::consts::PI * hz * index as f64 / f64::from(dst);
        let sine = phase.sin();
        let cosine = phase.cos();
        let fit = (ys * cc - yc * sc) / det * sine + (yc * ss - ys * sc) / det * cosine;
        let residual = f64::from(*sample) - fit;
        error += residual * residual;
        energy += f64::from(*sample) * f64::from(*sample);
    }
    let floor_db = 10.0 * (error / energy).log10();
    assert!(
        floor_db < -100.0,
        "{src} → {dst}: image floor {floor_db:.1} dB"
    );
}

fn noise_level_db(kernel: &[f32], drive: &[f32]) -> f32 {
    let output = reference(drive, kernel);
    let energy: f32 = output.iter().map(|sample| sample * sample).sum();
    10.0 * (energy / output.len() as f32).log10()
}

#[test]
fn the_same_impulse_keeps_its_level_across_sample_rates() {
    let drive = noise(3, 8_000);
    let low = decay(44_100);
    let high = decay(96_000);
    let low = ready(prepare_cab_samples(&low, &[], 44_100, 0.0, 0.0, false));
    let high = ready(prepare_cab_samples(&high, &[], 96_000, 0.0, 0.0, false));
    let low_db = noise_level_db(&low.left, &drive);
    let high_db = noise_level_db(&high.left, &drive);
    assert!(
        (low_db - high_db).abs() < 0.5,
        "{low_db:.2} dB at 44.1 kHz, {high_db:.2} dB at 96 kHz"
    );
}

#[test]
fn impulses_with_different_crest_factors_come_out_at_a_similar_level() {
    let drive = noise(8, 8_000);
    // Same length, so the convolution edge is the same.  The spike's
    // peak-to-energy ratio is the whole buffer; the flat one's is 1/64.
    let mut spike = vec![0.0; 64];
    spike[0] = 1.0;
    let flat = vec![1.0; 64];
    let spike = ready(prepare_cab_samples(&spike, &[], 48_000, 0.0, 0.0, false));
    let flat = ready(prepare_cab_samples(&flat, &[], 48_000, 0.0, 0.0, false));
    let spike_db = noise_level_db(&spike.left, &drive);
    let flat_db = noise_level_db(&flat.left, &drive);
    assert!(
        (spike_db - flat_db).abs() < 0.5,
        "spike {spike_db:.2} dB, flat {flat_db:.2} dB"
    );
}

#[test]
fn a_near_silent_impulse_is_not_boosted() {
    match prepare_cab_samples(&vec![1.0e-8; 400], &[], 48_000, 0.0, 0.0, false) {
        CabFile::Failed(_) => {}
        CabFile::Ready(_) => panic!("a silent impulse was boosted into a kernel"),
    }
}

fn decay(rate: u32) -> Vec<f32> {
    let tau = 0.006 * rate as f32;
    let count = ((rate as f32) * 0.04) as usize;
    (0..count)
        .map(|index| (-(index as f32) / tau).exp())
        .collect()
}

#[test]
fn the_tail_fades_out_and_a_silent_tail_is_left_alone() {
    let mut body = vec![1.0; 2_000];
    apply_tail_fade(&mut body, 48_000);
    assert!(
        body.last().unwrap().abs() < 1.0e-6,
        "{}",
        body.last().unwrap()
    );
    assert!((body[0] - 1.0).abs() < 1.0e-6);
    let kernel = ready(prepare_cab_samples(
        &vec![0.4; 3_000],
        &[],
        48_000,
        0.0,
        0.0,
        false,
    ));
    assert!(kernel.left.last().unwrap().abs() < 1.0e-5);

    let mut silent = vec![0.0; 800];
    silent[0] = 0.8;
    silent[12] = -0.3;
    let untouched = silent.clone();
    apply_tail_fade(&mut silent, 48_000);
    assert_eq!(silent, untouched);
}

fn reference(input: &[f32], taps: &[f32]) -> Vec<f32> {
    let mut output = vec![0.0; input.len()];
    for index in 0..input.len() {
        let mut acc = 0.0;
        for (tap, coeff) in taps.iter().enumerate() {
            if index >= tap {
                acc += coeff * input[index - tap];
            }
        }
        output[index] = acc;
    }
    output
}

fn assert_close(got: &[f32], expect: &[f32]) {
    let scale = expect
        .iter()
        .fold(0.0f32, |peak, sample| peak.max(sample.abs()))
        .max(1.0e-3);
    let mut worst = 0.0f32;
    for (sample, want) in got.iter().zip(expect) {
        worst = worst.max((sample - want).abs());
    }
    assert!(
        worst / scale < 1.0e-5,
        "relative error {} (abs {worst}, scale {scale})",
        worst / scale
    );
}

/// Feeds `input` in blocks of `block` and returns the wet left channel.
fn play(
    stage: &mut CabStage,
    controls: &PathControls,
    input: &[[f32; 2]],
    block: usize,
) -> Vec<f32> {
    let mut output = Vec::with_capacity(input.len());
    let mut offset = 0;
    while offset < input.len() {
        let count = block.min(input.len() - offset);
        stage.begin(controls, 48_000, 64.0);
        let mut frames = vec![[0.0; 4]; count];
        for (frame, sample) in frames.iter_mut().zip(&input[offset..offset + count]) {
            frame[0] = sample[0];
            frame[1] = sample[1];
        }
        stage.process_block(&mut frames, count);
        output.extend(frames.iter().map(|frame| frame[0]));
        offset += count;
    }
    output
}

fn noise(seed: u32, length: usize) -> Vec<f32> {
    let mut state = seed.max(1);
    let mut samples = Vec::with_capacity(length);
    for _ in 0..length {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let unit = (state >> 8) as f32 / (1u32 << 24) as f32;
        samples.push(unit * 2.0 - 1.0);
    }
    samples
}

#[test]
fn partitioned_convolution_matches_a_direct_sum() {
    let controls = controls(true);
    let lengths = [
        1usize,
        17,
        127,
        128,
        129,
        255,
        511,
        1_000,
        2_400,
        CAB_MAX_TAPS,
    ];
    let blocks = [1usize, 7, 64, 128, 200, 513];
    for taps in lengths {
        let impulse = noise(taps as u32 + 3, taps);
        for block in blocks {
            // A full-length IR only needs a few block sizes.  The shorter
            // ones cover the grid.
            if taps > 2_400 && block != 1 && block != 7 && block != 513 {
                continue;
            }
            let frames = if taps > 2_400 { 1_500 } else { 600 };
            let input_l = noise(11, frames);
            let input_r = noise(29, frames);
            let input: Vec<[f32; 2]> = input_l
                .iter()
                .zip(&input_r)
                .map(|(left, right)| [*left, *right])
                .collect();
            let mut stage = CabStage::new(48_000);
            stage.install_prepared(kernel(&impulse));
            let got = play(&mut stage, &controls, &input, block);
            let expect = reference(&input_l, &impulse);
            assert_close(&got, &expect);
        }
    }
}

#[test]
fn the_two_channels_convolve_their_own_taps() {
    let controls = controls(true);
    let left_ir = noise(5, 400);
    let right_ir = noise(6, 180);
    let frames = 500;
    let input_l = noise(31, frames);
    let input_r = noise(32, frames);
    let input: Vec<[f32; 2]> = input_l
        .iter()
        .zip(&input_r)
        .map(|(left, right)| [*left, *right])
        .collect();
    let mut stage = CabStage::new(48_000);
    stage.install_prepared(CabKernel::from_taps(&left_ir, &right_ir, 48_000));
    let mut got_l = Vec::new();
    let mut got_r = Vec::new();
    let mut offset = 0;
    let block = 13;
    while offset < input.len() {
        let count = block.min(input.len() - offset);
        stage.begin(&controls, 48_000, 64.0);
        let mut frames = vec![[0.0; 4]; count];
        for (frame, sample) in frames.iter_mut().zip(&input[offset..offset + count]) {
            *frame = [sample[0], sample[1], 0.0, 0.0];
        }
        stage.process_block(&mut frames, count);
        got_l.extend(frames.iter().map(|frame| frame[0]));
        got_r.extend(frames.iter().map(|frame| frame[1]));
        offset += count;
    }
    assert_close(&got_l, &reference(&input_l, &left_ir));
    assert_close(&got_r, &reference(&input_r, &right_ir));
}

#[test]
fn a_swap_mid_stream_crossfades_onto_the_new_impulse() {
    let controls = controls(true);
    let first = noise(4, 300);
    let second = noise(9, 900);
    let frames = 700;
    let input_l = noise(21, frames);
    let input: Vec<[f32; 2]> = input_l.iter().map(|sample| [*sample, *sample]).collect();
    let mut stage = CabStage::new(48_000);
    stage.install_prepared(kernel(&first));
    // Past the first 128-sample partition, so the outgoing tail is already
    // computed and has to move with the kernel it belongs to.
    let lead = 200;
    let before = play(&mut stage, &controls, &input[..lead], 7);
    controls.queue_cab(kernel(&second));
    let after = play(&mut stage, &controls, &input[lead..], 13);
    let mut got = before;
    got.extend(after);
    let old = reference(&input_l, &first);
    let new = reference(&input_l, &second);
    // 5 ms at 48 kHz.  The ramp's first step leaves zero on the sample
    // it is applied to: `next` advances, then returns.
    let fade = 0.005 * 48_000.0;
    let step = 1.0 / fade;
    let mut blend = 0.0;
    for index in lead..frames {
        let next = blend + step;
        blend = if next >= 1.0 { 1.0 } else { next };
        let want = old[index] + (new[index] - old[index]) * blend;
        let scale = old[index].abs().max(new[index].abs()).max(1.0e-3);
        assert!(
            (got[index] - want).abs() / scale < 1.0e-5,
            "sample {index}: got {} want {want}",
            got[index]
        );
    }
}
