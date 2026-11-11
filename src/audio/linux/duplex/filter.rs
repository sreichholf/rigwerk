//! Duplex `pw_filter` node: ports, process callback, and state listener.

use std::ffi::{c_void, CString};
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use pipewire as pw;
use pw::spa;
use pw::sys as pw_sys;
use spa::sys as spa_sys;

use crate::audio::denormal::DenormalGuard;
use crate::audio::mixer::OutputMixer;
use crate::audio::output_map::MAX_FOLLOWERS;
use crate::audio::shared::Shared;

use super::links::Observed;
use super::virtual_source::PublishSlot;
use super::{properties, MAX_FRAMES};

/// The node RigWerk publishes for the duplex path.
const DUPLEX_NODE: &str = "RigWerk";
/// Input FL/FR, clock output FL/FR, then one stereo pair per follower slot.
const INPUT_PORTS: usize = 2;
const CLOCK_OUT: usize = 2;
const TOTAL_PORTS: usize = INPUT_PORTS + CLOCK_OUT + MAX_FOLLOWERS * 2;

/// State the realtime callback owns.  The main loop only reads the atomics
/// it shares through `Arc`s; the struct itself lives in a `Box` that the
/// filter's listener points at and is freed only after the filter is gone.
struct ProcessData {
    shared: Arc<Shared>,
    mixer: OutputMixer,
    /// Input FL/FR, clock output FL/FR, then `follow_{i}_FL/FR`.
    ports: [*mut c_void; TOTAL_PORTS],
    scratch_l: Vec<f32>,
    scratch_r: Vec<f32>,
    /// Finished mix for the published RigWerk Out virtual source.
    publish: Arc<PublishSlot>,
    rate: u32,
    /// The graph rate and quantum the callback last ran with, for the main
    /// loop's status text.
    seen_rate: Arc<AtomicU32>,
    seen_quantum: Arc<AtomicU64>,
}

/// What `state_changed` needs.  It arrives on the main loop while the
/// realtime callback may hold `ProcessData`, so it gets its own box and its
/// own listener instead of sharing that one.
struct StateData {
    shared: Arc<Shared>,
    error: Arc<AtomicBool>,
}

