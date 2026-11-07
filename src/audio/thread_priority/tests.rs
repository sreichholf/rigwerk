use super::{priority_for, PromoteOnce};

#[test]
fn the_rt_limit_caps_the_priority() {
    assert_eq!(priority_for(95, 1, 99), 70);
    assert_eq!(priority_for(40, 1, 99), 40);
    // No limit: try the wanted priority and let the kernel decide.
    assert_eq!(priority_for(0, 1, 99), 70);
    assert_eq!(priority_for(libc::RLIM_INFINITY, 1, 99), 70);
}

#[test]
fn promotion_is_tried_once_and_never_panics() {
    let mut once = PromoteOnce::default();
    std::thread::spawn(move || {
        once.run();
        once.run();
        assert!(once.tried);
    })
    .join()
    .unwrap();
}
