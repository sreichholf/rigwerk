use super::DenormalGuard;
use std::hint::black_box;

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
#[test]
fn the_guard_flushes_subnormal_results_and_restores_the_mode() {
    let tiny = black_box(1.0e-30f32);
    let scale = black_box(1.0e-10f32);
    let before = black_box(tiny * scale);
    assert!(before.is_subnormal(), "{before:e}");
    {
        let _guard = DenormalGuard::new();
        let flushed = black_box(black_box(tiny) * black_box(scale));
        assert_eq!(flushed, 0.0);
    }
    let after = black_box(black_box(tiny) * black_box(scale));
    assert!(after.is_subnormal(), "{after:e}");
}

#[cfg(target_arch = "x86_64")]
#[test]
fn the_guard_reads_subnormal_inputs_as_zero() {
    let subnormal = black_box(f32::from_bits(1));
    let _guard = DenormalGuard::new();
    assert_eq!(black_box(subnormal * black_box(1.0f32)), 0.0);
}

#[test]
fn nested_guards_restore_in_order() {
    let outer = DenormalGuard::new();
    {
        let _inner = DenormalGuard::new();
    }
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    assert_eq!(
        black_box(black_box(1.0e-30f32) * black_box(1.0e-10f32)),
        0.0
    );
    drop(outer);
}
