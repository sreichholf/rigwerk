use std::sync::atomic::Ordering;

use super::CallbackFailure;
use crate::audio::{BufferMode, ClickRhythm, OutputMode};

const FOUR_FOUR: u8 = ClickRhythm::FourFour as u8;

/// A bridge like the BT-W3 case: 128-sample input, 480-sample output step.
fn shared_with_bridge_buffer(mode: BufferMode, fixed_ms: f32) -> super::Shared {
    let shared = super::Shared::new(48_000);
    shared.set_bridge_buffer(mode, fixed_ms);
    shared.set_source_rate(48_000);
    shared.set_target_frames(super::initial_target(128));
    shared.note_input_block(128);
    shared.note_output_block(480);
    shared
}

/// A caught callback panic reaches the screens as a lost run, with a message
/// the audio thread could not have built itself.
#[test]
fn a_failed_callback_publishes_a_message_and_a_lost_device() {
    let shared = super::Shared::new(48_000);
    assert_eq!(shared.callback_failure(), None);

    shared.note_callback_failure(CallbackFailure::Panic);

    let snapshot = shared.snapshot();
    assert_eq!(shared.callback_failure(), Some(CallbackFailure::Panic));
    assert_eq!(snapshot.callback_failure, Some(CallbackFailure::Panic));
    assert!(snapshot.device_lost);
    assert_eq!(
        snapshot.error.as_deref(),
        Some(CallbackFailure::Panic.message())
    );
    // A fresh run must not inherit the failure.
    shared.reset();
    let snapshot = shared.snapshot();
    assert!(!snapshot.device_lost);
    assert_eq!(snapshot.callback_failure, None);
    assert_eq!(snapshot.error, None);
    assert_eq!(shared.callback_failure(), None);
}

/// A block that does not divide into frames is not a plugin panic; the log
/// must say what actually went wrong.
#[test]
fn a_malformed_block_is_not_reported_as_a_panic() {
    let shared = super::Shared::new(48_000);

    shared.note_callback_failure(CallbackFailure::BadBlock);

    let snapshot = shared.snapshot();
    assert!(snapshot.device_lost);
    assert_eq!(snapshot.callback_failure, Some(CallbackFailure::BadBlock));
    assert_eq!(
        snapshot.error.as_deref(),
        Some(CallbackFailure::BadBlock.message())
    );
    assert_ne!(
        CallbackFailure::BadBlock.message(),
        CallbackFailure::Panic.message()
    );
    assert!(!CallbackFailure::BadBlock.message().contains("panicked"));
}

/// A later symptom does not relabel the cause that stopped the run.
#[test]
fn the_first_failure_is_the_one_shown() {
    let shared = super::Shared::new(48_000);
    shared.note_callback_failure(CallbackFailure::Panic);
    shared.note_callback_failure(CallbackFailure::BadBlock);

    assert_eq!(shared.callback_failure(), Some(CallbackFailure::Panic));
    assert_eq!(
        shared.snapshot().error.as_deref(),
        Some(CallbackFailure::Panic.message())
    );
}

/// An error a callback already published wins over the failure wording.
#[test]
fn an_existing_error_keeps_its_text() {
    let shared = super::Shared::new(48_000);
    shared.set_error("Output device disconnected: unplugged");
    shared.note_callback_failure(CallbackFailure::Panic);

    assert_eq!(
        shared.snapshot().error.as_deref(),
        Some("Output device disconnected: unplugged")
    );
}

/// A device failure stored first is not relabelled by a callback caught while
/// that device tears down: the device path owns the text and the row.
#[test]
fn a_device_failure_outranks_a_late_callback() {
    let shared = super::Shared::new(48_000);
    shared.set_device_lost("Output device disconnected: unplugged");
    shared.note_callback_failure(CallbackFailure::Panic);

    let snapshot = shared.snapshot();
    assert!(snapshot.device_lost);
    assert_eq!(snapshot.callback_failure, None);
    assert_eq!(
        snapshot.error.as_deref(),
        Some("Output device disconnected: unplugged")
    );
    // The record itself stays: it is what keeps a panicked callback out of
    // the chain while the run tears down.
    assert_eq!(shared.callback_failure(), Some(CallbackFailure::Panic));
}

/// The other order too: a callback caught first does not keep the device
/// flag from owning the report once a backend sees the device go.
#[test]
fn a_callback_before_a_device_flag_still_reports_the_device() {
    let shared = super::Shared::new(48_000);
    shared.note_callback_failure(CallbackFailure::Panic);
    shared.set_device_lost("Input device disconnected: unplugged");

    let snapshot = shared.snapshot();
    assert!(snapshot.device_lost);
    assert_eq!(snapshot.callback_failure, None);
    assert_eq!(
        snapshot.error.as_deref(),
        Some("Input device disconnected: unplugged")
    );
}

#[test]
fn a_different_device_in_an_extra_slot_bumps_its_generation() {
    let shared = super::Shared::new(48_000);
    let settings = [crate::audio::OutputDeviceSettings::default()];
    let mut plan = super::ExtraOutputPlan {
        count: 1,
        ..Default::default()
    };
    plan.slots[0].device_id = "hw:A".to_owned();
    plan.slots[0].node_name = "A".to_owned();
    shared.set_extra_outputs(&settings, &plan, &[]);
    let first = shared.extra_outputs[0].device_generation();

    shared.set_extra_outputs(&settings, &plan, &[]);
    assert_eq!(shared.extra_outputs[0].device_generation(), first);

    plan.slots[0].device_id = "hw:B".to_owned();
    plan.slots[0].node_name = "B".to_owned();
    shared.set_extra_outputs(&settings, &plan, &[]);
    assert_eq!(shared.extra_outputs[0].device_generation(), first + 1);
}

#[test]
fn scope_lane_keeps_peak_hold_bins_in_order() {
    let shared = super::Shared::new(48_000);
    shared.set_source_rate(48_000);
    let hop = ((48_000u64 * 250) / 1000 / super::SCOPE_LEN as u64).clamp(1, 512) as usize;
    // Quiet samples, then a louder one that must win the first closed bin.
    for _ in 0..hop - 1 {
        shared.push_frame(0.1, 0.1);
    }
    shared.push_frame(0.8, 0.8);
    for _ in 0..hop {
        shared.push_frame(-0.5, -0.5);
    }
    let mut input = vec![0.0; super::SCOPE_LEN];
    let mut output = vec![0.0; super::SCOPE_LEN];
    shared.copy_scope(&mut input, &mut output);
    assert!(
        (input[super::SCOPE_LEN - 2] - 0.8).abs() < 1.0e-5,
        "first closed bin should hold the louder sample, got {}",
        input[super::SCOPE_LEN - 2]
    );
    assert!(
        (input[super::SCOPE_LEN - 1] + 0.5).abs() < 1.0e-5,
        "second bin should keep sign of the peak, got {}",
        input[super::SCOPE_LEN - 1]
    );
    assert!(output.iter().all(|sample| *sample == 0.0));
}

