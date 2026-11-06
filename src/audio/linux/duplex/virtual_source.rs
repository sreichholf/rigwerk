//! RigWerk Out virtual source and the publish slot that feeds it.

use std::ffi::{c_void, CString};
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use pipewire as pw;
use pw::spa;
use pw::sys as pw_sys;
use spa::sys as spa_sys;

use crate::audio::denormal::DenormalGuard;
use crate::audio::output_map::VIRTUAL_OUT_NAME;

use super::{properties, MAX_FRAMES};

/// Stereo block the duplex callback writes for RigWerk Out; the virtual
/// source filter reads it.  Seqlock: odd `gen` means a write is in flight.
pub(super) struct PublishSlot {
    gen: AtomicU64,
    frames: AtomicUsize,
    left: std::cell::UnsafeCell<[f32; MAX_FRAMES]>,
    right: std::cell::UnsafeCell<[f32; MAX_FRAMES]>,
}

// One writer (duplex process) and one reader (virtual process); both on the
// PipeWire data thread, coordinated by the seqlock.
unsafe impl Sync for PublishSlot {}

impl PublishSlot {
    pub(super) fn new() -> Self {
        Self {
            gen: AtomicU64::new(0),
            frames: AtomicUsize::new(0),
            left: std::cell::UnsafeCell::new([0.0; MAX_FRAMES]),
            right: std::cell::UnsafeCell::new([0.0; MAX_FRAMES]),
        }
    }

