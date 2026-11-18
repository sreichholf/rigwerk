use super::PathLayout;
use crate::app::alignment::RoutingWarningKind;
use crate::app::blank_app;
use crate::app::settings::ExtraOutputPref;
use crate::audio::ChannelMap;

#[test]
fn layouts_set_routing_add_paths_and_read_back() {
    let mut app = blank_app();
    app.extra_outputs.push(ExtraOutputPref::virtual_output());
    assert_eq!(app.current_layout(), Some(PathLayout::Single));

    assert!(app.apply_layout(PathLayout::DryWet));
    assert_eq!(app.path_count(), 2);
    let second = app.path_slots()[1];
    assert!(
        !app.paths[second].as_ref().unwrap().mix.mute,
        "added playing"
    );
    assert_eq!(app.current_layout(), Some(PathLayout::DryWet));
    assert_eq!(
        app.clock_output.map(0),
        ChannelMap {
            left: 0b01,
            right: 0
        }
    );
    assert_eq!(
        app.clock_output.map(second),
        ChannelMap {
            left: 0,
            right: 0b10
        }
    );
    assert_eq!(
        app.extra_outputs[0].settings.map(second),
        ChannelMap::default()
    );
    // Aligned devices never warn.
    assert!(app.routing_warnings(0).is_empty());
    assert!(app.routing_warnings(1).is_empty());
    // Unaligned with the same latency and trim: still lined up.
    app.set_device_align(0, false);
    app.set_device_align(1, false);
    assert!(app.routing_warnings(1).is_empty());
    // A trim on the second path: the stream device sums two paths that
    // arrive apart (a comb filter) ...
    for device in 0..2 {
        let mut entry = app.device_path_map(device, second).unwrap();
        entry.trim_ms = 1.0;
        app.set_device_path_map(device, second, entry);
    }
    let warnings = app.routing_warnings(1);
    assert_eq!(warnings[0].kind, RoutingWarningKind::Sum);
    assert_eq!(warnings[0].paths, vec![0, second]);
    // ... and on the clock output they sit on different channels.
    assert_eq!(app.routing_warnings(0)[0].kind, RoutingWarningKind::Shift);
    for device in 0..2 {
        app.set_device_align(device, true);
        let mut entry = app.device_path_map(device, second).unwrap();
        entry.trim_ms = 0.0;
        app.set_device_path_map(device, second, entry);
    }

    assert!(app.apply_layout(PathLayout::PerDevice));
    assert_eq!(app.current_layout(), Some(PathLayout::PerDevice));
    assert!(app.routing_warnings(0).is_empty());
    assert!(app.path_reaches_device(0, 0) && !app.path_reaches_device(0, 1));
    assert!(app.path_reaches_device(second, 1));
    assert!(app.path_infos().iter().all(|info| info.routed));

    // Single keeps the second path (and its effects) but routes it nowhere.
    assert!(app.apply_layout(PathLayout::Single));
    assert_eq!(app.path_count(), 2);
    assert_eq!(app.current_layout(), Some(PathLayout::Single));
    assert!(!app.path_infos()[1].routed);

    // Any edit afterwards reads as Custom.
    app.clock_output.path_map[0].level_db = -1.0;
    assert_eq!(app.current_layout(), None);
}

#[test]
fn blend_and_dual_amp_copy_the_first_path() {
    let mut app = blank_app();
    app.path_mut().mix.mono = true;
    assert!(PathLayout::Blend.sums_paths() && PathLayout::DualAmp.sums_paths());
    assert!(!PathLayout::DryWet.sums_paths());
    assert!(app.apply_layout(PathLayout::DualAmp));
    let second = app.path_slots()[1];
    assert!(app.paths[second].as_ref().unwrap().mix.mono, "a copy");
    assert!(app.clock_output.path_map[second].invert);
    assert_eq!(app.current_layout(), Some(PathLayout::DualAmp));
    assert!(app.apply_layout(PathLayout::Blend));
    assert_eq!(app.path_count(), 2);
    assert_eq!(app.clock_output.path_map[0].level_db, -6.0);
    assert!(!app.clock_output.path_map[second].invert);
    assert_eq!(app.current_layout(), Some(PathLayout::Blend));
}