#[test]
fn auto_bridge_buffer_keeps_two_output_blocks_and_grows_256() {
    let shared = shared_with_bridge_buffer(BufferMode::Auto, 10.0);
    assert_eq!(shared.target_frames(), 960);
    shared.raise_target_after_dropout();
    assert_eq!(shared.target_frames(), 1216);
    shared.relax_target();
    assert_eq!(shared.target_frames(), 1152);
}

#[test]
fn set_output_lifts_the_auto_bridge_buffer_for_a_large_exclusive_step() {
    // Bridge opens output before input has filled anything.  Exclusive may
    // wake with a 10 ms (or larger) `GetBufferSize`, so the bridge buffer
    // must already cover two of those steps or the first block drops out.
    let shared = super::Shared::new(48_000);
    shared.set_bridge_buffer(BufferMode::Auto, 10.0);
    shared.set_source_rate(48_000);
    shared.set_target_frames(super::initial_target(128));
    shared.note_input_block(128);
    assert_eq!(shared.target_frames(), 256);
    shared.set_output(48_000, 480, OutputMode::Exclusive, "Exclusive · test");
    assert_eq!(shared.target_frames(), 960);
    assert_eq!(shared.output_step_frames.load(Ordering::Relaxed), 480);
}

#[test]
fn a_requested_native_rate_is_not_a_reading_until_the_graph_reports() {
    let shared = super::Shared::new(48_000);
    // What the two-stream native path publishes before PipeWire negotiates.
    shared.set_requested_source_rate(44_100);
    shared.set_requested_output(
        44_100,
        128,
        OutputMode::Standard,
        "PipeWire node · 128 frames",
    );
    let snapshot = shared.snapshot();
    assert_eq!(snapshot.input_rate, 44_100);
    assert_eq!(snapshot.output_rate, 44_100);
    assert!(
        !snapshot.input_rate_reported,
        "the input rate was only asked for"
    );
    assert!(
        !snapshot.output_rate_reported,
        "the output rate was only asked for"
    );
    // The value and its line are published as before; only the provenance
    // says neither is a reading yet.
    assert_eq!(snapshot.output_step_frames, 128);
    assert!(!snapshot.output_step_reported);
    assert_eq!(snapshot.output_mode, "PipeWire node · 128 frames");
    assert_eq!(snapshot.opened_output_mode, Some(OutputMode::Standard));

    // A stream that opened at that rate publishes a reading.
    shared.set_source_rate(48_000);
    shared.set_output(48_000, 64, OutputMode::Standard, "test output");
    let snapshot = shared.snapshot();
    assert_eq!(snapshot.input_rate, 48_000);
    assert_eq!(snapshot.output_rate, 48_000);
    assert!(snapshot.input_rate_reported);
    assert!(snapshot.output_rate_reported);
}

#[test]
fn a_requested_publication_never_downgrades_a_reading() {
    let shared = super::Shared::new(48_000);
    shared.set_source_rate(48_000);
    shared.set_output(48_000, 64, OutputMode::Shared, "opened at 48 kHz");
    shared.note_output_step(64);

    // A request after the graph reported has nothing to add: it must not
    // store its own value or clear the reading.
    shared.set_requested_source_rate(44_100);
    shared.set_requested_output(44_100, 256, OutputMode::Standard, "asked for 44.1 kHz");
    let snapshot = shared.snapshot();
    assert_eq!(snapshot.input_rate, 48_000);
    assert_eq!(snapshot.output_rate, 48_000);
    assert_eq!(snapshot.output_step_frames, 64);
    assert_eq!(snapshot.output_mode, "opened at 48 kHz");
    assert_eq!(snapshot.opened_output_mode, Some(OutputMode::Shared));
    assert!(snapshot.input_rate_reported);
    assert!(snapshot.output_rate_reported);
    assert!(snapshot.output_step_reported);

    // The duplex pair follows the same rule.
    shared.note_direct_rate(48_000);
    shared.note_requested_direct_rate(44_100);
    let snapshot = shared.snapshot();
    assert_eq!(
        (snapshot.input_rate, snapshot.output_rate),
        (48_000, 48_000)
    );
    assert!(snapshot.input_rate_reported);
    assert!(snapshot.output_rate_reported);
}

#[test]
fn a_reported_step_alone_does_not_make_the_output_rate_a_reading() {
    let shared = super::Shared::new(48_000);
    shared.set_requested_output(
        48_000,
        128,
        OutputMode::Standard,
        "PipeWire node · 128 frames",
    );
    shared.note_output_step(64);
    let snapshot = shared.snapshot();
    assert_eq!(snapshot.output_step_frames, 64);
    assert!(snapshot.output_step_reported);
    assert!(
        !snapshot.output_rate_reported,
        "the step arrived before the stream's format handler read the rate"
    );

    // The stream's own format handler publishes the rate it negotiated.
    shared.set_output_rate(44_100);
    let snapshot = shared.snapshot();
    assert_eq!(snapshot.output_rate, 44_100);
    assert!(snapshot.output_rate_reported);
    assert!(snapshot.output_step_reported, "the step reading stays");
}

#[test]
fn the_first_graph_block_makes_the_requested_step_a_reading() {
    let shared = super::Shared::new(48_000);
    shared.set_requested_output(
        48_000,
        128,
        OutputMode::Standard,
        "PipeWire node · 128 frames",
    );
    assert!(!shared.snapshot().output_step_reported);
    // The playback node's process callback stores the step it really gets.
    shared.note_output_step(64);
    let snapshot = shared.snapshot();
    assert_eq!(snapshot.output_step_frames, 64);
    assert_eq!(snapshot.output_rate, 48_000);
    assert!(snapshot.output_step_reported);
    assert!(
        !snapshot.output_rate_reported,
        "the step is not a rate reading: the format handler publishes that"
    );
}

#[test]
fn a_requested_output_publication_does_not_replace_a_reported_step() {
    let shared = super::Shared::new(48_000);
    shared.set_requested_output(48_000, 128, OutputMode::Standard, "asked for 128 frames");
    shared.note_output_step(64);
    // The graph has delivered a step, so a later request has nothing to add
    // even though its rate was never read.  Dropping the step arm of the
    // guard would rewrite all of rate, step, mode and opened mode here.
    shared.set_requested_output(44_100, 256, OutputMode::LowLatency, "asked for 256 frames");
    let snapshot = shared.snapshot();
    assert_eq!(snapshot.output_rate, 48_000);
    assert_eq!(snapshot.output_step_frames, 64);
    assert_eq!(snapshot.output_mode, "asked for 128 frames");
    assert_eq!(snapshot.opened_output_mode, Some(OutputMode::Standard));
    assert!(!snapshot.output_rate_reported);
    assert!(snapshot.output_step_reported);
}

