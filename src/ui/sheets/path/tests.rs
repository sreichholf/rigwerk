use super::{level_label, PATH_COLOUR_NAMES};
use crate::ui::theme::DARK;

#[test]
fn a_level_reads_with_the_real_minus_and_one_decimal() {
    assert_eq!(level_label(0.0), "0.0 dB");
    assert_eq!(level_label(-6.0), "−6.0 dB");
    assert_eq!(level_label(1.5), "+1.5 dB");
}

#[test]
fn every_path_colour_has_a_name() {
    assert_eq!(PATH_COLOUR_NAMES.len(), DARK.path.len());
}
