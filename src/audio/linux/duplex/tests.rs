/// Runs the real bridge entry point against a live PipeWire daemon.
/// Set `RIGWERK_PW_SOURCE` and `RIGWERK_PW_SINK` to node names (any node
/// with audio output ports works as the source, e.g. a `pw-cat` tone)
/// and run with `--ignored`.
#[test]
#[ignore = "needs a running PipeWire daemon and test nodes"]
fn duplex_bridge_runs_against_a_live_daemon() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    let (Ok(source), Ok(sink)) = (
        std::env::var("RIGWERK_PW_SOURCE"),
        std::env::var("RIGWERK_PW_SINK"),
    ) else {
        panic!("set RIGWERK_PW_SOURCE and RIGWERK_PW_SINK");
    };
    let shared = Arc::new(crate::audio::shared::Shared::new(48_000));
    shared.reset();
    let stop = Arc::new(AtomicBool::new(false));
    let config = crate::audio::types::StartConfig {
        asio_clsid: None,
        input_id: Some(format!("pipewire:{source}")),
        output_id: format!("pipewire:{sink}"),
        output_asio_clsid: None,
        sample_rate: 48_000,
        buffer_size: 128,
        output_rate: None,
        output_buffer: 128,
        asio_wrapper: false,
        real_style_output: false,
        exclusive_output: false,
        pipewire_low_latency: false,
    };
    let thread = {
        let shared = Arc::clone(&shared);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || crate::audio::linux::native::run(&config, shared, stop))
    };
    let seconds: u64 = std::env::var("RIGWERK_PW_SECONDS")
        .ok()
        .and_then(|text| text.parse().ok())
        .unwrap_or(3);
    let mut peak_in = 0.0f32;
    let mut peak_out = 0.0f32;
    for _ in 0..seconds * 10 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        let snapshot = shared.snapshot();
        peak_in = peak_in.max(snapshot.in_l);
        peak_out = peak_out.max(snapshot.out_l);
    }
    let snapshot = shared.snapshot();
    stop.store(true, Ordering::Relaxed);
    let result = thread.join().expect("bridge thread");
    eprintln!(
        "status={:?} mode={:?} error={:?} frames={} input_frames={} rate={} step={} \
         peak_in={peak_in} peak_out={peak_out} direct={} underruns={} path_delay={:?}",
        snapshot.status,
        snapshot.output_mode,
        snapshot.error,
        snapshot.frames,
        snapshot.input_frames,
        snapshot.output_rate,
        snapshot.output_step_frames,
        snapshot.direct,
        snapshot.underruns,
        snapshot.path_delay_ms,
    );
    assert!(result.is_ok(), "{result:?}");
    assert!(snapshot.direct, "the duplex node should be used");
    assert!(snapshot.frames > 0, "the node should have processed blocks");
    assert!(snapshot.input_frames > 0, "the source should be linked");
    assert!(peak_in > 0.0 && peak_out > 0.0, "audio should flow through");
    assert_eq!(snapshot.underruns, 0);
}