#[test]
fn the_duplex_node_publishes_its_graph_rate_as_a_reading() {
    let shared = super::Shared::new(48_000);
    shared.note_requested_direct_rate(44_100);
    let snapshot = shared.snapshot();
    assert_eq!(
        (snapshot.input_rate, snapshot.output_rate),
        (44_100, 44_100)
    );
    assert!(!snapshot.input_rate_reported);
    assert!(!snapshot.output_rate_reported);

    // The filter's first cycle publishes the graph's own rate.
    shared.note_direct_rate(48_000);
    let snapshot = shared.snapshot();
    assert_eq!(
        (snapshot.input_rate, snapshot.output_rate),
        (48_000, 48_000)
    );
    assert!(snapshot.input_rate_reported);
    assert!(snapshot.output_rate_reported);

    // A restart must not inherit the readings a previous run published.
    shared.reset();
    let snapshot = shared.snapshot();
    assert_eq!(snapshot.input_rate, 0);
    assert!(!snapshot.input_rate_reported);
    assert!(!snapshot.output_rate_reported);
}

#[test]
fn minimum_bridge_buffer_is_one_block_from_each_side() {
    let shared = shared_with_bridge_buffer(BufferMode::Minimum, 10.0);
    assert_eq!(shared.target_frames(), 608);
    shared.raise_target_after_dropout();
    assert_eq!(shared.target_frames(), 728);
    shared.relax_target();
    assert_eq!(shared.target_frames(), 696);
}

#[test]
fn fixed_bridge_buffer_stays_where_it_is_put() {
    let shared = shared_with_bridge_buffer(BufferMode::Fixed, 7.5);
    assert_eq!(shared.target_frames(), 360);
    shared.raise_target_after_dropout();
    assert_eq!(shared.target_frames(), 360);
    // Changing the size while running applies at once.
    shared.set_bridge_buffer(BufferMode::Fixed, 12.0);
    assert_eq!(shared.target_frames(), 576);
}

#[test]
fn switching_mode_while_running_applies_at_once() {
    let shared = shared_with_bridge_buffer(BufferMode::Auto, 10.0);
    shared.raise_target_after_dropout();
    shared.set_bridge_buffer(BufferMode::Minimum, 10.0);
    assert_eq!(shared.target_frames(), 608);
}

/// The CLAP main thread is process-wide.  These cases claim it for the
/// calling thread and must not overlap.  The thread id is restored
/// before the serial lock is released.
pub(crate) fn claim_main_thread() -> MainClaim {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let serial = SERIAL.lock().unwrap_or_else(|poison| poison.into_inner());
    MainClaim {
        _thread: crate::audio::plugins::claim_main_thread_for_test(),
        _serial: serial,
    }
}

pub(crate) struct MainClaim {
    // Dropped first, while `serial` is still held.
    _thread: crate::audio::plugins::MainThreadClaim,
    _serial: std::sync::MutexGuard<'static, ()>,
}

fn chain_rate(shared: &super::Shared) -> u32 {
    shared.paths[0].chain_rate()
}

fn built_rates(shared: &super::Shared) -> Vec<Option<u32>> {
    shared.paths[0]
        .chain
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .built_rates()
}

#[test]
fn publishing_output_rate_reloads_only_on_the_main_thread() {
    let _main = claim_main_thread();
    let shared = super::Shared::new(48_000);
    shared.restore_plugins(vec![crate::audio::PluginPref {
        path: "missing-rate-probe.vst3".to_owned(),
        ..crate::audio::PluginPref::default()
    }]);
    assert_eq!(chain_rate(&shared), 48_000);
    assert_eq!(built_rates(&shared), vec![None]);

    // Publishing the device rate does not itself rebuild the chain.
    shared.set_output(44_100, 128, OutputMode::Standard, "test output");
    assert_eq!(chain_rate(&shared), 48_000);
    assert_eq!(built_rates(&shared), vec![None]);

    std::thread::scope(|scope| {
        scope.spawn(|| shared.align_plugins_to_output_rate());
    });
    assert_eq!(
        built_rates(&shared),
        vec![None],
        "a bridge thread must not open the empty slot"
    );

    // The follow-up `service_plugins` calls, invoked directly.
    shared.align_plugins_to_output_rate();
    assert_eq!(chain_rate(&shared), 44_100);
    assert_eq!(built_rates(&shared), vec![Some(44_100)]);

    // A later device rate still waits for that follow-up, including the
    // one `service_plugins` runs each frame.
    shared.set_output(48_000, 128, OutputMode::Standard, "test output");
    assert_eq!(chain_rate(&shared), 44_100);
    assert_eq!(built_rates(&shared), vec![Some(44_100)]);
    shared.service_plugins();
    assert_eq!(chain_rate(&shared), 48_000);
    assert_eq!(built_rates(&shared), vec![Some(48_000)]);
}

#[test]
fn off_thread_drop_defers_plugin_slots_until_the_main_thread() {
    let _main = claim_main_thread();
    let plugins = crate::audio::plugins::tracked_slots_alive;

    // The UI thread drops the chain immediately.
    {
        let shared = super::Shared::new(48_000);
        let park = std::sync::Arc::clone(shared.plugin_park());
        shared.paths[0].edit_chain(|chain| chain.insert_tracked_slot_for_test());
        assert_eq!(plugins(), 1);
        drop(shared);
        assert_eq!(plugins(), 0, "the UI thread destroys the slot itself");
        assert_eq!(park.len(), 0, "nothing was deferred");
    }

    let shared = super::Shared::new(48_000);
    // The park outlives the run: the engine holds it in production, and a
    // bridge that drops the run hands its slots to it.
    let park = std::sync::Arc::clone(shared.plugin_park());
    shared.paths[0].edit_chain(|chain| chain.insert_tracked_slot_for_test());
    assert_eq!(plugins(), 1);
    std::thread::scope(|scope| {
        scope.spawn(move || drop(shared));
    });
    assert_eq!(
        plugins(),
        1,
        "the slot stays alive in the deferring bridge's park"
    );
    assert_eq!(park.len(), 1);
    park.drain();
    assert_eq!(plugins(), 0, "the main thread drops the deferred slot");
    assert_eq!(park.len(), 0);
}

fn missing(path: &str) -> crate::audio::PluginPref {
    crate::audio::PluginPref {
        path: path.to_owned(),
        name: path.to_owned(),
        ..crate::audio::PluginPref::default()
    }
}

fn names(shared: &super::Shared) -> Vec<String> {
    shared
        .plugin_views()
        .iter()
        .map(|view| view.name.clone())
        .collect()
}

/// Asserts that a publish happened since `last` and remembers it.
fn assert_published(shared: &super::Shared, last: &mut u64) {
    let version = shared.plugin_views_version();
    assert!(version > *last, "the edit did not republish the views");
    *last = version;
}

