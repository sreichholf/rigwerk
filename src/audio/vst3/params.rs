//! The parameter change lists `process` reads and writes
//! (`IParameterChanges`, `IParamValueQueue`), and the empty event list.
//!
//! Built at load for the plugin's parameter table and never grown: one
//! queue per parameter plus a scratch queue for ids the table does not
//! know, each with a fixed number of points.  The audio thread fills and
//! clears them, and the plugin calls them from inside `process`, so every
//! field is a relaxed atomic: no lock, no allocation, and `Send + Sync`
//! without an `unsafe impl`.  A full queue keeps its newest point by
//! replacing the last one.

use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::Arc;

use ::vst3::Steinberg::Vst::{
    Event, IEventListTrait, IParamValueQueue, IParamValueQueueTrait, IParameterChangesTrait,
    ParamID, ParamValue,
};
use ::vst3::Steinberg::{kInvalidArgument, kResultFalse, kResultOk, tresult};
use ::vst3::{Class, ComWrapper};

use crate::audio::plugins::ParamIds;

/// Points per queue on the input side.  The transfer collapses a burst to
/// the newest value per parameter, so a block rarely carries more than one.
pub(crate) const IN_POINTS: usize = 4;
/// Points per queue the plugin may write on the output side.
pub(crate) const OUT_POINTS: usize = 16;

/// A queue slot that is not claimed this block.
const NONE: u32 = u32::MAX;

/// One parameter's points for one block, in sample-offset order.
pub(crate) struct ParamValueQueue {
    id: AtomicU32,
    len: AtomicUsize,
    offsets: Box<[AtomicI32]>,
    /// `f64` bits.
    values: Box<[AtomicU64]>,
}

impl ParamValueQueue {
    fn new(points: usize) -> Self {
        let points = points.max(1);
        Self {
            id: AtomicU32::new(0),
            len: AtomicUsize::new(0),
            offsets: (0..points).map(|_| AtomicI32::new(0)).collect(),
            values: (0..points).map(|_| AtomicU64::new(0)).collect(),
        }
    }

    fn reset(&self, id: u32) {
        self.id.store(id, Relaxed);
        self.len.store(0, Relaxed);
    }

    fn point(&self, at: usize) -> (i32, f64) {
        (
            self.offsets[at].load(Relaxed),
            f64::from_bits(self.values[at].load(Relaxed)),
        )
    }

    fn set_point(&self, at: usize, offset: i32, value: f64) {
        self.offsets[at].store(offset, Relaxed);
        self.values[at].store(value.to_bits(), Relaxed);
    }

    /// `addPoint`: keeps offset order, replaces a point at the same
    /// offset, and when full drops the last point to make room.  Returns
    /// the new point's index.
    pub(crate) fn add(&self, offset: i32, value: f64) -> usize {
        let capacity = self.offsets.len();
        let len = self.len.load(Relaxed).min(capacity);
        let mut at = len;
        for index in 0..len {
            let existing = self.offsets[index].load(Relaxed);
            if existing == offset {
                self.values[index].store(value.to_bits(), Relaxed);
                return index;
            }
            if existing > offset {
                at = index;
                break;
            }
        }
        let len = if len == capacity {
            at = at.min(capacity - 1);
            capacity - 1
        } else {
            len
        };
        for index in (at..len).rev() {
            let (offset, value) = self.point(index);
            self.set_point(index + 1, offset, value);
        }
        self.set_point(at, offset, value);
        self.len.store(len + 1, Relaxed);
        at
    }

    /// The value at the last offset, if any point was added.
    pub(crate) fn last(&self) -> Option<f64> {
        let len = self.len.load(Relaxed).min(self.offsets.len());
        (len > 0).then(|| self.point(len - 1).1)
    }

    /// Every point, for the tests.
    #[cfg(test)]
    pub(crate) fn points(&self) -> Vec<(i32, f64)> {
        let len = self.len.load(Relaxed).min(self.offsets.len());
        (0..len).map(|at| self.point(at)).collect()
    }
}

impl Class for ParamValueQueue {
    type Interfaces = (IParamValueQueue,);
}

impl IParamValueQueueTrait for ParamValueQueue {
    unsafe fn getParameterId(&self) -> ParamID {
        self.id.load(Relaxed)
    }

    unsafe fn getPointCount(&self) -> i32 {
        self.len.load(Relaxed).min(self.offsets.len()) as i32
    }

    unsafe fn getPoint(&self, index: i32, offset: *mut i32, value: *mut ParamValue) -> tresult {
        let len = self.len.load(Relaxed).min(self.offsets.len());
        let Some(at) = usize::try_from(index).ok().filter(|&at| at < len) else {
            return kInvalidArgument;
        };
        let (at_offset, at_value) = self.point(at);
        // SAFETY: the plugin passes writable pointers or nulls.
        unsafe {
            if let Some(offset) = offset.as_mut() {
                *offset = at_offset;
            }
            if let Some(value) = value.as_mut() {
                *value = at_value;
            }
        }
        kResultOk
    }

    unsafe fn addPoint(&self, offset: i32, value: ParamValue, index: *mut i32) -> tresult {
        let at = self.add(offset, value);
        // SAFETY: the plugin passes a writable pointer or null.
        if let Some(index) = unsafe { index.as_mut() } {
            *index = at as i32;
        }
        kResultOk
    }
}

