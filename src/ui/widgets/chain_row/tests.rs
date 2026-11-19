use super::{
    add_effect_row, bypass_all, chain_event, chain_row, curve_db, effects_header, ChainEvent,
    ChainMark, ChainRow,
};
use crate::audio::EqSettings;
use crate::ui::widgets::{accesskit_frame, node_named, run_frame, test_input};
use eframe::egui::{self, accesskit::Toggled};

fn row<'a>(name: &'a str, mark: ChainMark<'a>) -> ChainRow<'a> {
    ChainRow {
        name,
        shown: None,
        index: 2,
        on: true,
        selected: false,
        failed: false,
        fail_reason: None,
        paused: false,
        mark,
        show_curve: true,
        sample_rate: 48_000,
    }
}

fn click(input: &mut egui::RawInput, at: egui::Pos2, button: egui::PointerButton) {
    input.events.push(egui::Event::PointerMoved(at));
    for pressed in [true, false] {
        input.events.push(egui::Event::PointerButton {
            pos: at,
            button,
            pressed,
            modifiers: egui::Modifiers::default(),
        });
    }
}

fn center(tree: &egui::accesskit::TreeUpdate, name: &str) -> egui::Pos2 {
    let bounds = node_named(tree, name).bounds().expect(name);
    egui::pos2(
        ((bounds.x0 + bounds.x1) / 2.0) as f32,
        ((bounds.y0 + bounds.y1) / 2.0) as f32,
    )
}

#[test]
fn a_low_shelf_lifts_the_left_of_the_mini_curve() {
    let mut eq = EqSettings::default();
    eq.bands[0].gain_db = 12.0;
    let curve = curve_db(&eq, 48_000);
    assert!(
        curve[0] > 9.0,
        "20 Hz should sit on the shelf, was {}",
        curve[0]
    );
    assert!(
        curve[23].abs() < 2.0,
        "20 kHz should be near unity, was {}",
        curve[23]
    );
}

#[test]
fn rows_expose_their_name_and_state() {
    let mut eq = EqSettings::default();
    eq.bands[0].gain_db = 6.0;
    let mut failed = row("Compressor", ChainMark::Latency("5.0 ms"));
    failed.failed = true;
    failed.on = false;
    failed.fail_reason = Some("The plugin did not load");
    let mut paused = row("Limiter", ChainMark::Latency(""));
    paused.paused = true;
    let tree = accesskit_frame(|ui| {
        chain_row(ui, &row("Pre EQ", ChainMark::Eq(&eq)));
        chain_row(ui, &row("Gate", ChainMark::Threshold("−24.0 dB")));
        chain_row(ui, &failed);
        chain_row(ui, &paused);
        let mut on = true;
        bypass_all(ui, &mut on);
        add_effect_row(ui, false);
        effects_header(ui, 2, "5.0 ms", &mut on);
    });
    assert_eq!(node_named(&tree, "Pre EQ").value(), Some("On"));
    assert_eq!(
        node_named(&tree, "Bypass Pre EQ").role(),
        egui::Role::Button
    );
    assert_eq!(node_named(&tree, "Gate").value(), Some("On"));
    assert_eq!(node_named(&tree, "Compressor").value(), Some("Didn't load"));
    assert_eq!(
        node_named(&tree, "Compressor").description(),
        Some("The plugin did not load")
    );
    assert_eq!(node_named(&tree, "Limiter").value(), Some("Paused"));
    assert_eq!(
        node_named(&tree, "Bypass all").toggled(),
        Some(Toggled::True)
    );
    assert_eq!(node_named(&tree, "+ Add effect").role(), egui::Role::Button);
    assert!(node_named(&tree, "+ Add effect").is_disabled());
    assert_eq!(node_named(&tree, "Effects 2").role(), egui::Role::Heading);
    assert_eq!(node_named(&tree, "Effects 2").value(), Some("5.0 ms"));
}

#[test]
fn the_led_bypasses_and_the_row_selects() {
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let sample = row("Pre EQ", ChainMark::Latency(""));
    let show = |ui: &mut egui::Ui| chain_row(ui, &sample);
    let output = run_frame(&ctx, test_input(None), |ui| {
        show(ui);
    });
    let tree = output.platform_output.accesskit_update.expect("tree");
    let led = center(&tree, "Bypass Pre EQ");
    let mut input = test_input(None);
    click(&mut input, led, egui::PointerButton::Primary);
    let mut event = None;
    let _ = run_frame(&ctx, input, |ui| {
        let response = show(ui);
        event = chain_event(&response);
    });
    assert_eq!(event, Some(ChainEvent::Bypass));

    let output = run_frame(&ctx, test_input(None), |ui| {
        show(ui);
    });
    let tree = output.platform_output.accesskit_update.expect("tree");
    let body = center(&tree, "Pre EQ");
    let mut input = test_input(None);
    click(&mut input, body, egui::PointerButton::Primary);
    let mut event = None;
    let _ = run_frame(&ctx, input, |ui| {
        let response = show(ui);
        event = chain_event(&response);
    });
    assert_eq!(event, Some(ChainEvent::Select));
}

