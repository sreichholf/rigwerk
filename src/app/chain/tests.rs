use super::ChainSelection;
use crate::audio::EqSlot;

#[test]
fn eq_edit_round_trips_through_selection() {
    assert_eq!(
        ChainSelection::from_eq_edit(EqSlot::Pre).eq_slot(),
        Some(EqSlot::Pre)
    );
    assert_eq!(
        ChainSelection::from_eq_edit(EqSlot::Post).eq_slot(),
        Some(EqSlot::Post)
    );
}

#[test]
fn plugin_selection_clamps_after_remove() {
    assert_eq!(
        ChainSelection::Plugin(3).clamp(2),
        ChainSelection::Plugin(1)
    );
    assert_eq!(ChainSelection::Plugin(0).clamp(0), ChainSelection::Plugins);
}

#[test]
fn selection_follows_its_effect_after_remove() {
    // Removing an earlier effect keeps the same effect selected.
    assert_eq!(
        ChainSelection::Plugin(2).after_removal(0, 2),
        ChainSelection::Plugin(1)
    );
    // Removing a later one leaves the selection alone.
    assert_eq!(
        ChainSelection::Plugin(0).after_removal(1, 2),
        ChainSelection::Plugin(0)
    );
    // Removing the selected last effect selects the new last one.
    assert_eq!(
        ChainSelection::Plugin(2).after_removal(2, 2),
        ChainSelection::Plugin(1)
    );
    assert_eq!(
        ChainSelection::Plugin(0).after_removal(0, 0),
        ChainSelection::Plugins
    );
    assert_eq!(
        ChainSelection::Gate.after_removal(0, 0),
        ChainSelection::Gate
    );
    // Removing the selected effect selects the one that slid into place.
    assert_eq!(
        ChainSelection::Plugin(1).after_removal(1, 2),
        ChainSelection::Plugin(1)
    );
    assert_eq!(
        ChainSelection::Plugins.after_removal(0, 1),
        ChainSelection::Plugins
    );
}

#[test]
fn output_device_selection_falls_back_to_the_master() {
    assert_eq!(
        ChainSelection::OutputDevice(0).clamp_output(1),
        ChainSelection::OutputDevice(0)
    );
    assert_eq!(
        ChainSelection::OutputDevice(1).clamp_output(1),
        ChainSelection::Output
    );
    assert_eq!(ChainSelection::Gate.clamp_output(0), ChainSelection::Gate);
    // The effect clamp leaves output devices alone.
    assert_eq!(
        ChainSelection::OutputDevice(0).clamp(0),
        ChainSelection::OutputDevice(0)
    );
    assert_eq!(ChainSelection::OutputDevice(0).eq_slot(), None);
}

/// A stack with one block in `position`, for the chain-order tests.
fn one_block(position: crate::audio::NamPosition) -> crate::audio::NamStack {
    use crate::audio::{NamBlockControls, NamStack};
    let mut stack = NamStack::default();
    stack.push(NamBlockControls {
        id: 0,
        position,
        ..NamBlockControls::default()
    });
    stack
}

#[test]
fn the_chain_is_in_signal_order_with_the_gate_where_its_slot_says() {
    use super::{chain_stages, ChainStage::*};
    use crate::audio::GateSlot;
    let pre = one_block(crate::audio::NamPosition::Pre);
    assert_eq!(
        chain_stages(2, GateSlot::Post, &pre),
        [PreEq, Nam(0), Effect(0), Effect(1), Cab, Gate, PostEq]
    );
    assert_eq!(
        chain_stages(2, GateSlot::Pre, &pre),
        [PreEq, Gate, Nam(0), Effect(0), Effect(1), Cab, PostEq]
    );
    assert_eq!(
        chain_stages(0, GateSlot::Post, &pre),
        [PreEq, Nam(0), Cab, Gate, PostEq]
    );
}

#[test]
fn the_nam_block_sits_before_effects_by_default_and_after_when_post() {
    use super::{chain_stages, ChainStage::*};
    use crate::audio::{GateSlot, NamPosition};
    assert_eq!(NamPosition::default(), NamPosition::Pre);
    let pre = one_block(NamPosition::Pre);
    let post = one_block(NamPosition::Post);
    assert_eq!(
        chain_stages(1, GateSlot::Post, &pre),
        [PreEq, Nam(0), Effect(0), Cab, Gate, PostEq]
    );
    assert_eq!(
        chain_stages(1, GateSlot::Post, &post),
        [PreEq, Effect(0), Nam(0), Cab, Gate, PostEq]
    );
    assert_eq!(
        chain_stages(1, GateSlot::Pre, &post),
        [PreEq, Gate, Effect(0), Nam(0), Cab, PostEq]
    );
    let stages = chain_stages(1, GateSlot::Post, &post);
    let nam = stages
        .iter()
        .position(|stage| *stage == Nam(0))
        .expect("nam");
    let cab = stages.iter().position(|stage| *stage == Cab).expect("cab");
    let gate = stages
        .iter()
        .position(|stage| *stage == Gate)
        .expect("gate");
    assert_eq!(nam + 1, cab, "the cabinet follows a post NAM block");
    assert_eq!(cab + 1, gate, "the cabinet precedes a post gate");
}

