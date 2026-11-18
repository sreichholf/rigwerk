use super::is_vb_cable_render;

#[test]
fn vb_cable_render_names_are_recognised() {
    assert!(is_vb_cable_render("CABLE Input (VB-Audio Virtual Cable)"));
    assert!(is_vb_cable_render("cable input (vb-audio)"));
    assert!(!is_vb_cable_render("CABLE Output (VB-Audio Virtual Cable)"));
    assert!(!is_vb_cable_render("Speakers (Realtek)"));
}
