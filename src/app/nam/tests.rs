//! The NAM folder helpers, the loader thread, what a loaded capture
//! publishes, the block edits and what follows them, the park flush, and
//! the heavy-preset check over every path.

use super::{
    normalize_models_folder, paste_chord_edge, suggested_bypass, NamDone, NamInstall, NamJob,
    NamLoader,
};
use crate::app::chain::ChainSelection;
use crate::app::screen::Sheet;
use crate::app::{blank_app, RigWerkApp};
use crate::audio::nam::{prepare_nam_json, NamFile, NamLoad};
use crate::audio::{EngineState, NamPosition};

/// The tiny WaveNet from the nam-rs crate docs.
const TINY: &str = r#"{
    "version": "0.5.4", "architecture": "WaveNet",
    "config": {
        "layers": [{
            "input_size": 1, "condition_size": 1, "channels": 1, "head_size": 1,
            "kernel_size": 1, "dilations": [1], "activation": "ReLU",
            "gated": false, "head_bias": false
        }],
        "head": null, "head_scale": 1.0
    },
    "weights": [1.0, 2.0, 0.0, 0.0, 1.0, 0.0, 1.0, 1.0]
}"#;

/// Adds a block to `slot` that runs a capture measured at `share`.
fn heavy_block(app: &mut RigWerkApp, slot: usize, share: f32) -> u32 {
    let index = app.add_nam_block(slot).expect("a block");
    let block = &mut app.paths[slot].as_mut().unwrap().nam[index];
    block.model = format!("/captures/{share}.nam");
    block.name = format!("Amp {share}");
    block.load = Some(NamLoad {
        mean: share,
        peak: share,
    });
    block.controls.id
}

#[test]
fn a_loaded_capture_publishes_its_calibration_levels() {
    let mut app = blank_app();
    let slot = app.path_slots()[0];
    let index = app.add_nam_block(slot).expect("a block");
    let block = &mut app.paths[slot].as_mut().unwrap().nam[index];
    block.model = "/captures/amp.nam".to_owned();
    block.install = NamInstall {
        path: block.model.clone(),
        rate: 48_000,
        run: 0,
        generation: 1,
        submodel: 0,
        factor: 1,
    };
    let (id, stamp) = (block.controls.id, block.install.clone());
    let mut loaded = match prepare_nam_json(TINY, 48_000, 1.0) {
        NamFile::Ready(loaded) => loaded,
        _ => panic!("the docs model should load at 48 kHz"),
    };
    loaded.level_in_dbu = Some(12.0);
    loaded.level_out_dbu = Some(-3.0);
    app.apply_nam_result(NamDone {
        slot,
        id,
        stamp,
        file: NamFile::Ready(loaded),
    });
    // No other edit follows: the answer itself publishes the levels.
    let published = app.engine.path_nam(slot);
    let controls = published
        .get(published.index_of(id).expect("the block is published"))
        .unwrap();
    assert_eq!(controls.level_in_dbu, Some(12.0));
    assert_eq!(controls.level_out_dbu, Some(-3.0));
}

#[test]
fn the_suggested_set_is_the_heaviest_until_the_load_fits() {
    assert_eq!(suggested_bypass([0.5, 0.2], 0.6), 0, "already under");
    assert_eq!(suggested_bypass([0.3, 0.3, 0.25, 0.2], 1.05), 2);
    assert_eq!(suggested_bypass([0.5], 0.9), 1, "every block it has");
}

#[test]
fn a_heavy_preset_is_checked_over_every_path() {
    let mut app = blank_app();
    let first = app.path_slots()[0];
    let second = app.add_path().expect("a free slot");
    let a = heavy_block(&mut app, first, 0.3);
    let b = heavy_block(&mut app, second, 0.3);
    heavy_block(&mut app, second, 0.25);
    heavy_block(&mut app, first, 0.2);
    // Neither path is over on its own; the selected one is the light one.
    app.select_path(first);
    assert!(app.nam_load(first) < crate::app::issues::DSP_WARN_SHARE);
    app.check_nam_load();
    let warning = app
        .nam_warning
        .as_ref()
        .expect("the paths together are over");
    assert!((warning.load - 1.05).abs() < 1e-4);
    assert_eq!(warning.suggested, 2);
    let listed: Vec<(usize, u32)> = warning
        .blocks
        .iter()
        .take(warning.suggested)
        .map(|block| (block.slot, block.id))
        .collect();
    assert!(listed.contains(&(first, a)) && listed.contains(&(second, b)));

    app.bypass_suggested_nam();
    for (slot, id) in [(first, a), (second, b)] {
        let published = app.engine.path_nam(slot);
        let at = published.index_of(id).expect("a bypassed block stays");
        assert!(!published.get(at).unwrap().enabled, "bypassed and pushed");
    }
    assert!(
        app.nam_warning.is_none(),
        "the banner leaves once the paths fit"
    );
    assert!(app.nam_total_load() < crate::app::issues::DSP_WARN_SHARE);
}