    pub(super) fn begin_write(&self) {
        self.gen.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn write_frame(&self, index: usize, left: f32, right: f32) {
        if index >= MAX_FRAMES {
            return;
        }
        // SAFETY: begin_write made `gen` odd; the reader spins until even.
        unsafe {
            (*self.left.get())[index] = left;
            (*self.right.get())[index] = right;
        }
    }

    pub(super) fn end_write(&self, frames: usize) {
        self.frames.store(frames.min(MAX_FRAMES), Ordering::Relaxed);
        self.gen.fetch_add(1, Ordering::Release);
    }

    /// Copies up to `max` frames into `out_l`/`out_r`.  Returns how many.
    /// Spins only briefly: an abandoned odd `gen` (writer never finished)
    /// must not livelock the virtual source callback.
    pub(super) fn read(&self, out_l: &mut [f32], out_r: &mut [f32], max: usize) -> usize {
        for _ in 0..64 {
            let start = self.gen.load(Ordering::Acquire);
            if start & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            let frames = self
                .frames
                .load(Ordering::Relaxed)
                .min(max)
                .min(out_l.len())
                .min(out_r.len());
            // SAFETY: `gen` was even; we re-check after the copy.
            unsafe {
                ptr::copy_nonoverlapping(self.left.get() as *const f32, out_l.as_mut_ptr(), frames);
                ptr::copy_nonoverlapping(
                    self.right.get() as *const f32,
                    out_r.as_mut_ptr(),
                    frames,
                );
            }
            if self.gen.load(Ordering::Acquire) == start {
                return frames;
            }
        }
        0
    }
}

/// Process data for the RigWerk Out virtual source filter.
struct VirtualProcess {
    publish: Arc<PublishSlot>,
    ports: [*mut c_void; 2],
    scratch_l: Vec<f32>,
    scratch_r: Vec<f32>,
}

unsafe extern "C" fn on_virtual_process(
    data: *mut c_void,
    position: *mut spa_sys::spa_io_position,
) {
    let data = unsafe { &mut *(data as *mut VirtualProcess) };
    if position.is_null() {
        return;
    }
    let _denormals = DenormalGuard::new();
    let frames = unsafe { (*position).clock.duration as usize };
    if frames == 0 {
        return;
    }
    let buffer = |port: *mut c_void| unsafe {
        pw_sys::pw_filter_get_dsp_buffer(port, frames as u32) as *mut f32
    };
    let out_l = buffer(data.ports[0]);
    let out_r = buffer(data.ports[1]);
    if data.scratch_l.len() < frames {
        // Cap to the prepared size; a larger quantum writes silence past it.
        let n = frames.min(MAX_FRAMES);
        data.scratch_l.resize(n, 0.0);
        data.scratch_r.resize(n, 0.0);
    }
    let want = frames.min(MAX_FRAMES);
    let got = data.publish.read(
        &mut data.scratch_l[..want],
        &mut data.scratch_r[..want],
        want,
    );
    for index in 0..frames {
        let (left, right) = if index < got {
            (data.scratch_l[index], data.scratch_r[index])
        } else {
            (0.0, 0.0)
        };
        if !out_l.is_null() {
            unsafe { *out_l.add(index) = left };
        }
        if !out_r.is_null() {
            unsafe { *out_r.add(index) = right };
        }
    }
}

/// RigWerk Out: a virtual source other apps can capture.  Created and
/// destroyed live when publish is toggled.
pub(super) struct VirtualSource {
    raw: NonNull<pw_sys::pw_filter>,
    _hooks: Box<spa_sys::spa_hook>,
    _events: Box<pw_sys::pw_filter_events>,
    _data: Box<VirtualProcess>,
}

impl Drop for VirtualSource {
    fn drop(&mut self) {
        unsafe { pw_sys::pw_filter_destroy(self.raw.as_ptr()) };
    }
}

impl VirtualSource {
    pub(super) fn new(core: &pw::core::CoreRc, publish: Arc<PublishSlot>) -> Result<Self, String> {
        let props = properties(&[
            ("media.type", "Audio"),
            ("media.class", "Audio/Source/Virtual"),
            ("media.role", "DSP"),
            ("node.name", VIRTUAL_OUT_NAME),
            ("node.description", VIRTUAL_OUT_NAME),
            ("node.virtual", "true"),
            ("node.autoconnect", "false"),
        ]);
        let name = CString::new(VIRTUAL_OUT_NAME).expect("node name has no NUL");
        let raw =
            unsafe { pw_sys::pw_filter_new(core.as_raw_ptr(), name.as_ptr(), props.into_raw()) };
        let raw =
            NonNull::new(raw).ok_or_else(|| "pw_filter_new (RigWerk Out) failed".to_owned())?;
        let mut data = Box::new(VirtualProcess {
            publish,
            ports: [ptr::null_mut(); 2],
            scratch_l: vec![0.0; MAX_FRAMES],
            scratch_r: vec![0.0; MAX_FRAMES],
        });
        for (index, (port_name, channel)) in [("capture_FL", "FL"), ("capture_FR", "FR")]
            .iter()
            .enumerate()
        {
            let props = properties(&[
                ("format.dsp", "32 bit float mono audio"),
                ("port.name", port_name),
                ("audio.channel", channel),
            ]);
            let port = unsafe {
                pw_sys::pw_filter_add_port(
                    raw.as_ptr(),
                    spa_sys::SPA_DIRECTION_OUTPUT,
                    pw_sys::pw_filter_port_flags_PW_FILTER_PORT_FLAG_MAP_BUFFERS,
                    std::mem::size_of::<u64>(),
                    props.into_raw(),
                    ptr::null_mut(),
                    0,
                )
            };
            if port.is_null() {
                unsafe { pw_sys::pw_filter_destroy(raw.as_ptr()) };
                return Err(format!("could not add RigWerk Out port {port_name}"));
            }
            data.ports[index] = port;
        }
        let mut events = Box::new(pw_sys::pw_filter_events {
            version: pw_sys::PW_VERSION_FILTER_EVENTS,
            destroy: None,
            state_changed: None,
            io_changed: None,
            param_changed: None,
            add_buffer: None,
            remove_buffer: None,
            process: Some(on_virtual_process as _),
            drained: None,
            command: None,
        });
        let mut hook: Box<spa_sys::spa_hook> = Box::new(unsafe { std::mem::zeroed() });
        unsafe {
            pw_sys::pw_filter_add_listener(
                raw.as_ptr(),
                hook.as_mut(),
                events.as_mut(),
                data.as_mut() as *mut VirtualProcess as *mut c_void,
            );
        }
        let filter = Self {
            raw,
            _hooks: hook,
            _events: events,
            _data: data,
        };
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
                "pw_filter_connect (RigWerk Out) failed: {}",
                std::io::Error::from_raw_os_error(-result)
            ));
        }
        Ok(filter)
    }
}