#[test]
fn published_views_follow_every_chain_edit() {
    let _main = claim_main_thread();
    let shared = super::Shared::new(48_000);
    assert!(shared.plugin_views().is_empty());
    let mut version = shared.plugin_views_version();

    shared.restore_plugins(vec![missing("a.vst3"), missing("b.vst3")]);
    assert_published(&shared, &mut version);
    assert_eq!(names(&shared), ["a.vst3", "b.vst3"]);

    shared.set_plugin_bypass(1, true);
    assert_published(&shared, &mut version);
    assert!(shared.plugin_views()[1].bypass);

    shared.set_plugin_mix(0, 0.25);
    assert_published(&shared, &mut version);
    assert_eq!(shared.plugin_views()[0].mix, 0.25);

    shared.move_plugin(0, 1);
    assert_published(&shared, &mut version);
    assert_eq!(names(&shared), ["b.vst3", "a.vst3"]);

    // Opening a slot whose file is missing leaves its error visible.
    shared.ensure_plugin_loaded(0);
    assert_published(&shared, &mut version);
    assert!(shared.plugin_views()[0].error.is_some());

    assert!(shared.set_plugin_parameter(1, 7, 0.5).is_err());
    assert_published(&shared, &mut version);
    assert!(shared.plugin_views()[1].error.is_some());

    shared.remove_plugin(0);
    assert_published(&shared, &mut version);
    assert_eq!(names(&shared), ["a.vst3"]);

    // A failed add stays in the list with its error.
    assert!(shared.add_plugin(missing("c.vst3")).is_err());
    assert_published(&shared, &mut version);
    assert_eq!(names(&shared), ["a.vst3", "c.vst3"]);
    assert!(shared.plugin_views()[1].error.is_some());

    // A snapshot the UI already holds is not changed by later edits.
    let held = shared.plugin_views();
    shared.restore_plugins(Vec::new());
    assert_eq!(held.len(), 2);
    assert!(shared.plugin_views().is_empty());
}

/// Holds the chain runtime on this thread, as the audio thread does, while
/// the UI-side calls run on a worker that stands in for the main thread.
/// None of them may wait for it: the worker must finish while it is still
/// held.
#[test]
fn drawing_and_quiet_service_frames_never_wait_for_the_audio_side() {
    let _main = claim_main_thread();
    let shared = super::Shared::new(48_000);
    shared.restore_plugins(vec![missing("a.vst3")]);
    shared.set_output(48_000, 128, OutputMode::Standard, "test output");
    // Opens the restored slot (it fails), so the chain is settled.
    shared.service_plugins();
    assert!(shared.plugin_views()[0].error.is_some());
    shared.paths[0].edit_chain(|chain| {
        chain.insert_clap_for_test(crate::audio::clap::tests::fake_plugin_for_test(false))
    });
    // What an open editor holds.
    let Some(editor) = shared.plugin_handle(1) else {
        panic!("the fake is loaded");
    };

    const HELD: std::time::Duration = std::time::Duration::from_secs(5);
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let chain = shared.paths[0].hold_chain_for_test();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let _worker_main = crate::audio::plugins::claim_main_thread_for_test();
            for _ in 0..100 {
                let views = shared.plugin_views();
                assert_eq!(views.len(), 2);
                // The open-editor poll: identity from the published view.
                assert_eq!(views[1].instance, Some(editor.instance_id()));
                shared.service_plugins();
                shared.align_plugins_to_output_rate();
            }
            done_tx.send(()).unwrap();
        });
        let finished = done_rx.recv_timeout(HELD);
        drop(chain);
        assert!(finished.is_ok(), "a UI-side call waited for the audio side");
    });
}

/// Structural edits are published at once while the audio side holds the
/// runtime.  The slot it lets go of is destroyed on the UI thread.
#[test]
fn structural_edits_never_wait_for_the_audio_side() {
    let _main = claim_main_thread();
    let alive = crate::audio::plugins::tracked_slots_alive;
    let shared = super::Shared::new(48_000);
    let path = &shared.paths[0];
    path.insert_delay_for_test(16);
    path.insert_delay_for_test(8);
    path.edit_chain(|chain| chain.insert_tracked_slot_for_test());
    assert_eq!(alive(), 1);
    let audio = path.hold_chain_for_test();

    const HELD: std::time::Duration = std::time::Duration::from_secs(5);
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let _worker_main = crate::audio::plugins::claim_main_thread_for_test();
            path.move_plugin(0, 1);
            path.remove_plugin(2);
            assert!(path.add_plugin(missing("c.vst3")).is_err());
            path.service_plugins();
            done_tx.send(()).unwrap();
        });
        let finished = done_rx.recv_timeout(HELD);
        assert!(
            finished.is_ok(),
            "a structural edit waited for the audio side"
        );
    });
    let latencies: Vec<u32> = path
        .plugin_views()
        .iter()
        .map(|view| view.latency)
        .collect();
    assert_eq!(
        latencies,
        [8, 16, 0],
        "published before the runtime applied it"
    );
    assert!(path.chain_commands_pending());
    assert_eq!(alive(), 1, "the removed slot is still in the runtime");

    // The audio side's next block applies the edits, then runs the chain.
    let mut seat = Some(audio);
    let mut left = vec![0.0f32; 64];
    let mut right = vec![0.0f32; 64];
    left[0] = 1.0;
    right[0] = 1.0;
    path.begin_chain(&mut seat);
    assert!(!path.chain_commands_pending());
    assert_eq!(path.latency_samples(), 24);
    assert!(path.run_plugins(&mut seat, &mut left, &mut right, 64));
    assert_eq!(left[24], 1.0);
    assert_eq!(alive(), 1, "the audio side destroys nothing");

    // Back on the UI thread, the next frame destroys what it handed back.
    drop(seat);
    path.service_plugins();
    assert_eq!(alive(), 0);
    path.restore_plugins(Vec::new());
}

/// Dragging an effect's mix, toggling its bypass and moving a plugin
/// control reach the plugin without the runtime, so they never wait for
/// the audio side: the worker finishes while this thread holds it.
#[test]
fn mix_bypass_and_controls_never_wait_for_the_audio_side() {
    let _main = claim_main_thread();
    let shared = super::Shared::new(48_000);
    let path = &shared.paths[0];
    path.edit_chain(|chain| {
        chain.insert_clap_for_test(crate::audio::clap::tests::fake_plugin_for_test(false))
    });
    const HELD: std::time::Duration = std::time::Duration::from_secs(5);
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let chain = path.hold_chain_for_test();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let _worker_main = crate::audio::plugins::claim_main_thread_for_test();
            for step in 0..100 {
                path.set_plugin_mix(0, step as f32 / 100.0);
                path.set_plugin_bypass(0, step % 2 == 0);
                // The fake has no controls: this fails, but at the
                // plugin.
                let _ = path.set_plugin_parameter(0, 7, 0.5);
                // Neither reading nor resetting the counters may wait for
                // the audio side.
                let _ = path.plugin_lock_report();
                path.reset_plugin_lock_counters();
            }
            path.set_plugin_mix(0, 0.25);
            path.set_plugin_bypass(0, true);
            done_tx.send(()).unwrap();
        });
        let finished = done_rx.recv_timeout(HELD);
        drop(chain);
        assert!(finished.is_ok(), "a mix, bypass or control change waited");
    });
    // The views show the change at once, and so does a save.
    let views = path.plugin_views();
    assert_eq!((views[0].mix, views[0].bypass), (0.25, true));
    let saved = &path.plugin_prefs()[0];
    assert_eq!((saved.mix, saved.bypass), (0.25, true));
    path.edit_chain(|chain| chain.restore(Vec::new()));
}