#[test]
fn a_capture_is_measured_at_the_opened_output_step() {
    let mut app = blank_app();
    app.buffer_size = 64;
    app.output_buffer = 256;
    assert_eq!(app.nam_frames(), 256, "the request until a bridge runs");
    app.engine.state = EngineState::Running;
    app.snapshot.output_rate = 48_000;
    app.snapshot.output_step_frames = 480;
    assert_eq!(app.nam_frames(), 480, "what the driver opened");
}

#[test]
fn a_plain_folder_keeps_its_path() {
    assert_eq!(normalize_models_folder("  /amps/nam  "), "/amps/nam");
}

#[test]
fn copy_as_path_quotes_are_stripped() {
    assert_eq!(
        normalize_models_folder("\"D:\\Captures\\Nam\""),
        "D:\\Captures\\Nam"
    );
    assert_eq!(normalize_models_folder("'/amps/nam'"), "/amps/nam");
}

#[test]
fn a_windows_file_uri_becomes_a_drive_path() {
    assert_eq!(
        normalize_models_folder("file:///C:/Users/Ada/Captures"),
        "C:/Users/Ada/Captures"
    );
    assert_eq!(
        normalize_models_folder("file://localhost/C:/Captures"),
        "C:/Captures"
    );
}

#[test]
fn a_file_uri_decodes_spaces() {
    assert_eq!(
        normalize_models_folder("file:///home/ada/my%20amps"),
        "/home/ada/my amps"
    );
}

#[test]
fn ctrl_v_pastes_once_on_the_rising_edge() {
    let (fire, held) = paste_chord_edge(true, true, false);
    assert!(fire);
    assert!(held);
    let (fire, held) = paste_chord_edge(true, true, held);
    assert!(!fire);
    assert!(held);
    let (fire, held) = paste_chord_edge(true, false, held);
    assert!(!fire);
    assert!(!held);
}

#[test]
fn ctrl_v_while_inactive_does_not_paste_when_the_field_is_entered() {
    let (fire, held) = paste_chord_edge(false, true, false);
    assert!(!fire);
    let (fire, _) = paste_chord_edge(true, true, held);
    assert!(!fire);
}