unsafe extern "C" fn on_process(data: *mut c_void, position: *mut spa_sys::spa_io_position) {
    // SAFETY: `data` is the `ProcessData` box registered with the listener,
    // alive until after `pw_filter_destroy`; only this callback touches it
    // while the filter runs.
    let data = unsafe { &mut *(data as *mut ProcessData) };
    if position.is_null() {
        return;
    }
    // Flush-to-zero for the whole cycle, as in the other audio callbacks;
    // `render_direct` re-asserts it around the chain.
    let _denormals = DenormalGuard::new();
    // SAFETY: PipeWire passes a valid position for this cycle.
    let clock = unsafe { &(*position).clock };
    let frames = clock.duration as usize;
    if frames == 0 {
        return;
    }
    let rate = clock.rate.denom;
    if rate > 0 && rate != data.rate {
        data.rate = rate;
        data.mixer.set_direct_rate(rate);
        data.shared.note_direct_rate(rate);
        data.seen_rate.store(rate, Ordering::Relaxed);
    }
    data.seen_quantum.store(frames as u64, Ordering::Relaxed);

    let buffer = |port: *mut c_void| unsafe {
        pw_sys::pw_filter_get_dsp_buffer(port, frames as u32) as *mut f32
    };
    let ProcessData {
        shared,
        mixer,
        ports,
        scratch_l,
        scratch_r,
        publish,
        ..
    } = data;
    let shared: &Shared = shared;
    let in_l = buffer(ports[0]);
    let in_r = buffer(ports[1]);
    // Clock + every preallocated follower pair; unlinked ports stay null.
    let mut out_bufs = [ptr::null_mut::<f32>(); CLOCK_OUT + MAX_FOLLOWERS * 2];
    for (slot, out) in out_bufs.iter_mut().enumerate() {
        *out = buffer(ports[INPUT_PORTS + slot]);
    }

    let has_input = !in_l.is_null() || !in_r.is_null();
    if has_input {
        // An unlinked input is not counted as delivered, so a source that
        // has gone away trips the engine's input heartbeat as before.
        shared.note_input_block(frames);
        shared.note_input_frames(frames);
    }
    shared
        .output_step_frames
        .store(frames.min(u32::MAX as usize) as u32, Ordering::Relaxed);
    let gain = shared.input_gain();
    let enabled_l = shared.input_enabled[0].load(Ordering::Relaxed);
    let enabled_r = shared.input_enabled[1].load(Ordering::Relaxed);
    let mut peak_l = 0.0f32;
    let mut peak_r = 0.0f32;
    let publish_active = (0..shared.follower_count())
        .any(|slot| shared.follower_is_virtual(slot) && shared.follower_is_published(slot));
    if publish_active {
        publish.begin_write();
    }
    // A cycle longer than the scratch runs in scratch-sized pieces, so every
    // output sample of the cycle is written.  The publish slot only keeps the
    // first MAX_FRAMES; device ports still get the whole cycle.
    let mut offset = 0;
    let mut published = 0usize;
    while offset < frames {
        let count = (frames - offset).min(MAX_FRAMES);
        for index in 0..count {
            let at = offset + index;
            // SAFETY: a non-null DSP buffer holds `frames` samples.  A
            // missing side copies the other one, as a mono source would.
            let raw_l = (!in_l.is_null()).then(|| unsafe { *in_l.add(at) });
            let raw_r = (!in_r.is_null()).then(|| unsafe { *in_r.add(at) });
            let (raw_l, raw_r) = match (raw_l, raw_r) {
                (Some(left), Some(right)) => (left, right),
                (Some(only), None) | (None, Some(only)) => (only, only),
                (None, None) => (0.0, 0.0),
            };
            // Raw, before gain; the direct path's one callback both captures
            // and plays, so a measurement here still sees the physical loop.
            shared.capture_calibration(raw_l, raw_r);
            let left = raw_l * gain;
            let right = raw_r * gain;
            shared.tap_frame(left, right);
            let left = if enabled_l { left } else { 0.0 };
            let right = if enabled_r { right } else { 0.0 };
            peak_l = peak_l.max(left.abs());
            peak_r = peak_r.max(right.abs());
            shared.push_scope_in(left, right);
            scratch_l[index] = left;
            scratch_r[index] = right;
        }
        mixer.render_direct_devices(
            shared,
            &scratch_l[..count],
            &scratch_r[..count],
            |device, index, mapped| {
                let at = offset + index;
                let base = device * 2;
                if base + 1 < out_bufs.len() {
                    let out_l = out_bufs[base];
                    let out_r = out_bufs[base + 1];
                    // SAFETY: a non-null DSP output buffer holds `frames` samples.
                    if !out_l.is_null() {
                        unsafe { *out_l.add(at) = mapped.sample(0) };
                    }
                    if !out_r.is_null() {
                        unsafe { *out_r.add(at) = mapped.sample(1) };
                    }
                }
                if publish_active && device > 0 && published + index < MAX_FRAMES {
                    let slot = device - 1;
                    if shared.follower_is_virtual(slot) && shared.follower_is_published(slot) {
                        publish.write_frame(published + index, mapped.sample(0), mapped.sample(1));
                    }
                }
            },
        );
        published = (published + count).min(MAX_FRAMES);
        offset += count;
    }
    if publish_active {
        publish.end_write(published);
    }
    shared.in_l.store(peak_l.to_bits(), Ordering::Relaxed);
    shared.in_r.store(peak_r.to_bits(), Ordering::Relaxed);
}

