//! The frame a plugin's editor view is attached with (`IPlugFrame`).
//!
//! `resizeView` records the size the view asked for, for the window to
//! pick up on its next frame, and then calls the view's `onSize` in the
//! same call stack, as the SDK requires.  It does nothing when the view
//! already has that size (`onSize` only on a change, `iplugview.h`), and a
//! `resizeView` from inside that `onSize` is refused, as the SDK's editor
//! host does (`resizeViewRecursionGard`).  No lock is held around the
//! call.  On Linux the
//! frame is also an `IRunLoop` (older VSTGUI and JUCE look for it here);
//! it registers into the same registry as the host context.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ::vst3::Steinberg::{
    kInvalidArgument, kResultFalse, kResultOk, kResultTrue, tresult, IPlugFrame, IPlugFrameTrait,
    IPlugView, IPlugViewTrait, ViewRect,
};
use ::vst3::{Class, ComRef};

/// An editor's frame.  `request` packs the last asked-for size as
/// `width << 32 | height`; 0 is none.
#[derive(Default)]
pub(crate) struct PlugFrame {
    request: AtomicU64,
    /// Set while a `resizeView` is inside the view's `onSize`.
    resizing: AtomicBool,
}

/// Clears [`PlugFrame::resizing`] when the outer `resizeView` returns.
struct Resizing<'a>(&'a AtomicBool);

impl Drop for Resizing<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl PlugFrame {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The size the view last asked for, once.
    pub(crate) fn take_request(&self) -> Option<(u32, u32)> {
        match self.request.swap(0, Ordering::AcqRel) {
            0 => None,
            packed => Some(((packed >> 32) as u32, packed as u32)),
        }
    }
}

impl Class for PlugFrame {
    #[cfg(target_os = "linux")]
    type Interfaces = (IPlugFrame, ::vst3::Steinberg::Linux::IRunLoop);
    #[cfg(not(target_os = "linux"))]
    type Interfaces = (IPlugFrame,);
}

impl IPlugFrameTrait for PlugFrame {
    unsafe fn resizeView(&self, view: *mut IPlugView, new_size: *mut ViewRect) -> tresult {
        // SAFETY: the plugin passes its live view or null.
        let Some(view) = (unsafe { ComRef::from_raw(view) }) else {
            return kInvalidArgument;
        };
        // SAFETY: the plugin passes a readable rect or null.
        let Some(rect) = (unsafe { new_size.as_ref() }) else {
            return kInvalidArgument;
        };
        let size = |from: i32, to: i32| {
            to.checked_sub(from)
                .and_then(|size| u32::try_from(size).ok())
                .filter(|&size| size > 0)
        };
        let (Some(width), Some(height)) =
            (size(rect.left, rect.right), size(rect.top, rect.bottom))
        else {
            return kInvalidArgument;
        };
        if self
            .resizing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return kResultFalse;
        }
        let _resizing = Resizing(&self.resizing);
        let mut current = ViewRect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        // SAFETY: the live view and a writable rect.  A view that cannot
        // say its size is taken to be changing.
        if unsafe { view.getSize(&mut current) } == kResultOk
            && size(current.left, current.right) == Some(width)
            && size(current.top, current.bottom) == Some(height)
        {
            return kResultTrue;
        }
        self.request.store(
            u64::from(width) << 32 | u64::from(height),
            Ordering::Release,
        );
        // SAFETY: the live view and the rect it passed in.
        unsafe { view.onSize(new_size) }
    }
}

#[cfg(target_os = "linux")]
impl ::vst3::Steinberg::Linux::IRunLoopTrait for PlugFrame {
    unsafe fn registerEventHandler(
        &self,
        handler: *mut ::vst3::Steinberg::Linux::IEventHandler,
        fd: ::vst3::Steinberg::Linux::FileDescriptor,
    ) -> tresult {
        // SAFETY: the plugin passes a live handler or null.
        unsafe { super::run_loop::register_fd(handler, fd) }
    }

    unsafe fn unregisterEventHandler(
        &self,
        handler: *mut ::vst3::Steinberg::Linux::IEventHandler,
    ) -> tresult {
        super::run_loop::unregister_fd(handler)
    }

    unsafe fn registerTimer(
        &self,
        handler: *mut ::vst3::Steinberg::Linux::ITimerHandler,
        milliseconds: ::vst3::Steinberg::Linux::TimerInterval,
    ) -> tresult {
        // SAFETY: the plugin passes a live handler or null.
        unsafe { super::run_loop::register_timer(handler, milliseconds) }
    }

    unsafe fn unregisterTimer(
        &self,
        handler: *mut ::vst3::Steinberg::Linux::ITimerHandler,
    ) -> tresult {
        super::run_loop::unregister_timer(handler)
    }
}

#[cfg(test)]
mod tests;
