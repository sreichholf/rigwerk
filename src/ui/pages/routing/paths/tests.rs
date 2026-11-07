use super::{
    align_status, card_caption, card_hint, custom_label, step_level, step_trim, trim_text,
};
use crate::app::cards::Card;
use crate::ui::widgets::path_level_text;

#[test]
fn levels_read_and_step_within_the_range() {
    assert_eq!(path_level_text(0.0), "0.0 dB");
    assert_eq!(path_level_text(-6.0), "−6.0 dB");
    assert_eq!(path_level_text(1.5), "+1.5 dB");
    assert_eq!(path_level_text(-60.0), "−∞ dB");
    assert_eq!(step_level(5.5, 1.0), 6.0);
    assert_eq!(step_level(-60.0, -1.0), -60.0);
    assert_eq!(step_level(-60.0, 1.0), -59.0);
}

#[test]
fn every_card_has_a_caption() {
    for card in Card::ALL {
        assert!(!card_caption(card).is_empty());
        assert!(!card_hint(card).is_empty());
    }
}

#[test]
fn trims_step_within_five_ms() {
    assert_eq!(step_trim(0.0, 0.1), 0.1);
    assert_eq!(step_trim(4.95, 0.1), 5.0);
    assert_eq!(step_trim(-5.0, -0.1), -5.0);
    assert_eq!(trim_text(0.0), "0.00 ms");
    assert_eq!(trim_text(0.004), "0.00 ms");
    assert_eq!(trim_text(0.01), "+0.01 ms");
    assert_eq!(trim_text(1.25), "+1.25 ms");
    assert_eq!(trim_text(-0.5), "−0.50 ms");
}

#[test]
fn custom_names_the_last_card_and_align_reads_the_latencies() {
    assert_eq!(custom_label(None), "Custom");
    assert_eq!(
        custom_label(Some(Card::PerDevice)),
        "Custom · edited from Per-device mixes"
    );
    let mut app = crate::app::blank_app();
    assert!(app.apply_card(Card::Blend));
    let devices =
        |app: &crate::app::RigWerkApp| vec![app.device_alignment(0).expect("the clock device")];
    assert_eq!(align_status(&devices(&app)), "nothing to align");
    let second = app.path_slots()[1];
    app.engine.screenshot_path_latency(second, 96);
    assert_eq!(align_status(&devices(&app)), "up to +2.0 ms");
    app.set_device_align(0, false);
    assert_eq!(
        align_status(&devices(&app)),
        "nothing to align · off on 1 device"
    );
}
