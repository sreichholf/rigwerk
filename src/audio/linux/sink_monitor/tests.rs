use std::time::{Duration, Instant};

use super::{chunk_peak, monitor_target, MonitorMeter, MonitorReader};

fn bytes(samples: &[f32]) -> Vec<u8> {
    samples
        .iter()
        .flat_map(|sample| sample.to_le_bytes())
        .collect()
}

#[test]
fn the_chunk_peak_reads_only_the_valid_window() {
    let data = bytes(&[0.9, -0.25, 0.5, -0.75]);
    assert_eq!(chunk_peak(&data, 0, 16), 0.9);
    assert_eq!(chunk_peak(&data, 4, 8), 0.5, "offset and size bound it");
    assert_eq!(chunk_peak(&data, 4, 4096), 0.75, "never past the mapping");
    assert_eq!(chunk_peak(&data, 16 + 12, 4), 0.75, "offset wraps");
    assert_eq!(chunk_peak(&[], 0, 16), 0.0);
    assert_eq!(chunk_peak(&bytes(&[f32::NAN, 0.1]), 0, 8), 0.1);
}

#[test]
fn the_reader_keeps_the_peak_since_the_last_read_and_goes_stale() {
    let meter = MonitorMeter::default();
    let mut reader = MonitorReader::default();
    let now = Instant::now();
    assert_eq!(reader.read(&meter, now), None, "no block yet");
    meter.note_block(0.2);
    meter.note_block(-0.6);
    meter.note_block(0.1);
    assert_eq!(reader.read(&meter, now), Some(0.6));
    meter.note_block(0.05);
    assert_eq!(reader.read(&meter, now), Some(0.05), "reset on each read");
    assert_eq!(
        reader.read(&meter, now + Duration::from_millis(100)),
        Some(0.05),
        "between blocks the last reading holds"
    );
    assert_eq!(
        reader.read(&meter, now + Duration::from_millis(300)),
        None,
        "no block for a while: no reading"
    );
}

#[test]
fn only_pipewire_sinks_get_a_monitor() {
    assert_eq!(
        monitor_target("pipewire:alsa_output.usb"),
        Some(Some("alsa_output.usb".to_owned()))
    );
    assert_eq!(monitor_target("alsa:pipewire"), Some(None), "default sink");
    assert_eq!(monitor_target("alsa:hw:CARD=USB,DEV=0"), None);
}
