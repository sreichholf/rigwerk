use std::sync::Arc;

use super::super::clap::{HostState, AUDIO_PORTS_CHANGED};
use super::load::{merge_synced_params, params_to_restore, Capture, Restore};
use super::pref::{
    base64_decode, base64_encode, PluginFormat, PluginParamPref, PluginParamView, PluginPref,
    PluginState, PLUGIN_STATE_VERSION,
};
use super::slot::Slot;

fn param(id: u32, value: f64) -> PluginParamPref {
    PluginParamPref { id, value }
}

fn view(id: u32, value: f64, default: f64) -> PluginParamView {
    PluginParamView {
        id,
        value,
        default,
        ..PluginParamView::default()
    }
}

#[test]
fn base64_round_trips_every_length() {
    let data: Vec<u8> = (0..=255u8).collect();
    for len in 0..data.len() {
        let encoded = base64_encode(&data[..len]);
        assert_eq!(encoded.len() % 4, 0);
        assert_eq!(base64_decode(&encoded).as_deref(), Some(&data[..len]));
    }
    assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    assert_eq!(base64_encode(b"fo"), "Zm8=");
    assert_eq!(base64_decode("Zm8="), Some(b"fo".to_vec()));
}

#[test]
fn base64_rejects_damage() {
    assert_eq!(base64_decode("Zm8"), None);
    assert_eq!(base64_decode("Zm*="), None);
    assert_eq!(base64_decode("Z==="), None);
    assert_eq!(base64_decode("Zg==Zm8="), None);
}

#[test]
fn state_blob_needs_a_known_version_and_data() {
    let state = PluginState::capture(&[1, 2, 3], &[param(1, 0.5)]);
    assert_eq!(state.version, PLUGIN_STATE_VERSION);
    assert_eq!(state.bytes(), Some(vec![1, 2, 3]));
    assert_eq!(state.params, vec![param(1, 0.5)]);
    let newer = PluginState {
        version: PLUGIN_STATE_VERSION + 1,
        ..state.clone()
    };
    assert_eq!(newer.bytes(), None);
    let damaged = PluginState {
        data: "!!".to_owned(),
        ..state.clone()
    };
    assert_eq!(damaged.bytes(), None);
    assert_eq!(PluginState::capture(&[], &[]).bytes(), None);
}

#[test]
fn pref_round_trips_with_and_without_state() {
    let mut pref = PluginPref {
        path: "C:/Plugins/Verb.clap".to_owned(),
        format: PluginFormat::Clap,
        class_id: Some("com.example.verb".to_owned()),
        params: vec![param(4, 0.75)],
        ..PluginPref::default()
    };
    let plain = ron::to_string(&pref).expect("serialize");
    assert!(!plain.contains("state"), "{plain}");
    let back: PluginPref = ron::from_str(&plain).expect("deserialize");
    assert!(back.state.is_none());
    assert_eq!(back.params, pref.params);

    pref.state = Some(PluginState::capture(b"\x00\xffstate", &pref.params));
    let text = ron::to_string(&pref).expect("serialize");
    let back: PluginPref = ron::from_str(&text).expect("deserialize");
    assert_eq!(back.state, pref.state);
    assert_eq!(
        back.state.and_then(|state| state.bytes()),
        Some(b"\x00\xffstate".to_vec())
    );
}

#[test]
fn old_pref_without_state_still_loads() {
    let back: PluginPref = ron::from_str(
        r#"(path:"C:/A.clap",format:clap,class_id:Some("a"),params:[(id:1,value:0.3)])"#,
    )
    .expect("old layout");
    assert!(back.state.is_none());
    assert_eq!(back.params, vec![param(1, 0.3)]);
    let damaged: PluginPref =
        ron::from_str(r#"(path:"C:/A.clap",state:Some((version:"x")))"#).expect("damaged blob");
    assert!(damaged.state.is_none());
}

#[test]
fn without_a_restored_state_every_saved_value_is_applied() {
    let saved = [param(1, 0.2), param(2, 0.9)];
    assert_eq!(
        params_to_restore(&saved, None),
        vec![(1, Some(0.2)), (2, Some(0.9))]
    );
}

#[test]
fn a_restored_state_takes_precedence_over_unchanged_values() {
    let saved = [param(1, 0.2), param(2, 0.9)];
    assert!(params_to_restore(&saved, Some(&saved)).is_empty());
}

#[test]
fn values_changed_after_the_capture_win_over_the_state() {
    let snapshot = [param(1, 0.2), param(2, 0.9), param(3, 0.4)];
    // 1 moved, 2 unchanged, 3 back to default (no longer saved), 4 new.
    let saved = [param(1, 0.6), param(2, 0.9), param(4, 0.1)];
    assert_eq!(
        params_to_restore(&saved, Some(&snapshot)),
        vec![(1, Some(0.6)), (4, Some(0.1)), (3, None)]
    );
}

#[test]
fn restore_ignores_the_snapshot_when_the_state_did_not_load() {
    let pref = PluginPref {
        params: vec![param(1, 0.2)],
        state: Some(PluginState::capture(&[7], &[param(1, 0.2)])),
        ..PluginPref::default()
    };
    let restore = Restore::from_pref(&pref);
    assert_eq!(restore.state, Some(vec![7]));
    assert!(restore.params(true).is_empty());
    assert_eq!(restore.params(false), vec![(1, Some(0.2))]);
}

#[test]
fn only_a_deferred_capture_keeps_the_slot_stale() {
    assert!(Capture::Deferred.leaves_stale());
    assert!(!Capture::Failed.leaves_stale());
    assert!(!Capture::Saved(PluginState::capture(&[1], &[])).leaves_stale());
}

#[test]
fn a_kept_older_blob_still_restores_newer_values() {
    // A panel move was still queued at save time, so the capture was
    // deferred and the older blob (with its own snapshot) was kept.
    let pref = PluginPref {
        params: vec![param(1, 0.9)],
        state: Some(PluginState::capture(&[7], &[param(1, 0.2), param(2, 0.4)])),
        ..PluginPref::default()
    };
    assert_eq!(
        Restore::from_pref(&pref).params(true),
        vec![(1, Some(0.9)), (2, None)]
    );
}

#[test]
fn sync_keeps_changed_values_and_ids_the_plugin_does_not_list() {
    let previous = [param(1, 0.8), param(9, 0.3)];
    let live = [view(1, 0.5, 0.5), view(2, 0.7, 0.1)];
    assert_eq!(
        merge_synced_params(&previous, &live),
        vec![param(2, 0.7), param(9, 0.3)]
    );
}

#[test]
fn a_port_change_is_visible_without_processing_a_block() {
    let mut slot = Slot::empty(PluginPref::default());
    slot.clap_host = Some(Arc::new(HostState::for_test(true)));
    assert!(slot.ports_changed());
    assert_eq!(slot.resolved_error().as_deref(), Some(AUDIO_PORTS_CHANGED));

    slot.clap_host = Some(Arc::new(HostState::for_test(false)));
    assert!(!slot.ports_changed());
    assert!(slot.resolved_error().is_none());
}
