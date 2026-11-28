use std::sync::atomic::Ordering;

use super::super::output::write_frame;
use super::super::output_map::OutputDeviceSettings;
use super::super::rt_guard::RtGuard;
use super::super::shared::{ExtraOutputLink, ExtraOutputPlan, Shared};
use super::super::types::BufferMode;
use super::mixer::ExtraOutputMixer;

/// A shared state with one generic extra output slot, its ring laid out for a
/// `channels`-channel device and a fixed bridge buffer of `bridge_buffer_ms`.
fn one_extra_output(channels: usize, bridge_buffer_ms: f32) -> Shared {
    one_extra_output_at(48_000, channels, bridge_buffer_ms)
}

/// [`one_extra_output`] with everything at `rate`.
fn one_extra_output_at(rate: u32, channels: usize, bridge_buffer_ms: f32) -> Shared {
    let shared = Shared::new(rate);
    shared.set_output(rate, 64, crate::audio::OutputMode::Standard, "test");
    shared.drift_enabled.store(false, Ordering::Relaxed);
    let mut plan = ExtraOutputPlan {
        count: 1,
        ..ExtraOutputPlan::default()
    };
    plan.slots[0] = ExtraOutputLink {
        device_id: "test:extra-output".to_owned(),
        ..ExtraOutputLink::default()
    };
    shared.set_extra_outputs(
        &[OutputDeviceSettings::default()],
        &plan,
        &[(BufferMode::Fixed, bridge_buffer_ms)],
    );
    let bus = shared.extra_output_bus(0);
    bus.set_device_channels(channels);
    bus.reset_playback();
    bus.set_bridge_buffer(BufferMode::Fixed, bridge_buffer_ms);
    bus.set_source_rate(rate);
    shared
        .extra_output_ring_active(0)
        .store(true, Ordering::Relaxed);
    shared
}

#[test]
fn extra_output_mixer_plays_the_device_frames_from_its_ring() {
    let shared = one_extra_output(2, 100.0);
    let bus = shared.extra_output_bus(0);
    bus.set_target_frames(64);
    for _ in 0..5_000 {
        bus.push_lanes(&[0.5, -0.25]);
    }

    let mut mixer = ExtraOutputMixer::new(0, 48_000, 2);
    mixer.publish_channels(&shared);
    assert_eq!(shared.extra_outputs[0].channels(), 2);
    let mut out = vec![[0.0f32; 2]; 64];
    mixer.render(&shared, 64, |index, mapped| {
        write_frame(&mut out[index], mapped);
    });
    // After priming through a deep bridge buffer, the ring's frames play as is:
    // the clock side already mapped them.
    assert_eq!(*out.last().unwrap(), [0.5, -0.25]);
}

/// Renders one extra output block into a buffer the caller owns, and asserts
/// that the block allocated and freed nothing.  The buffer is outside the
/// guard, as in `audio/mixer/rt_tests.rs`: writing a test's own output is not
/// the audio thread's allocation.  A release build counts nothing, so the two
/// reads are 0 == 0 there: the zeros are evidence in a debug test run.
fn render_one_block(
    mixer: &mut ExtraOutputMixer,
    shared: &Shared,
    frames: usize,
    out: &mut [Vec<f32>],
) {
    let guard = RtGuard::enter();
    mixer.render(shared, frames, |index, mapped| {
        for (channel, sample) in out[index].iter_mut().enumerate() {
            *sample = mapped.sample(channel);
        }
    });
    assert_eq!(
        guard.allocations(),
        0,
        "the block allocated on the audio thread"
    );
    assert_eq!(guard.frees(), 0, "the block freed on the audio thread");
    drop(guard);
}