#[test]
fn nam_blocks_in_both_positions_appear_in_signal_order() {
    use super::{chain_stages, neighbour_stage, ChainStage::*};
    use crate::audio::{GateSlot, NamBlockControls, NamPosition, NamStack};
    let mut stack = NamStack::default();
    for (id, position) in [
        (0, NamPosition::Pre),
        (1, NamPosition::Pre),
        (2, NamPosition::Post),
    ] {
        assert!(stack.push(NamBlockControls {
            id,
            position,
            ..NamBlockControls::default()
        }));
    }
    assert_eq!(
        chain_stages(2, GateSlot::Post, &stack),
        [
            PreEq,
            Nam(0),
            Nam(1),
            Effect(0),
            Effect(1),
            Nam(2),
            Cab,
            Gate,
            PostEq
        ]
    );
    // An empty stack still gets one row: the sheet's way in.
    assert_eq!(
        chain_stages(0, GateSlot::Post, &NamStack::default()),
        [PreEq, Nam(0), Cab, Gate, PostEq]
    );
    // ‹ › walks from the Pre group into the effects and into the Post group.
    assert_eq!(
        neighbour_stage(Nam(1), 2, GateSlot::Post, &stack, true),
        Some(Effect(0))
    );
    assert_eq!(
        neighbour_stage(Nam(2), 2, GateSlot::Post, &stack, false),
        Some(Effect(1))
    );
}

#[test]
fn neighbours_walk_the_chain_and_stop_at_its_ends() {
    use super::{neighbour_stage, ChainStage::*};
    use crate::audio::GateSlot;
    let nam = one_block(crate::audio::NamPosition::Pre);
    let next = |stage, gate| neighbour_stage(stage, 1, gate, &nam, true);
    let previous = |stage, gate| neighbour_stage(stage, 1, gate, &nam, false);
    assert_eq!(next(PreEq, GateSlot::Pre), Some(Gate));
    assert_eq!(next(Gate, GateSlot::Pre), Some(Nam(0)));
    assert_eq!(next(Nam(0), GateSlot::Pre), Some(Effect(0)));
    assert_eq!(next(Effect(0), GateSlot::Pre), Some(Cab));
    assert_eq!(next(Cab, GateSlot::Pre), Some(PostEq));
    assert_eq!(next(Effect(0), GateSlot::Post), Some(Cab));
    assert_eq!(next(Cab, GateSlot::Post), Some(Gate));
    assert_eq!(next(Gate, GateSlot::Post), Some(PostEq));
    assert_eq!(previous(PostEq, GateSlot::Post), Some(Gate));
    assert_eq!(previous(Gate, GateSlot::Post), Some(Cab));
    assert_eq!(next(PostEq, GateSlot::Post), None);
    assert_eq!(previous(PreEq, GateSlot::Post), None);
    assert_eq!(next(Effect(4), GateSlot::Post), None, "not in the chain");
}

#[test]
fn stages_switch_on_and_off_per_path() {
    use super::ChainStage;
    let mut app = crate::app::blank_app();
    assert!(app.stage_on(0, ChainStage::PreEq));
    app.set_stage_on(0, ChainStage::PreEq, false);
    assert!(!app.paths[0].as_ref().unwrap().mix.eqs[0].enabled);
    assert!(app.paths[0].as_ref().unwrap().mix.eqs[1].enabled);
    app.set_stage_on(0, ChainStage::Gate, true);
    assert!(app.stage_on(0, ChainStage::Gate));
    app.engine.restore_plugins(
        0,
        vec![crate::audio::PluginPref {
            name: "A".to_owned(),
            path: "a.vst3".to_owned(),
            ..Default::default()
        }],
    );
    app.set_stage_on(0, ChainStage::Effect(0), false);
    assert!(app.engine.plugin_views(0)[0].bypass);
    assert!(!app.stage_on(0, ChainStage::Effect(0)));
    assert!(!app.stage_on(3, ChainStage::PreEq), "no path in slot 3");
}
