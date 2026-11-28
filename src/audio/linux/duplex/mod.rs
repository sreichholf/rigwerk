//! Both sides already share the graph clock, so a ring would add a buffer
//! and a quantum of ordering slack.  DSP ports run at the graph rate: no
//! ring, no drift, no rate conversion.
//!
//! A filter is not placed like a `target.object` stream, so RigWerk links
//! its own ports and relinks when a device comes back.  Delay past this
//! node stays unknown: `pw_filter_get_time` reported 0, and PipeWire 1.0
//! did not pass port Latency params.  The safe crate has no filter
//! wrapper, so the filter goes through `pipewire-sys`.

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
use crate::audio::types::{OutputMode, StartConfig};

use super::native::{node_latency_frames, node_name};

use filter::Filter;
use links::{MadeLink, Observed};
use virtual_source::{VirtualOutputSlot, VirtualSource};

/// Scratch sized to PipeWire's default `clock.max-quantum`.
const MAX_FRAMES: usize = 8192;
const TICK: Duration = Duration::from_millis(50);

/// `Setup` is before any audio, so the caller can still open the two-stream
/// path.
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
    let virtual_feed = Arc::new(VirtualOutputSlot::new());
    // Before the filter: `Filter::new` connects, so the first callback can
    // publish the graph rate immediately.  A request never undoes a reading
    // (`Shared`), so one made after that callback is dropped.
    shared.note_requested_direct_rate(rate);
    let low_latency = config.pipewire_low_latency;
    let mode = move |rate: u32, quantum: u64| {
        let low = if low_latency { "Low latency · " } else { "" };
        format!("{low}PipeWire duplex node · {quantum} frames at {rate} Hz · no bridge buffer")
    };
    let opened = if low_latency {
        OutputMode::LowLatency
    } else {
        OutputMode::Standard
    };
    shared.set_requested_output(rate, frames, opened, mode(rate, frames as u64));
    let filter = Rc::new(
        Filter::new(
            &core,
            &latency,
            rate,
            Arc::clone(&shared),
            &observed,
            Arc::clone(&virtual_feed),
        )
        .map_err(setup)?,
    );
    // Only now: a Setup failure above must not leave the fallback running as
    // the duplex node.
    shared.direct.store(true, Ordering::Relaxed);
    shared.running.store(true, Ordering::Relaxed);
    shared.set_status(format!(
        "Bridging 2 ch · {rate} Hz · one PipeWire node (no bridge buffer)"
    ));

    // A refused link and other server errors show in the status line.
    let error_shared = Arc::clone(&shared);
    let _core_listener = core
        .add_listener_local()
        .error(move |_id, _seq, _res, message| {
            error_shared.set_status(format!("PipeWire: {message}"));
        })
        .register();

    let made: Rc<RefCell<HashMap<(u32, u32), MadeLink>>> = Rc::default();
    let shown = std::cell::Cell::new((rate, frames as u64));
    let filter_ref = Rc::clone(&filter);
    let virtual_output: Rc<RefCell<Option<VirtualSource>>> = Rc::default();

    let stop_flag = Arc::clone(&stop);
    let loop_quit = mainloop.clone();
    let tick_graph = Rc::clone(&graph);
    let tick_made = Rc::clone(&made);
    let tick_core = core.clone();
    let tick_virtual = Rc::clone(&virtual_output);
    let tick_feed = Arc::clone(&virtual_feed);
    let seen_rate = Arc::clone(&observed.rate);
    let seen_quantum = Arc::clone(&observed.quantum);
    let error = Arc::clone(&observed.error);
    let timer = mainloop.loop_().add_timer(move |_| {
        if stop_flag.load(Ordering::Relaxed) || error.load(Ordering::Relaxed) {
            loop_quit.quit();
            return;
        }
        let shared = filter_ref.shared();
        // Readings, replacing the request above.  The callback only stores
        // the rate and quantum.
        let rate = seen_rate.load(Ordering::Relaxed);
        let quantum = seen_quantum.load(Ordering::Relaxed);
        if rate > 0 && (rate, quantum) != shown.get() {
            shown.set((rate, quantum));
            shared.note_direct_rate(rate);
            shared.set_output(rate, quantum as u32, opened, mode(rate, quantum));
        }
        let want_virtual_output = (0..shared.extra_output_count())
            .any(|slot| shared.extra_output_is_virtual(slot) && shared.virtual_output_is_on(slot));
        let mut virtual_slot = tick_virtual.borrow_mut();
        match (want_virtual_output, virtual_slot.is_some()) {
            (true, false) => match VirtualSource::new(&tick_core, Arc::clone(&tick_feed)) {
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
        // Extra output add/remove is live: rebuild links whenever the plan moves.
        let plan = shared.extra_output_plan();
        let extra_output_sinks: Vec<(usize, &str)> = plan
            .slots
            .iter()
            .take(plan.count)
            .enumerate()
            .filter(|(_, slot)| !slot.virtual_output && !slot.node_name.is_empty())
            .map(|(index, slot)| (index, slot.node_name.as_str()))
            .collect();
        links::sync_wanted_links(
            &tick_core,
            &mut graph,
            &mut made,
            own,
            &source,
            &sink,
            &extra_output_sinks,
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
    virtual_output.borrow_mut().take();
    drop(filter);
    Ok(())
}

#[cfg(test)]
mod tests;
