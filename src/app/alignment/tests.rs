use crate::app::blank_app;
use crate::app::settings::FollowerPref;

#[test]
fn alignment_follows_the_switch_and_the_trims() {
    let mut app = blank_app();
    app.followers.push(FollowerPref::virtual_out());
    let second = app
        .duplicate_path(app.path_slots()[0])
        .expect("a free slot");
    assert!(app.device_align(0) && app.device_align(1));
    // No latency: nothing waits and nothing is added.
    // No bridge runs: the engine reports nothing, the app works it out.
    assert!(app.engine.device_alignment(0).is_none());
    let aligned = app.device_alignment(0).expect("the clock device");
    assert!(aligned.align && !aligned.live);
    assert_eq!(aligned.waits_for, None);
    assert_eq!(aligned.per_path[0], Some(0.0));
    assert_eq!(aligned.per_path[second], Some(0.0));
    assert_eq!(aligned.added_ms, 0.0);

    // A 1 ms trim on the second path delays it by 1 ms.
    let mut entry = app.device_path_map(0, second).unwrap();
    entry.trim_ms = 1.0;
    app.set_device_path_map(0, second, entry);
    let trimmed = app.device_alignment(0).unwrap();
    assert!((trimmed.per_path[second].unwrap() - 1.0).abs() < 1e-3);
    assert!((trimmed.total_ms - 1.0).abs() < 1e-3);
    // A negative trim makes the other path wait instead.
    entry.trim_ms = -2.0;
    app.set_device_path_map(0, second, entry);
    let early = app.device_alignment(0).unwrap();
    assert_eq!(early.per_path[second], Some(0.0));
    assert!((early.per_path[0].unwrap() - 2.0).abs() < 1e-3);

    app.set_device_align(1, false);
    assert!(!app.device_align(1) && app.device_align(0));
    assert!(!app.device_alignment(1).unwrap().align);
    assert!(app.device_alignment(2).is_none());
}
