use super::super::outputs::{channel_list, MapEdit};
use super::latency_line;
use crate::app::blank_app;
use crate::app::cards::Card;
use crate::ui::widgets::{accesskit_frame, node_named};

#[test]
fn the_latency_line_says_what_the_device_waits_for() {
    let mut app = blank_app();
    assert!(app.apply_card(Card::Blend));
    let infos = app.path_infos();
    let both = [&infos[0], &infos[1]];
    let line = |app: &crate::app::RigWerkApp| {
        latency_line("Scarlett", &app.device_alignment(0).unwrap(), &both)
    };
    assert_eq!(
        line(&app),
        "Nothing to align: every path on Scarlett has the same latency (0.0 ms)."
    );
    assert_eq!(
        latency_line("Scarlett", &app.device_alignment(0).unwrap(), &both[..1]),
        "Nothing to align: Main is the only path on Scarlett (0.0 ms)."
    );
    // 2 ms of plugin latency on the second path: the first waits.
    app.engine.screenshot_path_latency(infos[1].slot, 96);
    assert_eq!(
        line(&app),
        "Waits for Main 2, +2.0 ms. Every path on Scarlett plays 2.0 ms after its chain input."
    );
    // A trim on the first path: they no longer arrive together.
    app.clock_device.path_map[infos[0].slot].trim_ms = 0.5;
    assert_eq!(
        line(&app),
        "Waits for Main 2, +2.5 ms. Paths on Scarlett play up to 2.5 ms after their chain input (trims shift paths by up to 0.50 ms)."
    );
    app.clock_device.path_map[infos[0].slot].trim_ms = 0.0;
    app.set_device_align(0, false);
    assert_eq!(
        line(&app),
        "Alignment off: each path plays on Scarlett as its chain finishes, up to 2.0 ms apart."
    );
    app.engine.screenshot_path_latency(infos[1].slot, 0);
    assert_eq!(channel_list(&[0, 2]), "Out 1, Out 3");
}

#[test]
fn the_sum_block_edits_trim_and_align_and_flags_the_unaligned_sum() {
    let mut app = blank_app();
    assert!(app.apply_card(Card::DualAmp));
    let second = app.path_slots()[1];
    app.apply_map_edit(
        0,
        MapEdit::Trim {
            slot: second,
            trim_ms: 9.0,
            commit: true,
        },
    );
    assert_eq!(app.clock_device.path_map[second].trim_ms, 5.0, "clamped");
    assert!(app
        .log
        .iter()
        .any(|entry| entry.text.contains("Main 2 trim on")));
    app.apply_map_edit(
        0,
        MapEdit::Trim {
            slot: second,
            trim_ms: 0.0,
            commit: false,
        },
    );
    app.apply_map_edit(0, MapEdit::Align(false));
    assert!(!app.device_align(0));
    app.engine.screenshot_path_latency(second, 96);
    let tree = accesskit_frame(|ui| app.routing_page(ui));
    let trim = format!("Main 2 trim on {}", app.output_device_rows()[0].name);
    let _ = node_named(&tree, &trim);
    let _ = node_named(&tree, "Align paths");
    app.apply_map_edit(0, MapEdit::Align(true));
    assert!(app.device_align(0));
    assert!(app.routing_warnings(0).is_empty());
    app.engine.screenshot_path_latency(second, 0);
}