unsafe extern "C" fn on_state_changed(
    data: *mut c_void,
    _old: pw_sys::pw_filter_state,
    state: pw_sys::pw_filter_state,
    error: *const std::os::raw::c_char,
) {
    if state != pw_sys::pw_filter_state_PW_FILTER_STATE_ERROR {
        return;
    }
    // SAFETY: `data` is the `StateData` box of the main-loop listener, alive
    // until after `pw_filter_destroy`; nothing else holds it mutably.
    let data = unsafe { &*(data as *const StateData) };
    let message = if error.is_null() {
        "unknown error".to_owned()
    } else {
        // SAFETY: PipeWire passes a NUL-terminated message.
        unsafe { std::ffi::CStr::from_ptr(error) }
            .to_string_lossy()
            .into_owned()
    };
    data.error.store(true, Ordering::Relaxed);
    data.shared
        .set_device_lost(format!("PipeWire duplex node: {message}"));
}

/// The `pw_filter` with its ports, listener and callback state.  Dropping it
/// destroys the filter first (which stops the realtime callback), then frees
/// what the callback used.
pub(super) struct Filter {
    raw: NonNull<pw_sys::pw_filter>,
    _hooks: [Box<spa_sys::spa_hook>; 2],
    _events: [Box<pw_sys::pw_filter_events>; 2],
    _data: Box<ProcessData>,
    state: Box<StateData>,
}

impl Drop for Filter {
    fn drop(&mut self) {
        // SAFETY: `raw` came from `pw_filter_new` and is destroyed once.
        // Destroying it removes the node from the data loop before returning.
        unsafe { pw_sys::pw_filter_destroy(self.raw.as_ptr()) };
    }
}