/// Capture, save, and editor sync go through the published links, so they
/// finish while this thread holds the runtime.  A capture off the plugin's
/// main thread is deferred and leaves the previous blob.
#[test]
fn capture_and_prefs_never_wait_for_the_audio_side() {
    use crate::audio::clap::tests::{fake_plugin_with_state_for_test, set_fake_state_for_test};
    let _main = claim_main_thread();
    let shared = super::Shared::new(48_000);
    let path = &shared.paths[0];
    const HELD: std::time::Duration = std::time::Duration::from_secs(5);
    let (loaded_tx, loaded_rx) = std::sync::mpsc::channel();
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let (released_tx, released_rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let worker = scope.spawn(move || {
            // The fake's state is per thread, and CLAP saves only on the
            // thread that loaded it: it is loaded, saved and destroyed here.
            let _worker_main = crate::audio::plugins::claim_main_thread_for_test();
            path.restore_plugins(vec![missing("a.vst3")]);
            path.edit_chain(|chain| {
                chain.insert_clap_for_test(fake_plugin_with_state_for_test(b"saved state"))
            });
            path.set_plugin_mix(1, 0.5);
            loaded_tx.send(()).unwrap();
            held_rx.recv().unwrap();
            let mut saved = Vec::new();
            for _ in 0..100 {
                path.mark_plugin_state_stale(1);
                path.capture_plugin_states();
                path.sync_plugin_parameters(1);
                saved = path.plugin_prefs();
            }
            let handle = path.plugin_handle(1).expect("the fake is loaded");
            let captured = !handle.wants_capture();
            done_tx.send(()).unwrap();

            // A capture off the plugin's main thread is deferred.
            set_fake_state_for_test(b"newer state");
            path.mark_plugin_state_stale(1);
            std::thread::scope(|scope| {
                scope.spawn(|| path.capture_plugin_states()).join().unwrap();
            });
            let deferred = (handle.wants_capture(), handle.saved().1);
            drop(handle);
            // Once the runtime is parked, so the removal applies here.
            released_rx.recv().unwrap();
            path.edit_chain(|chain| chain.restore(Vec::new()));
            (saved, captured, deferred)
        });
        loaded_rx.recv().unwrap();
        let chain = path.hold_chain_for_test();
        held_tx.send(()).unwrap();
        let finished = done_rx.recv_timeout(HELD);
        drop(chain);
        released_tx.send(()).unwrap();
        assert!(
            finished.is_ok(),
            "a capture or a save waited for the audio side"
        );
        let (saved, captured, (still_stale, kept)) = worker.join().unwrap();

        // The save is the whole chain, the unopened slot included.
        let names: Vec<&str> = saved.iter().map(|pref| pref.path.as_str()).collect();
        assert_eq!(names, ["a.vst3", ""]);
        assert_eq!(saved[1].mix, 0.5);
        let state = saved[1].state.as_ref().expect("the capture succeeded");
        assert_eq!(state.bytes().as_deref(), Some(&b"saved state"[..]));
        assert_eq!(
            state.params, saved[1].params,
            "the snapshot is the saved values"
        );
        assert!(captured, "a successful capture leaves nothing to capture");

        assert!(still_stale, "a deferred capture stays stale");
        assert_eq!(
            kept.as_ref().and_then(|state| state.bytes()).as_deref(),
            Some(&b"saved state"[..]),
            "a deferred capture keeps the previous blob"
        );
    });
}

/// A plugin that restarts with a new latency is republished by the next
/// service frame (views and the path's latency total) while this thread
/// holds the chain runtime.
#[test]
fn a_latency_change_republishes_without_the_audio_side() {
    use crate::audio::clap::tests::{
        fake_plugin_with_latency_for_test, restart_fake_with_latency_for_test,
    };
    let _main = claim_main_thread();
    let shared = super::Shared::new(48_000);
    let path = &shared.paths[0];
    const HELD: std::time::Duration = std::time::Duration::from_secs(5);
    let (loaded_tx, loaded_rx) = std::sync::mpsc::channel();
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let (released_tx, released_rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let worker = scope.spawn(move || {
            // The fake's state is per thread: it is loaded, restarted and
            // serviced here.
            let _worker_main = crate::audio::plugins::claim_main_thread_for_test();
            path.edit_chain(|chain| {
                chain.insert_clap_for_test(fake_plugin_with_latency_for_test(64))
            });
            assert_eq!(path.latency_samples(), 64);
            loaded_tx.send(()).unwrap();
            held_rx.recv().unwrap();
            // It has not processed, so the main thread restarts it at once.
            restart_fake_with_latency_for_test(8);
            path.service_plugins();
            let latency = (path.plugin_views()[0].latency, path.latency_samples());
            done_tx.send(()).unwrap();
            // Destroyed on the thread that loaded it, once the runtime is
            // parked.
            released_rx.recv().unwrap();
            path.edit_chain(|chain| chain.restore(Vec::new()));
            latency
        });
        loaded_rx.recv().unwrap();
        let chain = path.hold_chain_for_test();
        held_tx.send(()).unwrap();
        let finished = done_rx.recv_timeout(HELD);
        drop(chain);
        released_tx.send(()).unwrap();
        assert!(
            finished.is_ok(),
            "the latency republish waited for the audio side"
        );
        assert_eq!(worker.join().unwrap(), (8, 8));
    });
}

