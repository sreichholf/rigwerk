use super::{
    alsa_pcm_is_default, alsa_pcm_is_direct, alsa_pcm_is_placeholder, smallest_output_buffer,
};
use super::{describe, requested_output_buffer};
use crate::audio::types::OutputMode;

fn config(min: u32, max: u32) -> cpal::SupportedStreamConfig {
    cpal::SupportedStreamConfig::new(
        2,
        48_000,
        cpal::SupportedBufferSize::Range { min, max },
        cpal::SampleFormat::F32,
    )
}

#[test]
fn direct_alsa_pcms_are_recognised() {
    assert!(alsa_pcm_is_direct("ALSA:hw:CARD=0,DEV=0"));
    assert!(alsa_pcm_is_direct("ALSA:plughw:CARD=1,DEV=0"));
    assert!(alsa_pcm_is_direct("alsa:hw:CARD=0,DEV=0"));
    assert!(alsa_pcm_is_direct("alsa:plughw:CARD=1,DEV=0"));
    assert!(alsa_pcm_is_direct("hw:CARD=0,DEV=0"));
}

#[test]
fn virtual_alsa_pcms_are_not_direct() {
    for id in [
        "ALSA:default",
        "ALSA:pipewire",
        "ALSA:pulse",
        "ALSA:sysdefault:CARD=0",
    ] {
        assert!(!alsa_pcm_is_direct(id), "{id}");
    }
    assert!(alsa_pcm_is_default("ALSA:default"));
    assert!(alsa_pcm_is_default("default"));
    assert!(!alsa_pcm_is_default("ALSA:hw:CARD=0,DEV=0"));
}

#[test]
fn sink_plugins_that_cannot_carry_audio_are_skipped() {
    assert!(alsa_pcm_is_placeholder("ALSA:null"));
    assert!(alsa_pcm_is_placeholder("alsa:null"));
    assert!(alsa_pcm_is_placeholder("ALSA:oss"));
    assert!(alsa_pcm_is_placeholder("ALSA:oss:default"));
    assert!(!alsa_pcm_is_placeholder("ALSA:pulse"));
    assert!(!alsa_pcm_is_placeholder("ALSA:hw:CARD=0,DEV=0"));
}

#[test]
fn the_smallest_period_respects_a_sane_floor() {
    assert_eq!(smallest_output_buffer(&config(16, 4096), 32), 32);
    assert_eq!(smallest_output_buffer(&config(128, 4096), 32), 128);
    assert_eq!(smallest_output_buffer(&config(16, 24), 32), 24);
}

#[test]
fn only_low_latency_asks_for_the_smallest_period() {
    let open = config(16, 4096);
    let floored = config(128, 4096);
    assert_eq!(requested_output_buffer(&open, 256, true), 32);
    assert_eq!(requested_output_buffer(&floored, 256, true), 128);
    assert_eq!(requested_output_buffer(&open, 256, false), 256);
}

#[test]
fn the_mode_line_names_the_period_that_runs() {
    let note = crate::audio::output::FIXED_BUFFER_REFUSED;
    let low = "Low latency · ALSA smallest period";
    assert_eq!(describe(true, true).0, OutputMode::LowLatency);
    assert_eq!(describe(true, true).1, low);
    assert_eq!(describe(true, false).0, OutputMode::Standard);
    assert_eq!(describe(true, false).1, format!("{low}{note}"));
    assert_eq!(describe(false, true).0, OutputMode::Standard);
    assert_eq!(describe(false, true).1, "Standard output");
    assert_eq!(describe(false, false).1, format!("Standard output{note}"));
}
