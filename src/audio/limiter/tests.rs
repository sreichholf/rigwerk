use super::{safety_ceiling_linear, SafetyLimiter};

#[test]
fn a_full_scale_signal_is_held_at_the_ceiling() {
    let ceiling = safety_ceiling_linear();
    let mut limiter = SafetyLimiter::new(48_000);
    let mut frame = [1.0f32, -1.0];
    limiter.process(&mut frame);
    assert_eq!(frame, [ceiling, -ceiling]);

    let mut huge = [f32::MAX, -f32::MAX];
    limiter.process(&mut huge);
    assert_eq!(huge, [ceiling, -ceiling]);
}

#[test]
fn a_signal_under_the_ceiling_is_unchanged() {
    let mut limiter = SafetyLimiter::new(48_000);
    let mut frame = [-0.0f32, 0.25, -0.5, 0.1];
    let before = frame.map(f32::to_bits);
    limiter.process(&mut frame);
    assert_eq!(frame.map(f32::to_bits), before);

    let ceiling = safety_ceiling_linear();
    let mut near = [ceiling * 0.99, -0.2];
    let before = near.map(f32::to_bits);
    limiter.process(&mut near);
    assert_eq!(near.map(f32::to_bits), before);
    assert_eq!(limiter.reduction_db(), 0.0);
}

#[test]
fn gain_reduction_is_zero_when_idle_and_reports_while_limiting() {
    let mut limiter = SafetyLimiter::new(48_000);
    assert_eq!(limiter.reduction_db(), 0.0);
    let mut quiet = [0.2f32, -0.2];
    limiter.process(&mut quiet);
    assert_eq!(limiter.reduction_db(), 0.0);

    let ceiling = safety_ceiling_linear();
    let peak = ceiling * 10.0f32.powf(3.2 / 20.0);
    let mut hot = [peak, peak * -0.25];
    limiter.process(&mut hot);
    assert!((hot[0] - ceiling).abs() < 1.0e-5, "{}", hot[0]);
    let gr = limiter.reduction_db();
    assert!((gr - 3.2).abs() < 0.05, "reduction while limiting: {gr}");

    for _ in 0..48_000 {
        let mut rest = [0.0f32, 0.0];
        limiter.process(&mut rest);
    }
    assert_eq!(limiter.reduction_db(), 0.0, "release returns to unity");
    let mut again = [-0.25f32, 0.1];
    let before = again.map(f32::to_bits);
    limiter.process(&mut again);
    assert_eq!(again.map(f32::to_bits), before);
}
