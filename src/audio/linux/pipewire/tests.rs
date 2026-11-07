use super::{
    decide_quantum_force, default_pcm_is_pipewire, is_pipewire_pcm, metadata_value, parse_state,
    QuantumForce,
};

#[test]
fn the_pipewire_pcm_is_recognised_with_or_without_a_host_prefix() {
    assert!(is_pipewire_pcm("ALSA:pipewire"));
    assert!(is_pipewire_pcm("alsa:pipewire"));
    assert!(is_pipewire_pcm("pipewire"));
    assert!(!is_pipewire_pcm("ALSA:default"));
    assert!(!is_pipewire_pcm("ALSA:pulse"));
}

#[test]
fn a_forced_quantum_wins_over_the_default() {
    let text = "update: id:0 key:'clock.quantum' value:'1024' type:''\n\
                update: id:0 key:'clock.force-quantum' value:'64' type:''\n\
                update: id:0 key:'clock.rate' value:'48000' type:''";
    let state = parse_state(text);
    assert_eq!(state.quantum, 64);
    assert_eq!(state.rate, 48_000);
}

#[test]
fn zero_force_falls_back_to_the_default_quantum() {
    let text = "update: id:0 key:'clock.quantum' value:'1024' type:''\n\
                update: id:0 key:'clock.force-quantum' value:'0' type:''";
    assert_eq!(parse_state(text).quantum, 1024);
}

#[test]
fn values_are_read_from_the_metadata_line() {
    let text = "update: id:0 key:'clock.rate' value:'44100' type:''";
    assert_eq!(metadata_value(text, "clock.rate"), Some(44_100));
}

#[test]
fn a_pulse_default_is_not_pipewire_just_because_the_name_default_appears() {
    let pulse = "pcm.!default {\n    type pulse\n    hint { show on description \"Default\" }\n}\n";
    assert!(!default_pcm_is_pipewire(pulse));
    let pipewire = "pcm.!default {\n    type pipewire\n}\n";
    assert!(default_pcm_is_pipewire(pipewire));
}

#[test]
fn a_later_default_block_overrides_an_earlier_one() {
    let config = "pcm.!default { type pipewire }\n\
                  pcm.!default { type pulse }\n";
    assert!(!default_pcm_is_pipewire(config));
    let back = "pcm.!default { type pulse }\n\
                pcm.!default { type pipewire }\n";
    assert!(default_pcm_is_pipewire(back));
}

#[test]
fn a_commented_pipewire_type_does_not_count() {
    let config = "pcm.!default {\n    # type pipewire\n    type pulse\n}\n";
    assert!(!default_pcm_is_pipewire(config));
}

#[test]
fn an_unread_quantum_is_not_treated_as_zero_and_a_write_is_owned() {
    assert_eq!(
        decide_quantum_force(None, false, false, false),
        QuantumForce::Untouched
    );
    assert_eq!(
        decide_quantum_force(Some(0), false, false, false),
        QuantumForce::Untouched
    );
    assert_eq!(
        decide_quantum_force(Some(0), true, true, false),
        QuantumForce::Owned { previous: 0 }
    );
    assert_eq!(
        decide_quantum_force(Some(1024), true, false, true),
        QuantumForce::RolledBack
    );
    assert_eq!(
        decide_quantum_force(Some(1024), true, false, false),
        QuantumForce::Owned { previous: 1024 }
    );
}

#[test]
fn restore_quantum_reports_a_missing_tool() {
    match super::restore_quantum(0) {
        Ok(()) => {}
        Err(error) => assert!(
            error.contains("pw-metadata"),
            "restore error should name the tool: {error}"
        ),
    }
}