#[test]
fn an_extra_output_block_allocates_and_frees_nothing() {
    let frames = 128;
    let shared = one_extra_output(2, 100.0);
    // The drift loop steers every block; the rate switch below is planned by
    // the publisher, and the counted block only adopts the table.
    shared.drift_enabled.store(true, Ordering::Relaxed);
    let bus = shared.extra_output_bus(0);
    // Enough to leave priming on the first block and run dry later, so the
    // dropout fade is counted too.
    for _ in 0..bus.target_frames() + 6 * frames {
        bus.push_lanes(&[0.25, -0.25]);
    }
    let mut mixer = ExtraOutputMixer::new(0, 48_000, 2);
    let mut out = vec![vec![0.0f32; 2]; frames];
    render_one_block(&mut mixer, &shared, frames, &mut out);
    assert!(
        out.iter().any(|frame| frame[0] > 0.0),
        "the block played the prepared ring"
    );
    assert_eq!(
        bus.source_rate.load(Ordering::Relaxed),
        48_000,
        "no rate change yet"
    );
    // The stream published the device rate it opened at, and the clock output
    // redraws at 96 kHz: the publisher plans the kernel, and the next counted
    // block only adopts it.
    bus.set_playback_rate(48_000);
    shared.set_output_rate(96_000);
    render_one_block(&mut mixer, &shared, frames, &mut out);
    assert_eq!(
        bus.source_rate.load(Ordering::Relaxed),
        96_000,
        "the counted block took the new clock rate"
    );
    // 96 kHz source into a 48 kHz device burns two ring frames per output
    // frame, so the ring runs dry well inside this loop.
    for _ in 0..40 {
        render_one_block(&mut mixer, &shared, frames, &mut out);
    }
    assert!(
        bus.dropouts.load(Ordering::Relaxed) >= 1,
        "the blocks ran the ring dry"
    );
}

#[test]
fn an_extra_output_dropout_bump_allocates_and_frees_nothing() {
    let frames = 128;
    let shared = one_extra_output(2, 10.0);
    let bus = shared.extra_output_bus(0);
    // Auto raises the target after a dropout; that bump has to run inside the
    // counted block like the rest of the block does.
    bus.set_bridge_buffer(BufferMode::Auto, 10.0);
    let target = bus.target_frames();
    for _ in 0..target + 4 * frames {
        bus.push_lanes(&[0.25, -0.25]);
    }
    let mut mixer = ExtraOutputMixer::new(0, 48_000, 2);
    let mut out = vec![vec![0.0f32; 2]; frames];
    let mut blocks = 0;
    while bus.dropouts.load(Ordering::Relaxed) == 0 {
        render_one_block(&mut mixer, &shared, frames, &mut out);
        blocks += 1;
        assert!(blocks < 16, "the ring ran dry");
    }
    assert_eq!(
        bus.target_frames(),
        target + 256,
        "the dropout bump ran inside a counted block"
    );
}

#[test]
fn every_lane_of_a_multichannel_extra_output_plays() {
    let shared = one_extra_output(6, 100.0);
    let bus = shared.extra_output_bus(0);
    assert_eq!(bus.ring.lanes(), 6);
    let frame = [0.1, 0.2, 0.3, 0.4, 0.5, 0.6];
    for _ in 0..5_000 {
        bus.push_lanes(&frame);
    }
    let mut mixer = ExtraOutputMixer::new(0, 48_000, 6);
    let mut out = vec![[0.0f32; 6]; 64];
    mixer.render(&shared, 64, |index, mapped| {
        write_frame(&mut out[index], mapped);
    });
    assert_eq!(*out.last().unwrap(), frame);
}

#[test]
fn a_extra_output_dropout_does_not_stop_the_clock() {
    let shared = one_extra_output(2, 1.0);
    shared.running.store(true, Ordering::Relaxed);
    let bus = shared.extra_output_bus(0);
    // 1 ms at 48 kHz → 48-frame bridge buffer. Leave priming, then run dry.
    for _ in 0..48 {
        bus.push_lanes(&[0.4, 0.4]);
    }

    let mut mixer = ExtraOutputMixer::new(0, 48_000, 2);
    mixer.render(&shared, 128, |_index, _mapped| {});
    assert!(
        bus.dropouts.load(Ordering::Relaxed) >= 1,
        "empty extra output ring should count a dropout"
    );
    assert!(
        shared.running.load(Ordering::Relaxed),
        "extra output dropout must not stop the bridge"
    );
    assert_eq!(shared.dropouts.load(Ordering::Relaxed), 0);
    let snapshot = shared.snapshot();
    assert!(snapshot.extra_output_dropouts[0] >= 1, "per extra output");
    assert_eq!(snapshot.dropouts, 0, "not the clock output's");
}

#[test]
fn a_missing_extra_output_shows_in_the_snapshot_until_it_reopens() {
    let shared = one_extra_output(2, 10.0);
    let bus = shared.extra_output_bus(0);
    bus.missing.store(true, Ordering::Relaxed);
    assert_eq!(
        shared.snapshot().extra_output_missing,
        [true, false, false, false]
    );
    bus.reset_playback();
    assert!(!shared.snapshot().extra_output_missing[0]);
}

