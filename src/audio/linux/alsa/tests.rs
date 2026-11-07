use super::{
    alsa_pcm_is_default, alsa_pcm_is_direct, alsa_pcm_is_placeholder, smallest_output_buffer,
};

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
