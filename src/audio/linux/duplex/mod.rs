//! One PipeWire node for a PipeWire → PipeWire bridge.
//!
//! When both the input and the output are PipeWire nodes they already share
//! one graph clock: PipeWire itself keeps any second device in step with the
//! graph driver.  Two separate streams joined by RigWerk's ring would add a
//! cushion and drift steering that have nothing to correct, and because the
//! two streams are not linked to each other the graph may run playback before
//! capture in a cycle, which costs one more quantum.
//!
//! This path is a single `pw_filter` node, "RigWerk", with two DSP input
//! ports and two DSP output ports.  One realtime callback reads the input
//! ports, runs the mixer chain on that block and writes the output ports in
//! the same graph cycle.  There is no ring, no cushion, no priming, no drift
//! steering and no rate conversion: DSP ports always run at the graph rate.
//!
//! A filter is not placed by the session manager the way a stream with
//! `target.object` is, so RigWerk links its own ports.  It watches the
//! registry for the chosen source and sink by `node.name` and creates links
//! through `link-factory` whenever the ports it wants are present and not yet
//! linked.  A device that disappears and comes back (a profile switch, a USB
//! re-plug) is linked again as soon as its new ports show up.
//!
//! The delay past RigWerk's output (filters, Bluetooth, the device) is left
//! unknown on this path.  A stream reports it through `pw_stream_get_time`;
//! a filter's `pw_filter_get_time` reported 0 here, and PipeWire 1.0 did not
//! pass the ports' Latency params to the filter, so there is nothing honest
//! to show.
//!
//! The safe `pipewire` crate has no filter wrapper, so the filter itself goes
//! through `pipewire-sys`.  Everything else (core, registry, links) uses the
//! safe API.

mod filter;
mod graph;
mod links;
mod virtual_source;

pub(crate) use graph::Graph;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use pipewire as pw;

use crate::audio::shared::Shared;
use crate::audio::types::StartConfig;

use super::native::{node_latency_frames, node_name};

use filter::Filter;
use links::{MadeLink, Observed};
use virtual_source::{PublishSlot, VirtualSource};

/// The largest graph quantum the scratch is sized for.  PipeWire's own
/// `clock.max-quantum` default is 8192.
const MAX_FRAMES: usize = 8192;
/// How often the main loop checks the stop flag, links and the graph rate.
const TICK: Duration = Duration::from_millis(50);

/// Why the duplex path did not run.  `Setup` errors happen before any audio
/// flowed, so the caller can still open the two-stream path instead.
pub(crate) enum DuplexError {
    Setup(String),
    Runtime(String),
}

fn properties(pairs: &[(&str, &str)]) -> pw::properties::PropertiesBox {
    let mut props = pw::properties::PropertiesBox::new();
    for (key, value) in pairs {
        props.insert(*key, *value);
    }
    props
}