#[test]
fn a_ring_held_far_above_its_bridge_buffer_pins_the_drift_loop() {
    let shared = one_extra_output(2, 200.0);
    shared.drift_enabled.store(true, Ordering::Relaxed);
    let bus = shared.extra_output_bus(0);
    let target = bus.target_frames();
    assert_eq!(target, 9_600);
    // Just under the purge line: the device seems to run slow, far more
    // than the loop may correct.
    let held = target * 2;
    let mut mixer = ExtraOutputMixer::new(0, 48_000, 2);
    let mut pinned_after = None;
    for block in 0..400 {
        while bus.ring_frames() < held {
            bus.push_lanes(&[0.1, 0.1]);
        }
        mixer.render(&shared, 1_024, |_, _| {});
        if bus.drift_at_limit.load(Ordering::Relaxed) {
            pinned_after = Some(block);
            break;
        }
    }
    assert!(pinned_after.is_some(), "the integral reaches its limit");
    assert!(shared.snapshot().extra_output_drift_at_limit[0]);
    bus.reset_playback();
    assert!(
        !shared.snapshot().extra_output_drift_at_limit[0],
        "a reopen clears it"
    );
}

#[test]
fn closing_a_large_bridge_buffer_step_does_not_flag_the_drift_loop() {
    // 8 kHz and 256-frame blocks (32 ms): the loop's seconds, fewer frames.
    let shared = one_extra_output_at(8_000, 2, 200.0);
    shared.drift_enabled.store(true, Ordering::Relaxed);
    let bus = shared.extra_output_bus(0);
    let mut mixer = ExtraOutputMixer::new(0, 8_000, 2);
    let mut block = |pushed: usize| {
        for _ in 0..pushed {
            bus.push_lanes(&[0.1, 0.1]);
        }
        mixer.render(&shared, 256, |_, _| {});
        bus.drift_at_limit.load(Ordering::Relaxed)
    };
    while bus.ring_frames() < bus.target_frames() {
        bus.push_lanes(&[0.1, 0.1]);
    }
    // The clock side feeds exactly what the device takes: no clock error.
    for _ in 0..100 {
        assert!(!block(256));
    }
    // 30 ms less bridge buffer: far more ring error than the proportional term
    // alone corrects at the limit, so the integral winds up while the loop
    // drains it (about a minute), with nothing wrong with the clock.
    bus.set_bridge_buffer(BufferMode::Fixed, 170.0);
    let mut closed = None;
    for index in 0..3_000 {
        assert!(!block(256), "flagged {index} blocks after the step");
        if bus.ring_frames() < bus.target_frames() {
            closed.get_or_insert(index);
        }
        if closed.is_some_and(|closed| index > closed + 100) {
            break;
        }
    }
    assert!(closed.is_some(), "the loop closed the step");
    assert_eq!(bus.dropouts.load(Ordering::Relaxed), 0);

    // A device that really takes about 2000 ppm more than it is fed still
    // pins it.
    let pinned = (0..3_000).any(|index| block(if index % 2 == 0 { 255 } else { 256 }));
    assert!(pinned, "a real clock error still flags");
}

/// A wide extra output with 8192-frame blocks at `rate`: the Auto bridge buffer is
/// not clamped below its floor, and the excess purge still fires.
fn a_wide_ring_holds_an_auto_bridge_buffer(rate: u32, channels: usize) {
    let shared = Shared::new(rate);
    shared.set_output(rate, 64, crate::audio::OutputMode::Standard, "test");
    shared.drift_enabled.store(false, Ordering::Relaxed);
    let bus = shared.extra_output_bus(0);
    bus.set_device_channels(channels);
    bus.reset_playback();
    bus.set_bridge_buffer(BufferMode::Auto, 10.0);
    bus.set_source_rate(rate);
    // 8192-frame extra output blocks: the Auto floor is two of them.
    bus.note_output_block(8_192);
    assert_eq!(bus.floor_frames(), 16_384);
    assert_eq!(bus.target_frames(), 16_384, "not clamped below the floor");
    let purge_line = bus.target_frames() * 2 + 8_192;
    assert!(purge_line < bus.ring.capacity_frames());

    // A ring filled past the purge line is trimmed back to the target.
    shared
        .extra_output_ring_active(0)
        .store(true, Ordering::Relaxed);
    let frame = vec![0.25f32; channels];
    for _ in 0..purge_line + 20_000 {
        bus.push_lanes(&frame);
    }
    assert_eq!(bus.overruns.load(Ordering::Relaxed), 0);
    let mut mixer = ExtraOutputMixer::new(0, rate, channels);
    mixer.render(&shared, 8_192, |_, _| {});
    let before = bus.ring_frames();
    mixer.render(&shared, 8_192, |_, _| {});
    assert!(
        bus.ring_frames() <= bus.target_frames(),
        "purged from {before} to {}",
        bus.ring_frames()
    );
}