#[test]
fn the_effect_menu_removes_and_alt_up_moves() {
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let sample = row("Room", ChainMark::Latency("5.0 ms"));
    let output = run_frame(&ctx, test_input(None), |ui| {
        chain_row(ui, &sample);
    });
    let tree = output.platform_output.accesskit_update.expect("tree");
    let mut input = test_input(None);
    click(
        &mut input,
        center(&tree, "Room"),
        egui::PointerButton::Secondary,
    );
    let _ = run_frame(&ctx, input, |ui| {
        chain_row(ui, &sample);
    });
    // The menu's first frame is a sizing pass.  The items are in the tree
    // on the next frame.
    let output = run_frame(&ctx, test_input(None), |ui| {
        chain_row(ui, &sample);
    });
    let tree = output.platform_output.accesskit_update.expect("tree");
    let mut input = test_input(None);
    click(
        &mut input,
        center(&tree, "Remove"),
        egui::PointerButton::Primary,
    );
    let mut event = None;
    let _ = run_frame(&ctx, input, |ui| {
        let response = chain_row(ui, &sample);
        event = chain_event(&response);
    });
    assert_eq!(event, Some(ChainEvent::Remove));

    let _ = run_frame(&ctx, test_input(None), |ui| {
        chain_row(ui, &sample).request_focus();
    });
    let _ = run_frame(&ctx, test_input(None), |ui| {
        chain_row(ui, &sample);
    });
    let mut input = test_input(None);
    input
        .events
        .push(egui::Event::ModifiersChanged(egui::Modifiers {
            alt: true,
            ..Default::default()
        }));
    input.events.push(egui::Event::Key {
        key: egui::Key::ArrowUp,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers {
            alt: true,
            ..Default::default()
        },
    });
    let mut event = None;
    let _ = run_frame(&ctx, input, |ui| {
        let response = chain_row(ui, &sample);
        event = chain_event(&response);
    });
    assert_eq!(event, Some(ChainEvent::MoveUp));
}

#[test]
fn ctrl_enter_opens_the_focused_effect_editor() {
    let ctx = egui::Context::default();
    let sample = row("Room", ChainMark::Latency("5.0 ms"));
    let _ = run_frame(&ctx, test_input(None), |ui| {
        chain_row(ui, &sample).request_focus();
    });
    let _ = run_frame(&ctx, test_input(None), |ui| {
        chain_row(ui, &sample);
    });
    let mut input = test_input(None);
    input
        .events
        .push(egui::Event::ModifiersChanged(egui::Modifiers::COMMAND));
    input.events.push(egui::Event::Key {
        key: egui::Key::Enter,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::COMMAND,
    });
    let mut event = None;
    let mut clicked = false;
    let mut enter_left = false;
    let _ = run_frame(&ctx, input, |ui| {
        let response = chain_row(ui, &sample);
        event = chain_event(&response);
        clicked = response.clicked();
        enter_left = ui.input(|input| input.key_pressed(egui::Key::Enter));
    });
    assert_eq!(event, Some(ChainEvent::Editor));
    assert!(!clicked);
    assert!(!enter_left);

    let gate = row("Gate", ChainMark::Threshold("−24.0 dB"));
    let _ = run_frame(&ctx, test_input(None), |ui| {
        chain_row(ui, &gate).request_focus();
    });
    let _ = run_frame(&ctx, test_input(None), |ui| {
        chain_row(ui, &gate);
    });
    let mut input = test_input(None);
    input
        .events
        .push(egui::Event::ModifiersChanged(egui::Modifiers::COMMAND));
    input.events.push(egui::Event::Key {
        key: egui::Key::Enter,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::COMMAND,
    });
    let mut event = None;
    let _ = run_frame(&ctx, input, |ui| {
        let response = chain_row(ui, &gate);
        event = chain_event(&response);
    });
    assert_ne!(event, Some(ChainEvent::Editor));
}

#[test]
fn a_long_label_is_shortened_and_does_not_cover_the_name() {
    let ctx = egui::Context::default();
    let long = "STEREO DIRT 2 Gain2 BAL O808 CAB";
    let output = run_frame(&ctx, test_input(None), |ui| {
        ui.allocate_ui(egui::vec2(200.0, 40.0), |ui| {
            chain_row(ui, &row("Amp", ChainMark::Label(long)));
        });
    });
    let mut texts = Vec::new();
    for clipped in &output.shapes {
        if let egui::Shape::Text(text) = &clipped.shape {
            let rect = text.galley.rect.translate(text.pos.to_vec2());
            texts.push((text.galley.text().to_owned(), rect));
        }
    }
    let (_, name) = texts
        .iter()
        .find(|(text, _)| text == "Amp")
        .expect("the name is drawn whole");
    let (mark, mark_rect) = texts
        .iter()
        .find(|(text, _)| text.ends_with('…'))
        .expect("the label is shortened");
    assert!(long.starts_with(mark.trim_end_matches('…')));
    assert!(
        mark_rect.left() >= name.right(),
        "{mark_rect:?} over {name:?}"
    );
}

#[test]
fn a_shown_text_takes_the_name_place_and_keeps_the_name_for_readers() {
    let long = "STEREO DIRT 2 Gain2 BAL O808 CAB";
    let mut amp = row("Amp", ChainMark::Label(""));
    amp.shown = Some(long);
    let ctx = egui::Context::default();
    let output = run_frame(&ctx, test_input(None), |ui| {
        ui.allocate_ui(egui::vec2(200.0, 40.0), |ui| {
            chain_row(ui, &amp);
        });
    });
    let texts: Vec<String> = output
        .shapes
        .iter()
        .filter_map(|clipped| match &clipped.shape {
            egui::Shape::Text(text) => Some(text.galley.text().to_owned()),
            _ => None,
        })
        .collect();
    assert!(!texts.iter().any(|text| text == "Amp"), "{texts:?}");
    let drawn = texts
        .iter()
        .find(|text| text.starts_with("STEREO"))
        .expect("the capture is drawn");
    assert!(drawn.len() > "STEREO DIRT".len(), "uses the row: {drawn}");

    let tree = accesskit_frame(|ui| {
        chain_row(ui, &amp);
    });
    assert_eq!(node_named(&tree, "Amp").description(), Some(long));
}