#[test]
fn a_nam_file_uses_its_folder() {
    let dir = std::env::temp_dir().join(format!("rigwerk-nam-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp folder");
    let file = dir.join("amp.nam");
    std::fs::write(&file, b"x").expect("temp capture");
    let got = normalize_models_folder(&file.to_string_lossy());
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(got, dir.to_string_lossy());
}

#[test]
fn the_loader_answers_a_job_off_the_ui_thread() {
    let loader = NamLoader::spawn().expect("the loader thread starts");
    let stamp = NamInstall {
        path: "/nonexistent/capture.nam".to_owned(),
        rate: 48_000,
        run: 1,
        generation: 1,
        submodel: 0,
        factor: 1,
    };
    assert!(loader.request(NamJob {
        slot: 2,
        id: 7,
        stamp: stamp.clone(),
        path: stamp.path.clone(),
        rate: 48_000,
        slim: 1.0,
        factor: 1,
        frames: 256,
    }));
    let answer = loader
        .done
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the loader answers");
    assert_eq!((answer.slot, answer.id), (2, 7));
    assert_eq!(answer.stamp, stamp);
    assert!(
        matches!(answer.file, NamFile::Failed(_)),
        "a missing file is a failure, not a panic"
    );
}

#[test]
fn the_install_stamp_separates_rate_run_and_width() {
    let base = NamInstall {
        path: "a.nam".to_owned(),
        rate: 48_000,
        run: 3,
        generation: 1,
        submodel: 0,
        factor: 1,
    };
    let mut other = base.clone();
    assert_eq!(other, base);
    other.rate = 44_100;
    assert_ne!(other, base);
    let mut other = base.clone();
    other.run = 4;
    assert_ne!(other, base);
    let mut other = base.clone();
    other.submodel = 2;
    assert_ne!(other, base);
    let mut other = base.clone();
    other.factor = 2;
    assert_ne!(other, base, "a factor rebuild is a different install");
}

#[test]
fn the_engine_drops_parked_models_when_the_ui_flushes() {
    let app = blank_app();
    let NamFile::Ready(loaded) = prepare_nam_json(TINY, 48_000, 1.0) else {
        panic!("the docs model should load at 48 kHz");
    };
    let park = std::sync::Arc::clone(app.engine.shared.nam_park());
    park.retire(loaded.prepared.model);
    assert_eq!(park.len(), 1);
    app.engine.flush_parks();
    assert_eq!(park.len(), 0, "the frame's flush makes room again");
}

/// The ids of `slot`'s blocks, in stack order.
fn ids(app: &RigWerkApp, slot: usize) -> Vec<u32> {
    app.paths[slot].as_ref().unwrap().nam_ids()
}

/// An app whose first path has `count` blocks before the effects, and
/// that path's slot.
fn blocks(count: usize) -> (RigWerkApp, usize) {
    let mut app = blank_app();
    let slot = app.path_slots()[0];
    for _ in 0..count {
        app.add_nam_block(slot).expect("a block");
    }
    (app, slot)
}

fn nam_sheet(slot: usize, index: usize) -> Option<Sheet> {
    Some(Sheet::Nam { path: slot, index })
}

#[test]
fn a_drag_moves_a_block_the_whole_way_and_names_its_new_index() {
    let (mut app, slot) = blocks(4);
    let [a, b, c, d] = ids(&app, slot)[..] else {
        panic!("four blocks");
    };
    assert_eq!(app.move_nam_block(slot, 0, 3), Some(3));
    assert_eq!(ids(&app, slot), [b, c, d, a]);
    let published: Vec<u32> = app
        .engine
        .path_nam(slot)
        .blocks()
        .iter()
        .map(|block| block.id)
        .collect();
    assert_eq!(published, [b, c, d, a]);
    assert_eq!(app.move_nam_block(slot, 3, -2), Some(1));
    assert_eq!(ids(&app, slot), [b, a, c, d]);
}

#[test]
fn a_move_at_the_group_edge_keeps_the_sheet_on_its_block() {
    let (mut app, slot) = blocks(2);
    let [a, b] = ids(&app, slot)[..] else {
        panic!("two blocks");
    };
    app.open_sheet(Sheet::Nam {
        path: slot,
        index: 0,
    });
    assert_eq!(app.move_nam_block(slot, 0, -1), None, "already first");
    assert_eq!(app.chrome.sheet, nam_sheet(slot, 0));
    assert_eq!(app.move_nam_block(slot, 0, 1), Some(1));
    assert_eq!(ids(&app, slot), [b, a]);
    assert_eq!(
        app.chrome.sheet,
        nam_sheet(slot, 1),
        "the sheet follows the block"
    );
    assert_eq!(app.chain_selected, ChainSelection::Nam(1));
}

#[test]
fn a_block_that_changes_sides_keeps_its_sheet() {
    let (mut app, slot) = blocks(3);
    let first = ids(&app, slot)[0];
    app.open_sheet(Sheet::Nam {
        path: slot,
        index: 0,
    });
    let mut controls = app.paths[slot].as_ref().unwrap().nam[0].controls;
    controls.position = NamPosition::Post;
    assert_eq!(app.set_nam_controls(slot, 0, controls), Some(2));
    assert_eq!(ids(&app, slot)[2], first);
    assert_eq!(app.chrome.sheet, nam_sheet(slot, 2));
    assert_eq!(app.chain_selected, ChainSelection::Nam(2));
}

#[test]
fn removing_a_block_keeps_the_sheet_on_what_it_showed() {
    let (mut app, slot) = blocks(3);
    app.open_sheet(Sheet::Nam {
        path: slot,
        index: 2,
    });
    assert!(app.remove_nam_block(slot, 0), "a block before the open one");
    assert_eq!(app.chrome.sheet, nam_sheet(slot, 1));
    assert_eq!(app.chain_selected, ChainSelection::Nam(1));
    assert!(app.remove_nam_block(slot, 1), "the open block itself");
    assert_eq!(app.chrome.sheet, nam_sheet(slot, 0), "the block before it");
    assert_eq!(app.chain_selected, ChainSelection::Nam(0));
    assert!(app.remove_nam_block(slot, 0), "the last block");
    assert_eq!(
        app.chrome.sheet,
        nam_sheet(slot, 0),
        "the way in to an empty stack"
    );
    assert_eq!(app.chain_selected, ChainSelection::PreEq);
}

#[test]
fn a_failed_capture_does_not_count_as_running() {
    let mut app = blank_app();
    let slot = app.path_slots()[0];
    heavy_block(&mut app, slot, 0.3);
    let path = app.paths[slot].as_mut().unwrap();
    assert!(path.nam_active());
    path.nam[0].load = None;
    path.nam[0].error = Some("not a capture".to_owned());
    assert!(!path.nam_active());
    assert_eq!(app.nam_load(slot), 0.0);
    assert!(app.nam_heaviest(slot).is_empty());
}
