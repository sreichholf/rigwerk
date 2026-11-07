use super::{frames_to_hns, hns_to_frames, ExclusiveInfo, HNS_PER_SEC};

#[test]
fn an_oversized_endpoint_buffer_matches_its_period_in_hns() {
    // UAC1-style: driver returns 8× the frames implied by the requested
    // period.  Re-init uses that frame count as the new period so wakes
    // and writes stay 1:1 (see match_period_to_buffer).
    let requested = 48u32;
    let allocated = requested * 8;
    assert!(allocated >= requested.saturating_mul(2));
    let matched_hns = frames_to_hns(allocated, 48_000);
    assert_eq!(hns_to_frames(matched_hns, 48_000), allocated);
    assert!(matched_hns > frames_to_hns(requested, 48_000));
}

#[test]
fn exclusive_describe_marks_this_device_only() {
    let info = ExclusiveInfo {
        rate: 48_000,
        channels: 2,
        step_frames: 48,
        min_period_frames: 48,
        default_period_frames: 480,
    };
    let text = info.describe(48_000);
    assert!(text.contains("Exclusive"), "{text}");
    assert!(text.contains("this device only"), "{text}");
    assert!(text.contains("48 smp"), "{text}");
    assert!(text.contains("1.0 ms"), "{text}");
    assert!(text.contains("down from"), "{text}");
}

#[test]
fn exclusive_describe_notes_rate_mismatch() {
    let info = ExclusiveInfo {
        rate: 44_100,
        channels: 2,
        step_frames: 64,
        min_period_frames: 64,
        default_period_frames: 441,
    };
    let text = info.describe(48_000);
    assert!(text.contains("44100 Hz") || text.contains("44"), "{text}");
}

#[test]
fn hns_frame_round_trip_is_stable() {
    let rate = 48_000;
    for frames in [32u32, 48, 64, 96, 128, 256, 480] {
        let hns = frames_to_hns(frames, rate);
        assert!(hns > 0);
        assert_eq!(hns_to_frames(hns, rate), frames);
    }
    assert_eq!(frames_to_hns(480, 48_000), HNS_PER_SEC / 100);
}