/// The UI sets a control, services, reads, and captures while `process`
/// still holds the chain.  None of that waits.  Released, the audio side
/// delivers the newest value, and the effect was not paused.
#[test]
fn clap_controls_never_wait_for_the_audio_side() {
    use crate::audio::clap::tests::{
        fake_plugin_with_params_for_test, fake_process_gate_for_test, fake_received_for_test,
    };
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    let _main = claim_main_thread();
    let shared = super::Shared::new(48_000);
    let path = &shared.paths[0];
    path.edit_chain(|chain| chain.insert_clap_for_test(fake_plugin_with_params_for_test()));
    let handle = path.plugin_handle(0).expect("the fake is loaded");
    let (hold, inside) = fake_process_gate_for_test();
    // The first block starts processing.
    let (mut left, mut right) = (vec![0.0f32; 64], vec![0.0f32; 64]);
    path.process_plugins(&mut left, &mut right, 64);
    assert!(handle.shared().pause.processing());

    const HELD: Duration = Duration::from_secs(5);
    const SETS: usize = 10_000;
    hold.store(true, Ordering::SeqCst);
    let stop = AtomicBool::new(false);
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let audio = scope.spawn(|| {
            let (mut left, mut right) = (vec![0.0f32; 64], vec![0.0f32; 64]);
            while !stop.load(Ordering::SeqCst) {
                path.process_plugins(&mut left, &mut right, 64);
            }
        });
        let since = Instant::now();
        while !inside.load(Ordering::SeqCst) {
            assert!(since.elapsed() < HELD, "the audio thread never got inside");
            std::thread::sleep(Duration::from_millis(1));
        }
        // Lets the block go if the UI hangs, so a failure does not hang.
        let watchdog = {
            let hold = std::sync::Arc::clone(&hold);
            scope.spawn(move || {
                let timed_out = done_rx.recv_timeout(HELD).is_err();
                hold.store(false, Ordering::SeqCst);
                timed_out
            })
        };
        for step in 0..SETS {
            let value = step as f64 / (SETS - 1) as f64;
            path.set_plugin_parameter(0, 7, value)
                .expect("set the gain");
            if step % 500 == 0 {
                path.service_plugins();
                path.capture_plugin_states();
                let _ = handle.sync_full();
                let _ = path.plugin_prefs();
            }
        }
        let still_held = inside.load(Ordering::SeqCst);
        done_tx.send(()).unwrap();
        assert!(
            !watchdog.join().unwrap(),
            "a UI call waited for the audio side"
        );
        assert!(still_held, "the block was released before the UI finished");
        // The block is released; the newest value reaches `process`.
        let since = Instant::now();
        while fake_received_for_test().last() != Some(&(7, 2.0, "process")) {
            assert!(since.elapsed() < HELD, "the last value never arrived");
            std::thread::sleep(Duration::from_millis(1));
        }
        stop.store(true, Ordering::SeqCst);
        audio.join().unwrap();
    });
    assert_eq!(handle.shared().pause.paused_blocks(), 0);
    assert_eq!(handle.failure(), None);
    assert_eq!(
        path.slot_links.lock().unwrap()[0]
            .controls
            .lock_stats()
            .skips,
        0
    );
    assert_eq!(path.plugin_views()[0].parameters[0].value, 1.0);
    drop(handle);
    path.edit_chain(|chain| chain.restore(Vec::new()));
}

#[test]
fn a_plugin_latency_change_republishes_the_views() {
    use crate::audio::clap::tests::{
        fake_plugin_with_latency_for_test, restart_fake_with_latency_for_test,
    };
    let _main = claim_main_thread();
    let shared = super::Shared::new(48_000);
    shared.paths[0]
        .edit_chain(|chain| chain.insert_clap_for_test(fake_plugin_with_latency_for_test(64)));
    assert_eq!(shared.plugin_views()[0].latency, 64);
    assert_eq!(shared.paths[0].latency_samples(), 64);
    // A quiet frame publishes nothing.
    let version = shared.paths[0].plugin_views_version();
    shared.service_plugins();
    assert_eq!(shared.paths[0].plugin_views_version(), version);

    // The plugin restarts with a new latency (it has not processed, so
    // the main thread restarts it right away).
    restart_fake_with_latency_for_test(8);
    shared.service_plugins();
    assert_eq!(shared.plugin_views()[0].latency, 8);
    assert_eq!(shared.paths[0].latency_samples(), 8);
}

#[test]
fn a_panicking_edit_clears_what_it_published() {
    let _main = claim_main_thread();
    let shared = super::Shared::new(48_000);
    shared.paths[0].edit_chain(|chain| {
        chain.insert_clap_for_test(crate::audio::clap::tests::fake_plugin_for_test(false))
    });
    assert_eq!(shared.plugin_views().len(), 1);
    assert_eq!(shared.paths[0].slot_links.lock().unwrap().len(), 1);
    assert!(shared.paths[0].has_plugins());

    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        shared.paths[0].edit_chain(|chain| {
            chain.set_bypass(0, true);
            panic!("an edit failed halfway");
        })
    }));
    assert!(panicked.is_err());
    assert!(shared.paths[0].chain.is_poisoned());
    assert!(shared.plugin_views().is_empty());
    assert!(shared.paths[0].slot_links.lock().unwrap().is_empty());
    assert!(!shared.paths[0].has_plugins());
    // A poisoned chain refuses further edits and publishes nothing new.
    shared.set_plugin_bypass(0, false);
    assert!(shared.plugin_views().is_empty());
}

#[test]
fn views_name_the_loaded_instance() {
    let _main = claim_main_thread();
    let shared = super::Shared::new(48_000);
    shared.restore_plugins(vec![missing("a.vst3")]);
    assert_eq!(shared.plugin_views()[0].instance, None);
    shared.paths[0].edit_chain(|chain| {
        chain.insert_clap_for_test(crate::audio::clap::tests::fake_plugin_for_test(false))
    });
    let Some(handle) = shared.plugin_handle(1) else {
        panic!("the fake is loaded");
    };
    assert_eq!(
        shared.plugin_views()[1].instance,
        Some(handle.instance_id())
    );
    // An editor still holding the old instance no longer matches once
    // the effect is replaced.
    shared.remove_plugin(1);
    shared.paths[0].edit_chain(|chain| {
        chain.insert_clap_for_test(crate::audio::clap::tests::fake_plugin_for_test(false))
    });
    assert_ne!(
        shared.plugin_views()[1].instance,
        Some(handle.instance_id())
    );
}

#[test]
fn a_processing_failure_is_published_on_the_next_service_frame() {
    let _main = claim_main_thread();
    let shared = super::Shared::new(48_000);
    shared.paths[0].edit_chain(|chain| {
        chain.insert_clap_for_test(crate::audio::clap::tests::fake_plugin_for_test(true))
    });
    assert!(shared.paths[0].has_plugins());
    assert!(shared.plugin_views()[0].error.is_none());

    let mut left = vec![0.5f32; 64];
    let mut right = vec![0.5f32; 64];
    let version = shared.plugin_views_version();
    shared.paths[0].process_plugins(&mut left, &mut right, 64);
    assert!(
        !shared.paths[0].has_plugins(),
        "the failed effect left the chain"
    );
    assert_eq!(
        shared.plugin_views_version(),
        version,
        "the audio thread must not rebuild the views"
    );
    assert!(shared.plugin_views()[0].error.is_none());

    shared.service_plugins();
    let error = shared.plugin_views()[0].error.clone();
    assert!(
        error
            .as_deref()
            .is_some_and(|text| text.starts_with("Processing failed")),
        "{error:?}"
    );
    // Nothing else changed, so the next frame does not publish again.
    let version = shared.plugin_views_version();
    shared.service_plugins();
    assert_eq!(shared.plugin_views_version(), version);
}

