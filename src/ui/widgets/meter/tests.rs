use super::{
    bar_width, horizontal_meter, linear_to_db, meter_fraction, vertical_meter, zone_colour,
    MeterChannel,
};
use crate::ui::theme::{palette, DARK, LIGHT};
use crate::ui::widgets::{node_named, run_frame, test_input};
use eframe::egui;

fn near(actual: f32, expected: f32) {
    assert!((actual - expected).abs() < 1.0e-3, "{actual} != {expected}");
}

#[test]
fn zones_follow_the_palette_in_both_themes() {
    let ctx = egui::Context::default();
    ctx.set_theme(egui::Theme::Dark);
    let colours = palette(&ctx);
    assert_eq!(zone_colour(colours, -12.0), colours.meter);
    assert_eq!(zone_colour(colours, -6.0), colours.meter);
    assert_eq!(zone_colour(colours, -3.0), colours.meter_hot);
    assert_eq!(zone_colour(colours, -1.0), colours.meter_hot);
    assert_eq!(zone_colour(colours, 0.5), colours.error);
    assert_eq!(colours.meter, DARK.meter);
    ctx.set_theme(egui::Theme::Light);
    let colours = palette(&ctx);
    assert_eq!(zone_colour(colours, -3.0), colours.meter_hot);
    assert_eq!(zone_colour(colours, 1.0), colours.error);
    assert_eq!(colours.error, LIGHT.error);
    assert_eq!(colours.meter_hot, LIGHT.meter_hot);
}

#[test]
fn the_scale_and_the_bar_width_follow_the_channel_count() {
    near(meter_fraction(0.0), 1.0);
    near(meter_fraction(-48.0), 0.0);
    near(meter_fraction(-24.0), 0.5);
    near(linear_to_db(1.0), 0.0);
    assert!(linear_to_db(2.0) > 0.0);
    assert_eq!(bar_width(1), 5.0);
    assert_eq!(bar_width(2), 5.0);
    assert_eq!(bar_width(4), 3.0);
    assert_eq!(bar_width(8), 2.0);
}

#[test]
fn the_peak_holds_for_one_and_a_half_seconds_and_a_clip_latches() {
    let ctx = egui::Context::default();
    let hot = 10f32.powf(-3.0 / 20.0);
    let quiet = 10f32.powf(-40.0 / 20.0);
    let mut state = Vec::new();
    let at = |time: f64, level: f32, state: &mut Vec<MeterChannel>| {
        let mut input = test_input(None);
        input.time = Some(time);
        run_frame(&ctx, input, |ui| {
            vertical_meter(ui, &[level], state, 160.0, true, "Input meter");
        });
    };
    at(10.0, hot, &mut state);
    near(state[0].peak_db, -3.0);
    assert!(!state[0].clipped);
    at(11.0, quiet, &mut state);
    near(state[0].peak_db, -3.0);
    at(11.6, quiet, &mut state);
    near(state[0].peak_db, -40.0);

    at(12.0, 2.0, &mut state);
    assert!(state[0].clipped, "0 dBFS and above latches");
    at(12.1, quiet, &mut state);
    assert!(state[0].clipped, "the latch survives the level falling");
}

#[test]
fn clicking_a_latched_bar_clears_it() {
    let ctx = egui::Context::default();
    let mut state = vec![MeterChannel {
        clipped: true,
        ..MeterChannel::default()
    }];
    let mut rect = egui::Rect::NOTHING;
    let _ = run_frame(&ctx, test_input(None), |ui| {
        rect = vertical_meter(ui, &[0.1], &mut state, 160.0, true, "Input meter").rect;
    });
    assert!(state[0].clipped);
    let mut click = test_input(None);
    click.time = Some(1.0);
    let at = egui::pos2(rect.right() - 2.0, rect.center().y);
    click.events.push(egui::Event::PointerMoved(at));
    for pressed in [true, false] {
        click.events.push(egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        });
    }
    let _ = run_frame(&ctx, click, |ui| {
        vertical_meter(ui, &[0.1], &mut state, 160.0, true, "Input meter");
    });
    assert!(!state[0].clipped);
}

#[test]
fn meters_expose_their_labels() {
    let mut state = Vec::new();
    let tree = crate::ui::widgets::accesskit_frame(|ui| {
        vertical_meter(ui, &[0.5, 0.25], &mut state, 160.0, true, "Room meter");
        horizontal_meter(ui, 0.5, "Room");
    });
    let vertical = node_named(&tree, "Room meter");
    assert_eq!(vertical.role(), egui::Role::Meter);
    assert!(vertical.value().is_some());
    let horizontal = node_named(&tree, "Room");
    assert_eq!(horizontal.role(), egui::Role::Meter);
}

#[test]
fn eight_channels_still_draw() {
    let mut state = Vec::new();
    let levels = [0.2; 8];
    let tree = crate::ui::widgets::accesskit_frame(|ui| {
        vertical_meter(ui, &levels, &mut state, 140.0, false, "Output meter");
    });
    assert_eq!(state.len(), 8);
    assert_eq!(node_named(&tree, "Output meter").role(), egui::Role::Meter);
}
