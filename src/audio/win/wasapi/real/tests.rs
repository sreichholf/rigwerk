use super::RealInfo;

fn info(step: u32, min: u32, default: u32) -> RealInfo {
    RealInfo {
        rate: 48_000,
        channels: 2,
        step_frames: step,
        min_frames: min,
        default_frames: default,
    }
}

#[test]
fn a_driver_with_only_its_default_says_so() {
    let text = info(480, 480, 480).describe(48_000);
    assert!(text.contains("no smaller step"), "{text}");
    assert!(!text.contains("down from"), "{text}");
}

#[test]
fn a_genuinely_smaller_step_is_reported() {
    let text = info(128, 128, 480).describe(48_000);
    assert!(text.contains("down from 480 smp"), "{text}");
    assert!(!text.contains("no smaller step"), "{text}");
}

#[test]
fn a_step_held_by_another_app_is_reported() {
    let text = info(480, 128, 480).describe(48_000);
    assert!(text.contains("another app holds"), "{text}");
}

#[test]
fn access_denied_is_recognised() {
    assert!(super::access_denied(0x8007_0005u32 as i32));
    assert!(!super::access_denied(0x8889_000Au32 as i32));
}