/// A CLAP port change is a reload request: the next service frame
/// rebuilds the effect, with no error, and its latency stays counted until
/// then.
#[test]
fn a_clap_port_change_reloads_the_effect_on_the_next_service_frame() {
    let _main = claim_main_thread();
    let loads = std::rc::Rc::new(std::cell::Cell::new(0));
    let counted = std::rc::Rc::clone(&loads);
    let _loader = crate::audio::plugins::set_test_loader(move |_pref, _rate| {
        counted.set(counted.get() + 1);
        Ok((
            crate::audio::clap::tests::fake_plugin_with_latency_for_test(32),
            false,
        ))
    });
    let shared = super::Shared::new(48_000);
    shared.paths[0]
        .add_plugin(crate::audio::PluginPref {
            path: format!("{}clap", crate::audio::plugins::TEST_PATH),
            format: crate::audio::PluginFormat::Clap,
            ..crate::audio::PluginPref::default()
        })
        .expect("load the fake");
    let old = shared
        .plugin_handle(0)
        .expect("the fake is a loaded CLAP slot");
    crate::audio::clap::tests::rescan_ports_for_test(&old);
    assert_eq!(shared.paths[0].latency_samples(), 32);

    shared.service_plugins();
    assert_eq!(loads.get(), 2, "reloaded once");
    let new = shared.plugin_handle(0).expect("reloaded");
    assert_ne!(new.instance_id(), old.instance_id());
    let view = shared.plugin_views()[0].clone();
    assert_eq!(view.error, None);
    assert_eq!(view.latency, 32);
    assert!(shared.paths[0].has_plugins());
}

/// A reload request in a frame that also has to follow a new device rate
/// loads the plugin once, at the new rate, not once per reason.
#[test]
fn a_reload_request_with_a_new_device_rate_pending_loads_once_at_that_rate() {
    let _main = claim_main_thread();
    let rates = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let seen = std::rc::Rc::clone(&rates);
    let _loader = crate::audio::plugins::set_test_loader(move |_pref, rate| {
        seen.borrow_mut().push(rate);
        Ok((
            crate::audio::clap::tests::fake_plugin_for_test(false),
            false,
        ))
    });
    let shared = super::Shared::new(48_000);
    shared.paths[0]
        .add_plugin(crate::audio::PluginPref {
            path: format!("{}clap", crate::audio::plugins::TEST_PATH),
            format: crate::audio::PluginFormat::Clap,
            ..crate::audio::PluginPref::default()
        })
        .expect("load the fake");
    assert_eq!(*rates.borrow(), vec![48_000]);
    shared.set_output(44_100, 128, OutputMode::Standard, "test output");
    // The plugin asks to be rebuilt (a layout change).
    shared
        .plugin_handle(0)
        .expect("loaded")
        .shared()
        .want_reload();
    shared.service_plugins();
    assert_eq!(
        *rates.borrow(),
        vec![48_000, 44_100],
        "one load, at the new rate"
    );
    assert_eq!(chain_rate(&shared), 44_100);
    shared.service_plugins();
    assert_eq!(rates.borrow().len(), 2);
}

#[test]
fn extra_outputs_are_pushed_live_and_cleared() {
    use super::{ExtraOutputLink, ExtraOutputPlan};
    use crate::audio::output_map::OutputDeviceSettings;

    let shared = super::Shared::new(48_000);
    let mut plan = ExtraOutputPlan {
        count: 2,
        ..ExtraOutputPlan::default()
    };
    plan.slots[0] = ExtraOutputLink {
        device_id: "ALSA:hw:CARD=1,DEV=0".to_owned(),
        node_name: "speakers".to_owned(),
        virtual_output: false,
        virtual_output_on: false,
    };
    plan.slots[1] = ExtraOutputLink {
        device_id: String::new(),
        node_name: String::new(),
        virtual_output: true,
        virtual_output_on: true,
    };
    shared.set_extra_outputs(
        &[
            OutputDeviceSettings {
                level_db: -6.0,
                mute: true,
                ..OutputDeviceSettings::default()
            },
            OutputDeviceSettings::default(),
        ],
        &plan,
        &[(BufferMode::Minimum, 10.0), (BufferMode::Auto, 10.0)],
    );
    assert_eq!(shared.extra_output_count(), 2);
    assert_eq!(shared.snapshot().extra_output_count, 2);
    let live = shared.extra_output_plan();
    assert_eq!(live.slots[0].node_name, "speakers");
    assert_eq!(live.slots[0].device_id, "ALSA:hw:CARD=1,DEV=0");
    assert!(live.slots[1].virtual_output && live.slots[1].virtual_output_on);
    assert_eq!(
        shared.extra_output_bus(0).bridge_buffer_mode(),
        BufferMode::Minimum
    );

    shared.set_extra_outputs(&[], &ExtraOutputPlan::default(), &[]);
    assert_eq!(shared.extra_output_count(), 0);
    assert!(shared.extra_output_plan().slots[0].node_name.is_empty());
}

#[test]
fn tap_returns_frames_in_order() {
    let shared = super::Shared::new(48_000);
    for frame in 0..1_000u32 {
        shared.tap_frame(frame as f32, -(frame as f32));
    }
    let mut out = vec![0.0f32; 64 * 2];
    assert_eq!(shared.copy_tap(&mut out, 64), 64);
    for frame in 0..64 {
        assert_eq!(out[frame * 2], (936 + frame) as f32);
        assert_eq!(out[frame * 2 + 1], -((936 + frame) as f32));
    }
}

#[test]
fn tap_wraps_and_keeps_order() {
    let shared = super::Shared::new(48_000);
    let capacity = super::TAP_CAPACITY_FRAMES;
    // Two full turns plus a little, so the newest window straddles the
    // wrap point.
    let written = capacity * 2 + 100;
    for frame in 0..written {
        shared.tap_frame(frame as f32, frame as f32 + 0.5);
    }
    let mut out = vec![0.0f32; 128 * 2];
    assert_eq!(shared.copy_tap(&mut out, 128), 128);
    for frame in 0..128 {
        let expected = (written - 128 + frame) as f32;
        assert_eq!(out[frame * 2], expected);
        assert_eq!(out[frame * 2 + 1], expected + 0.5);
    }
    // A whole ring is also a valid window and ends at the newest frame.
    let mut whole = vec![0.0f32; capacity * 2];
    assert_eq!(shared.copy_tap(&mut whole, capacity), capacity);
    assert_eq!(whole[(capacity - 1) * 2], (written - 1) as f32);
    assert_eq!(whole[0], (written - capacity) as f32);
}

#[test]
fn tap_needs_enough_frames() {
    let shared = super::Shared::new(48_000);
    let mut out = vec![0.0f32; 32 * 2];
    assert_eq!(shared.copy_tap(&mut out, 32), 0);
    for frame in 0..31 {
        shared.tap_frame(frame as f32, 0.0);
    }
    assert_eq!(shared.copy_tap(&mut out, 32), 0);
    shared.tap_frame(31.0, 0.0);
    assert_eq!(shared.copy_tap(&mut out, 32), 32);
    // A window taller than the output slice is refused rather than read
    // past its end.
    assert_eq!(shared.copy_tap(&mut out, 33), 0);
    // A zero-frame window has nothing to copy.
    assert_eq!(shared.copy_tap(&mut out, 0), 0);
    let mut tiny = [0.0f32; 2];
    // A slice too short for the window is refused too.
    assert_eq!(shared.copy_tap(&mut tiny, 32), 0);
    // A window taller than the ring is refused however much room the output
    // has.
    let mut oversized = vec![0.0f32; (super::TAP_CAPACITY_FRAMES + 1) * 2];
    assert_eq!(
        shared.copy_tap(&mut oversized, super::TAP_CAPACITY_FRAMES + 1),
        0
    );
    // A run reset empties the tap: nothing is read until the new
    // run has written a whole window again.
    shared.reset();
    assert_eq!(shared.copy_tap(&mut out, 32), 0);
}