#[test]
fn eight_and_more_channels_hold_an_auto_bridge_buffer() {
    a_wide_ring_holds_an_auto_bridge_buffer(48_000, 8);
    a_wide_ring_holds_an_auto_bridge_buffer(48_000, 12);
    a_wide_ring_holds_an_auto_bridge_buffer(192_000, 8);
}

#[test]
fn a_relaid_ring_takes_no_frames_from_an_older_block() {
    let shared = one_extra_output(2, 10.0);
    let bus = shared.extra_output_bus(0);
    let generation = bus.generation();
    let frame = super::super::output_map::DeviceFrame::from_lanes(&[0.5, 0.5], 2);
    shared.push_extra_output_frame(0, generation, &frame);
    assert_eq!(bus.ring_frames(), 1);
    // The stream reopened on another device mid-block.
    bus.set_device_channels(4);
    bus.reset_playback();
    shared.push_extra_output_frame(0, generation, &frame);
    assert_eq!(bus.ring_frames(), 0);
    shared.push_extra_output_frame(0, bus.generation(), &frame);
    assert_eq!(bus.ring_frames(), 1);
}

/// Pushes `frames` of a frame whose played lanes are 0.75 and click-free
/// lanes 0.25 through the clock side's entry point, then plays the
/// extra output at `device_rate` until it has run dry and faded out.  Returns
/// each played frame's `(sample, record_sample)` on every channel.
fn played_and_recorded(
    channels: usize,
    device_rate: u32,
    frames: usize,
) -> (Shared, Vec<Vec<(f32, f32)>>) {
    let shared = one_extra_output_at(44_100, channels, 1.0);
    shared
        .extra_output_ring_active(0)
        .store(true, Ordering::Relaxed);
    let bus = shared.extra_output_bus(0);
    let frame = super::super::output_map::DeviceFrame::from_both_lanes(
        &vec![0.75; channels],
        &vec![0.25; channels],
        channels,
    );
    for _ in 0..frames {
        shared.push_extra_output_frame(0, bus.generation(), &frame);
    }
    let mut mixer = ExtraOutputMixer::new(0, device_rate, channels);
    mixer.configure_source_rate(&shared);
    assert_eq!(mixer.resampler_pair(), (44_100, device_rate));
    let mut out = Vec::new();
    mixer.render(&shared, frames + 512, |_, mapped| {
        out.push(
            (0..channels)
                .map(|channel| (mapped.sample(channel), mapped.record_sample(channel)))
                .collect(),
        );
    });
    (shared, out)
}

#[test]
fn a_narrow_extra_output_carries_the_click_free_lanes_through_its_ring() {
    for channels in [1, 2, 3, 4] {
        let (shared, out) = played_and_recorded(channels, 48_000, 2_000);
        assert_eq!(shared.extra_output_bus(0).ring.lanes(), channels * 2);
        // Through the resampler (44.1 → 48 kHz) and the dropout fade, the
        // click-free half keeps its own value under the same gain.
        let mut faded = false;
        let mut full = false;
        for frame in &out {
            for &(wet, record) in frame {
                assert!(
                    (wet - 3.0 * record).abs() < 1.0e-5,
                    "{channels} ch: {wet} vs {record}"
                );
                full |= (record - 0.25).abs() < 1.0e-5;
                faded |= record > 0.0 && record < 0.24;
            }
        }
        assert!(full, "{channels} ch: the click-free frame plays");
        assert!(faded, "{channels} ch: the fade reaches it");
    }
}

#[test]
fn a_wide_extra_output_records_the_played_lanes() {
    for channels in [5, 8, 9, 10] {
        let (shared, out) = played_and_recorded(channels, 44_100, 2_000);
        assert_eq!(
            shared.extra_output_bus(0).ring.lanes(),
            super::super::output_map::frame_lanes(channels)
        );
        let played = out
            .iter()
            .flatten()
            .filter(|(wet, _)| (*wet - 0.75).abs() < 1.0e-6)
            .count();
        assert!(played > 0, "{channels} ch");
        for &(wet, record) in out.iter().flatten() {
            assert_eq!(wet.to_bits(), record.to_bits(), "{channels} ch");
        }
    }
}