/// Runs the duplex node until `stop`.  A `Setup` error means nothing was
/// opened and the two-stream path can still be tried.
pub(crate) fn run(
    config: &StartConfig,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
) -> Result<(), DuplexError> {
    let setup = DuplexError::Setup;
    let source = node_name(config.input_id.as_deref().unwrap_or(""))
        .ok_or_else(|| setup("Choose a PipeWire input".to_owned()))?
        .to_owned();
    let sink = node_name(&config.output_id)
        .ok_or_else(|| setup("Choose a PipeWire output".to_owned()))?
        .to_owned();
    let rate = config.output_rate.unwrap_or(config.sample_rate).max(1);
    // One node, one quantum: the smaller of the two buffer requests.
    let frames = node_latency_frames(
        config.pipewire_low_latency,
        config.buffer_size.min(config.output_buffer),
    );
    let latency = format!("{frames}/{rate}");

    let mainloop =
        pw::main_loop::MainLoopRc::new(None).map_err(|error| setup(error.to_string()))?;
    let context =
        pw::context::ContextRc::new(&mainloop, None).map_err(|error| setup(error.to_string()))?;
    let core = context
        .connect_rc(None)
        .map_err(|error| setup(error.to_string()))?;
    let registry = core
        .get_registry_rc()
        .map_err(|error| setup(error.to_string()))?;

    let graph = Rc::new(RefCell::new(Graph::default()));
    let added = Rc::clone(&graph);
    let removed = Rc::clone(&graph);
    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            if let Some(props) = global.props.as_ref() {
                added
                    .borrow_mut()
                    .add_global(global.id, &global.type_, props);
            }
        })
        .global_remove(move |id| removed.borrow_mut().remove_global(id))
        .register();

    let observed = Observed {
        rate: Arc::new(AtomicU32::new(0)),
        quantum: Arc::new(AtomicU64::new(frames as u64)),
        error: Arc::new(AtomicBool::new(false)),
    };
    let publish = Arc::new(PublishSlot::new());
    let filter = Rc::new(
        Filter::new(
            &core,
            &latency,
            rate,
            Arc::clone(&shared),
            &observed,
            Arc::clone(&publish),
        )
        .map_err(setup)?,
    );
    // Only now: a fallback must not start from this path's rates.  The
    // callback publishes the real graph rate on its first cycle, and the
    // timer below re-publishes it, so this guess cannot stick.
    shared.note_direct_rate(rate);
    shared.direct.store(true, Ordering::Relaxed);
    let low_latency = config.pipewire_low_latency;
    let mode = move |rate: u32, quantum: u64| {
        let low = if low_latency { "Low latency · " } else { "" };
        format!("{low}PipeWire duplex node · {quantum} frames at {rate} Hz · no bridge buffer")
    };
    shared.set_output(rate, frames, mode(rate, frames as u64));
    shared.running.store(true, Ordering::Relaxed);
    shared.set_status(format!(
        "Bridging 2 ch · {rate} Hz · one PipeWire node (no bridge buffer)"
    ));

    // Errors from the server (a link the factory refused, for one) show up
    // in the status line while the bridge runs.
    let error_shared = Arc::clone(&shared);
    let _core_listener = core
        .add_listener_local()
        .error(move |_id, _seq, _res, message| {
            error_shared.set_status(format!("PipeWire: {message}"));
        })
        .register();

    // Links RigWerk asked for, by (output port, input port).
    let made: Rc<RefCell<HashMap<(u32, u32), MadeLink>>> = Rc::default();
    let shown = std::cell::Cell::new((rate, frames as u64));
    let filter_ref = Rc::clone(&filter);
    let virtual_out: Rc<RefCell<Option<VirtualSource>>> = Rc::default();

    let stop_flag = Arc::clone(&stop);
    let loop_quit = mainloop.clone();
    let tick_graph = Rc::clone(&graph);
    let tick_made = Rc::clone(&made);
    let tick_core = core.clone();
    let tick_virtual = Rc::clone(&virtual_out);
    let tick_publish = Arc::clone(&publish);
    let seen_rate = Arc::clone(&observed.rate);
    let seen_quantum = Arc::clone(&observed.quantum);
    let error = Arc::clone(&observed.error);
    let timer = mainloop.loop_().add_timer(move |_| {
        if stop_flag.load(Ordering::Relaxed) || error.load(Ordering::Relaxed) {
            loop_quit.quit();
            return;
        }
        let shared = filter_ref.shared();
        // The mode text follows the graph rate and quantum; the callback
        // only stores them.
        let rate = seen_rate.load(Ordering::Relaxed);
        let quantum = seen_quantum.load(Ordering::Relaxed);
        if rate > 0 && (rate, quantum) != shown.get() {
            shown.set((rate, quantum));
            shared.note_direct_rate(rate);
            shared.set_output(rate, quantum as u32, mode(rate, quantum));
        }
        // Create or drop RigWerk Out when publish is toggled live.
        let want_publish = (0..shared.follower_count())
            .any(|slot| shared.follower_is_virtual(slot) && shared.follower_is_published(slot));
        let mut virtual_slot = tick_virtual.borrow_mut();
        match (want_publish, virtual_slot.is_some()) {
            (true, false) => match VirtualSource::new(&tick_core, Arc::clone(&tick_publish)) {
                Ok(source) => *virtual_slot = Some(source),
                Err(message) => shared.set_status(format!("RigWerk Out: {message}")),
            },
            (false, true) => *virtual_slot = None,
            _ => {}
        }
        drop(virtual_slot);
        let Some(own) = filter_ref.node_id() else {
            return;
        };
        let mut graph = tick_graph.borrow_mut();
        let mut made = tick_made.borrow_mut();
        // Follower add/remove is live: rebuild links whenever the plan moves.
        let plan = shared.follower_plan();
        let follower_sinks: Vec<(usize, &str)> = plan
            .slots
            .iter()
            .take(plan.count)
            .enumerate()
            .filter(|(_, slot)| !slot.virtual_out && !slot.node_name.is_empty())
            .map(|(index, slot)| (index, slot.node_name.as_str()))
            .collect();
        links::sync_wanted_links(
            &tick_core,
            &mut graph,
            &mut made,
            own,
            &source,
            &sink,
            &follower_sinks,
            shared,
        );
    });
    let started = timer.update_timer(Some(TICK), Some(TICK)).into_result();
    if let Err(error) = started {
        shared.running.store(false, Ordering::Relaxed);
        shared.direct.store(false, Ordering::Relaxed);
        return Err(DuplexError::Runtime(format!(
            "PipeWire duplex timer: {error}"
        )));
    }
    mainloop.run();
    drop(timer);
    // Links first, then the virtual source, then the node.
    made.borrow_mut().clear();
    virtual_out.borrow_mut().take();
    drop(filter);
    Ok(())
}

#[cfg(test)]
mod tests {
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
}