/// The changes of one block: a queue per parameter, claimed in the order
/// they are first used, plus one scratch queue for an unknown id.
pub(crate) struct ParameterChanges {
    /// `ids.len()` queues, then the scratch queue.
    queues: Box<[ComWrapper<ParamValueQueue>]>,
    /// Queues `[0, used)` are claimed this block.
    used: AtomicUsize,
    /// Parameter index → its claimed queue, or [`NONE`].
    slot_of: Box<[AtomicU32]>,
    ids: Arc<ParamIds>,
}

impl ParameterChanges {
    /// Lists for `ids`, `points` per queue (allocates; UI thread, at load).
    pub(crate) fn new(ids: Arc<ParamIds>, points: usize) -> Self {
        let count = ids.len();
        Self {
            queues: (0..=count)
                .map(|_| ComWrapper::new(ParamValueQueue::new(points)))
                .collect(),
            used: AtomicUsize::new(0),
            slot_of: (0..count).map(|_| AtomicU32::new(NONE)).collect(),
            ids,
        }
    }

    fn used(&self) -> usize {
        self.used.load(Relaxed).min(self.slot_of.len())
    }

    /// The queue of parameter `index`, claimed if it is not yet.
    fn claim(&self, index: usize) -> Option<usize> {
        let id = self.ids.id(index)?;
        let slot = self.slot_of[index].load(Relaxed);
        if slot != NONE {
            return Some(slot as usize);
        }
        let slot = self.used();
        if slot >= self.slot_of.len() {
            return None;
        }
        self.queues[slot].reset(id);
        self.slot_of[index].store(slot as u32, Relaxed);
        self.used.store(slot + 1, Relaxed);
        Some(slot)
    }

    /// Adds a point for parameter `index` (audio thread).
    pub(crate) fn push(&self, index: usize, offset: i32, value: f64) {
        if let Some(slot) = self.claim(index) {
            self.queues[slot].add(offset, value);
        }
    }

    /// Calls `emit(index, value)` with each claimed parameter's value at
    /// its last offset.
    pub(crate) fn for_each_last(&self, mut emit: impl FnMut(usize, f64)) {
        for queue in &self.queues[..self.used()] {
            let id = queue.id.load(Relaxed);
            if let (Some(index), Some(value)) = (self.ids.index_of(id), queue.last()) {
                emit(index, value);
            }
        }
    }

    /// Whether no parameter has a point.
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.used() == 0
    }

    /// Forgets every point; only the claimed queues are touched.
    pub(crate) fn clear(&self) {
        for queue in &self.queues[..self.used()] {
            if let Some(index) = self.ids.index_of(queue.id.load(Relaxed)) {
                self.slot_of[index].store(NONE, Relaxed);
            }
            queue.len.store(0, Relaxed);
        }
        self.used.store(0, Relaxed);
    }

    /// The queue for parameter `index`, for the tests.
    #[cfg(test)]
    pub(crate) fn points(&self, index: usize) -> Vec<(i32, f64)> {
        match self.slot_of[index].load(Relaxed) {
            NONE => Vec::new(),
            slot => self.queues[slot as usize].points(),
        }
    }

    fn queue_ptr(&self, slot: usize) -> *mut IParamValueQueue {
        self.queues[slot]
            .as_com_ref::<IParamValueQueue>()
            .map_or(std::ptr::null_mut(), |queue| queue.as_ptr())
    }
}

impl Class for ParameterChanges {
    type Interfaces = (::vst3::Steinberg::Vst::IParameterChanges,);
}

impl IParameterChangesTrait for ParameterChanges {
    unsafe fn getParameterCount(&self) -> i32 {
        self.used() as i32
    }

    unsafe fn getParameterData(&self, index: i32) -> *mut IParamValueQueue {
        match usize::try_from(index) {
            Ok(slot) if slot < self.used() => self.queue_ptr(slot),
            _ => std::ptr::null_mut(),
        }
    }

    unsafe fn addParameterData(
        &self,
        id: *const ParamID,
        index: *mut i32,
    ) -> *mut IParamValueQueue {
        // SAFETY: the plugin passes a readable id or null.
        let Some(&id) = (unsafe { id.as_ref() }) else {
            return std::ptr::null_mut();
        };
        let slot = match self.ids.index_of(id).and_then(|at| self.claim(at)) {
            Some(slot) => slot,
            None => {
                // An id the table does not know: its points go nowhere.
                let scratch = self.queues.len() - 1;
                self.queues[scratch].reset(id);
                scratch
            }
        };
        // SAFETY: the plugin passes a writable pointer or null.
        if let Some(index) = unsafe { index.as_mut() } {
            *index = slot as i32;
        }
        self.queue_ptr(slot)
    }
}

/// An event list with no events, which refuses new ones: RigWerk hosts
/// effects and sends no notes.
pub(crate) struct EmptyEvents;

impl Class for EmptyEvents {
    type Interfaces = (::vst3::Steinberg::Vst::IEventList,);
}

impl IEventListTrait for EmptyEvents {
    unsafe fn getEventCount(&self) -> i32 {
        0
    }

    unsafe fn getEvent(&self, _index: i32, _event: *mut Event) -> tresult {
        kInvalidArgument
    }

    unsafe fn addEvent(&self, _event: *mut Event) -> tresult {
        kResultFalse
    }
}

#[cfg(test)]
mod tests;