impl Filter {
    pub(super) fn new(
        core: &pw::core::CoreRc,
        latency: &str,
        rate: u32,
        shared: Arc<Shared>,
        observed: &Observed,
        publish: Arc<PublishSlot>,
    ) -> Result<Self, String> {
        let rate_hint = format!("1/{}", rate.max(1));
        let props = properties(&[
            ("media.type", "Audio"),
            ("media.category", "Filter"),
            ("media.role", "DSP"),
            ("node.name", DUPLEX_NODE),
            ("node.description", "RigWerk"),
            ("node.latency", latency),
            // A hint, not a force: the graph switches only when its allowed
            // rates and the other clients permit it.
            ("node.rate", &rate_hint),
            // RigWerk links this node itself.
            ("node.autoconnect", "false"),
        ]);
        let name = CString::new(DUPLEX_NODE).expect("node name has no NUL");
        // SAFETY: the core outlives the filter (`run` drops the filter
        // first); `pw_filter_new` takes ownership of `props`.
        let raw =
            unsafe { pw_sys::pw_filter_new(core.as_raw_ptr(), name.as_ptr(), props.into_raw()) };
        let raw = NonNull::new(raw).ok_or_else(|| "pw_filter_new failed".to_owned())?;

        // Clock device is stereo; followers get their own stereo port pairs.
        let mut mixer = OutputMixer::new(rate, 2);
        mixer.prepare(MAX_FRAMES);
        mixer.publish_device_channels(&shared);
        mixer.publish_follower_channels(&shared);
        let mut state = Box::new(StateData {
            shared: Arc::clone(&shared),
            error: Arc::clone(&observed.error),
        });
        let mut data = Box::new(ProcessData {
            shared,
            mixer,
            ports: [ptr::null_mut(); TOTAL_PORTS],
            scratch_l: vec![0.0; MAX_FRAMES],
            scratch_r: vec![0.0; MAX_FRAMES],
            publish,
            // Unknown until the first cycle, which then publishes the real
            // graph rate.
            rate: 0,
            seen_rate: Arc::clone(&observed.rate),
            seen_quantum: Arc::clone(&observed.quantum),
        });
        let events = |state_changed, process| {
            Box::new(pw_sys::pw_filter_events {
                version: pw_sys::PW_VERSION_FILTER_EVENTS,
                destroy: None,
                state_changed,
                io_changed: None,
                param_changed: None,
                add_buffer: None,
                remove_buffer: None,
                process,
                drained: None,
                command: None,
            })
        };
        let mut process_events = events(None, Some(on_process as _));
        let mut state_events = events(Some(on_state_changed as _), None);
        // SAFETY: an all-zero `spa_hook` is the unlinked state PipeWire
        // expects before `add_listener`.
        let mut hooks: [Box<spa_sys::spa_hook>; 2] = [
            Box::new(unsafe { std::mem::zeroed() }),
            Box::new(unsafe { std::mem::zeroed() }),
        ];

        let mut port_names: Vec<(String, &'static str, u32)> = vec![
            ("input_FL".to_owned(), "FL", spa_sys::SPA_DIRECTION_INPUT),
            ("input_FR".to_owned(), "FR", spa_sys::SPA_DIRECTION_INPUT),
            ("output_FL".to_owned(), "FL", spa_sys::SPA_DIRECTION_OUTPUT),
            ("output_FR".to_owned(), "FR", spa_sys::SPA_DIRECTION_OUTPUT),
        ];
        for slot in 0..MAX_FOLLOWERS {
            port_names.push((
                format!("follow_{slot}_FL"),
                "FL",
                spa_sys::SPA_DIRECTION_OUTPUT,
            ));
            port_names.push((
                format!("follow_{slot}_FR"),
                "FR",
                spa_sys::SPA_DIRECTION_OUTPUT,
            ));
        }
        for (index, (port_name, channel, direction)) in port_names.iter().enumerate() {
            let props = properties(&[
                ("format.dsp", "32 bit float mono audio"),
                ("port.name", port_name),
                ("audio.channel", channel),
            ]);
            // SAFETY: `raw` is a live filter; the call takes `props`.
            let port = unsafe {
                pw_sys::pw_filter_add_port(
                    raw.as_ptr(),
                    *direction,
                    pw_sys::pw_filter_port_flags_PW_FILTER_PORT_FLAG_MAP_BUFFERS,
                    std::mem::size_of::<u64>(),
                    props.into_raw(),
                    ptr::null_mut(),
                    0,
                )
            };
            if port.is_null() {
                // SAFETY: nothing else refers to the filter yet.
                unsafe { pw_sys::pw_filter_destroy(raw.as_ptr()) };
                return Err(format!("could not add port {port_name}"));
            }
            data.ports[index] = port;
        }

        // SAFETY: the hook, event and data boxes stay at fixed addresses and
        // live in `Filter` until after `pw_filter_destroy`.  `process` runs on
        // the data thread and only sees `ProcessData`; `state_changed` runs
        // on the main loop and only sees `StateData`.
        unsafe {
            pw_sys::pw_filter_add_listener(
                raw.as_ptr(),
                hooks[0].as_mut(),
                process_events.as_mut(),
                data.as_mut() as *mut ProcessData as *mut c_void,
            );
            pw_sys::pw_filter_add_listener(
                raw.as_ptr(),
                hooks[1].as_mut(),
                state_events.as_mut(),
                state.as_mut() as *mut StateData as *mut c_void,
            );
        }
        // From here on `Filter`'s Drop destroys `raw` on every error path.
        let filter = Self {
            raw,
            _hooks: hooks,
            _events: [process_events, state_events],
            _data: data,
            state,
        };
        // SAFETY: ports and listener are in place.
        let result = unsafe {
            pw_sys::pw_filter_connect(
                filter.raw.as_ptr(),
                pw_sys::pw_filter_flags_PW_FILTER_FLAG_RT_PROCESS,
                ptr::null_mut(),
                0,
            )
        };
        if result < 0 {
            return Err(format!(
                "pw_filter_connect failed: {}",
                std::io::Error::from_raw_os_error(-result)
            ));
        }
        Ok(filter)
    }

    /// The filter's node id once the server has bound it.
    pub(super) fn node_id(&self) -> Option<u32> {
        // SAFETY: `raw` is live for `self`'s lifetime.
        let id = unsafe { pw_sys::pw_filter_get_node_id(self.raw.as_ptr()) };
        (id != spa_sys::SPA_ID_INVALID).then_some(id)
    }

    pub(super) fn shared(&self) -> &Shared {
        &self.state.shared
    }
}