#[test]
fn tap_does_not_touch_bridge_ring() {
    let shared = super::Shared::new(48_000);
    for frame in 0..100 {
        shared.push_frame(frame as f32, 0.0);
    }
    let head = shared.head.load(Ordering::Relaxed);
    let tail = shared.tail.load(Ordering::Relaxed);
    let frames = shared.ring_frames();
    for frame in 0..5_000 {
        shared.tap_frame(frame as f32, 0.0);
    }
    assert_eq!(shared.head.load(Ordering::Relaxed), head);
    assert_eq!(shared.tail.load(Ordering::Relaxed), tail);
    assert_eq!(shared.ring_frames(), frames);
    assert_eq!(shared.ring_frames(), 100);
    // The bridge ring still pops the frames it was given.
    assert_eq!(shared.pop_frame(), Some((0.0, 0.0)));
}

#[test]
fn a_new_run_keeps_record_the_click() {
    let previous = super::Shared::new(48_000);
    let mut next = super::Shared::new(48_000);
    next.adopt_settings_from(&previous);
    assert!(!next.recorder.with_click());
    previous.recorder.set_with_click(true);
    let mut next = super::Shared::new(48_000);
    next.adopt_settings_from(&previous);
    assert!(next.recorder.with_click());
}

#[test]
fn a_new_run_keeps_the_metronome_transport_but_not_the_beat() {
    let previous = super::Shared::new(48_000);
    previous.metronome_running.store(true, Ordering::Relaxed);
    previous
        .metronome_bpm
        .store(96.0f32.to_bits(), Ordering::Relaxed);
    previous.publish_metronome_beat(3, FOUR_FOUR);
    let mut next = super::Shared::new(48_000);
    next.adopt_settings_from(&previous);
    assert!(next.metronome_running.load(Ordering::Relaxed));
    assert_eq!(
        f32::from_bits(next.metronome_bpm.load(Ordering::Relaxed)),
        96.0
    );
    // 0 is the "—" the Live strip shows.
    assert_eq!(next.snapshot().metronome_beat, 0);
}

#[test]
fn a_bridge_error_clears_the_published_beat() {
    let shared = super::Shared::new(48_000);
    shared.publish_metronome_beat(2, FOUR_FOUR);
    shared.set_error("device lost");
    assert_eq!(shared.snapshot().metronome_beat, 0);
}

#[test]
fn a_beat_written_after_the_bridge_failed_is_not_shown() {
    // The callback can finish its block after `set_error` cleared the
    // beat, and store its beat again.
    let shared = super::Shared::new(48_000);
    shared.set_error("stream error");
    shared.publish_metronome_beat(3, FOUR_FOUR);
    assert_eq!(shared.snapshot().metronome_beat, 0);
    let lost = super::Shared::new(48_000);
    lost.set_device_lost("device lost");
    lost.publish_metronome_beat(2, FOUR_FOUR);
    assert_eq!(lost.snapshot().metronome_beat, 0);
    // A running bridge shows its beat.
    let running = super::Shared::new(48_000);
    running.running.store(true, Ordering::Relaxed);
    running.publish_metronome_beat(2, FOUR_FOUR);
    assert_eq!(running.snapshot().metronome_beat, 2);
}

#[test]
fn a_stopping_bridge_shows_no_beat() {
    let shared = super::Shared::new(48_000);
    shared.running.store(true, Ordering::Relaxed);
    shared.publish_metronome_beat(2, FOUR_FOUR);
    assert_eq!(shared.snapshot().metronome_beat, 2);
    // `AudioEngine::stop` clears `running` and leaves a lingering bridge
    // whose mixer can still publish until its teardown ends.
    shared.running.store(false, Ordering::Relaxed);
    shared.publish_metronome_beat(3, FOUR_FOUR);
    assert_eq!(shared.snapshot().metronome_beat, 0);
}

#[test]
fn the_published_beat_and_rhythm_round_trip_in_one_value() {
    for beat in [0u8, 1, 7, 12, u8::MAX] {
        for rhythm in [0u8, ClickRhythm::ThreeFour as u8, u8::MAX] {
            let packed = super::pack_metronome_beat(beat, rhythm);
            assert_eq!(super::unpack_metronome_beat(packed), (beat, rhythm));
        }
    }
    let shared = super::Shared::new(48_000);
    shared.running.store(true, Ordering::Relaxed);
    shared.publish_metronome_beat(3, ClickRhythm::SixEight as u8);
    assert_eq!(shared.metronome_beat(), (3, ClickRhythm::SixEight as u8));
    let snapshot = shared.snapshot();
    assert_eq!(snapshot.metronome_beat, 3);
    // Stopping clears the beat, not the bar it belonged to.
    shared.clear_metronome_beat();
    assert_eq!(shared.metronome_beat(), (0, ClickRhythm::SixEight as u8));
}

#[test]
fn the_opened_output_mode_is_published_with_its_line_and_cleared_on_reset() {
    let shared = super::Shared::new(48_000);
    assert_eq!(shared.snapshot().opened_output_mode, None);
    shared.set_output(
        48_000,
        144,
        OutputMode::Shared,
        "Standard output · Shared, smallest buffer refused: no client",
    );
    let snapshot = shared.snapshot();
    assert_eq!(snapshot.opened_output_mode, Some(OutputMode::Shared));
    assert!(snapshot.output_mode.contains("refused"));
    shared.set_output(48_000, 128, OutputMode::RealStyle, "REAL-style · 128 smp");
    assert_eq!(
        shared.snapshot().opened_output_mode,
        Some(OutputMode::RealStyle)
    );
    shared.reset();
    let snapshot = shared.snapshot();
    assert_eq!(snapshot.opened_output_mode, None);
    assert!(snapshot.output_mode.is_empty());
}

#[test]
fn every_output_mode_survives_its_stored_byte() {
    let modes = [
        OutputMode::Standard,
        OutputMode::LowLatency,
        OutputMode::Shared,
        OutputMode::RealStyle,
        OutputMode::Exclusive,
        OutputMode::Asio,
    ];
    for mode in modes {
        assert_ne!(mode.to_u8(), 0, "{mode:?}");
        assert_eq!(OutputMode::from_u8(mode.to_u8()), Some(mode));
    }
    assert_eq!(OutputMode::from_u8(0), None);
    assert_eq!(OutputMode::from_u8(200), None);
}

/// The duplex node has no mixer or resampler: its rate publishers stay
/// atomics-only and post no kernel for the clock mixer.
#[test]
fn the_duplex_rate_publishers_post_no_kernel() {
    let shared = super::Shared::new(48_000);
    shared.note_requested_direct_rate(44_100);
    shared.note_direct_rate(48_000);
    assert_eq!(shared.clock_kernel.pending_pair(), None);
}