#[test]
fn the_ring_doubles_its_lanes_up_to_four_channels() {
    use super::super::output_map::FRAME_LANES;
    use super::bus::{carries_record, ring_lanes};
    let expected = [
        (1, true, 2),
        (2, true, 4),
        (3, true, 6),
        (4, true, 8),
        (5, false, 5),
        (8, false, 8),
        // Past eight channels the shared slot is the ninth lane.
        (9, false, 9),
        (10, false, 9),
        (32, false, 9),
    ];
    for (channels, carries, lanes) in expected {
        assert_eq!(carries_record(channels), carries, "{channels} ch");
        assert_eq!(ring_lanes(channels), lanes, "{channels} ch");
        assert!(ring_lanes(channels) <= FRAME_LANES);
    }
}

#[test]
fn a_extra_output_stream_publishes_its_rate_buffer_and_bridge_buffer_fill() {
    let shared = one_extra_output(2, 10.0);
    let bus = shared.extra_output_bus(0);
    for _ in 0..240 {
        bus.push_lanes(&[0.5, -0.25]);
    }
    let mut mixer = ExtraOutputMixer::new(0, 48_000, 2);
    mixer.render(&shared, 128, |_, _| {});
    // No stream has published its rate: nothing to read yet.
    let snapshot = shared.snapshot();
    assert_eq!(snapshot.extra_output_rates, [0; 4]);
    assert_eq!(snapshot.extra_output_buffer_frames, [0; 4]);
    assert_eq!(snapshot.extra_output_ring_fill, [0.0; 4]);
    assert_eq!(snapshot.extra_output_target_frames, [0; 4]);

    // The stream opened at 48 kHz; its driver asks for 128 frames.
    bus.set_playback_rate(48_000);
    mixer.render(&shared, 128, |_, _| {});
    let snapshot = shared.snapshot();
    assert_eq!(snapshot.extra_output_rates[0], 48_000);
    assert_eq!(snapshot.extra_output_buffer_frames[0], 128);
    // A fixed 10 ms bridge buffer at the clock's 48 kHz.
    assert_eq!(snapshot.extra_output_target_frames[0], 480);
    let expected = bus.ring_frames() as f32 / 480.0;
    assert!(
        (snapshot.extra_output_ring_fill[0] - expected).abs() < 1.0e-6,
        "{} vs {expected}",
        snapshot.extra_output_ring_fill[0]
    );
    assert!(snapshot.extra_output_ring_fill[0] > 0.0);
    assert_eq!(snapshot.extra_output_rates[1..], [0; 3]);

    // Closed: the readings go with the stream.
    bus.set_playback_rate(0);
    let snapshot = shared.snapshot();
    assert_eq!(snapshot.extra_output_rates[0], 0);
    assert_eq!(snapshot.extra_output_buffer_frames[0], 0);
    assert_eq!(snapshot.extra_output_ring_fill[0], 0.0);
    // A fresh open starts without the old stream's buffer.
    bus.reset_playback();
    bus.set_playback_rate(44_100);
    assert_eq!(shared.snapshot().extra_output_buffer_frames[0], 0);
}

/// The clock output redrawing after the extra output opened posts a kernel
/// for the pair (clock rate, the device's own rate); the extra mixer adopts
/// it without building one.
#[test]
fn an_extra_output_adopts_the_kernel_the_clock_rate_posts() {
    let frames = 128;
    let shared = one_extra_output(2, 100.0);
    let bus = shared.extra_output_bus(0);
    bus.set_playback_rate(48_000);
    for _ in 0..bus.target_frames() + 6 * frames {
        bus.push_lanes(&[0.25, -0.25]);
    }
    let mut mixer = ExtraOutputMixer::new(0, 48_000, 2);
    let mut out = vec![vec![0.0f32; 2]; frames];
    render_one_block(&mut mixer, &shared, frames, &mut out);
    let built = mixer.configure_calls();

    shared.set_output_rate(96_000);
    assert_eq!(
        shared.extra_kernels[0].pending_pair(),
        Some((96_000, 48_000)),
        "the clock publisher planned the extra's pair"
    );
    render_one_block(&mut mixer, &shared, frames, &mut out);

    assert_eq!(mixer.resampler_pair(), (96_000, 48_000));
    assert_eq!(
        mixer.configure_calls(),
        built,
        "the callback built a kernel"
    );
    assert_eq!(bus.source_rate.load(Ordering::Relaxed), 96_000);
    assert!(out.iter().any(|frame| frame[0] > 0.0));
}
