//! A plugin's editor view ([`Vst3Editor`]) and the frame it is attached
//! with (`IPlugFrame`).  The window it lives in is the app's; the view only
//! needs its native handle.
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
    kInvalidArgument, kResultFalse, kResultOk, kResultTrue, tresult, FIDString, IPlugFrame,
    IPlugFrameTrait, IPlugView, IPlugViewContentScaleSupport, IPlugViewContentScaleSupportTrait,
    IPlugViewTrait, ViewRect,
};
use ::vst3::{Class, ComPtr, ComRef, ComWrapper};

use crate::audio::clap::ParentWindow;

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
        let owner = std::ptr::from_ref(self) as usize;
        // SAFETY: the plugin passes a live handler or null.
        unsafe { super::run_loop::register_fd(owner, handler, fd) }
    }

    unsafe fn unregisterEventHandler(
        &self,
        handler: *mut ::vst3::Steinberg::Linux::IEventHandler,
    ) -> tresult {
        let owner = std::ptr::from_ref(self) as usize;
        super::run_loop::unregister_fd(owner, handler)
    }

    unsafe fn registerTimer(
        &self,
        handler: *mut ::vst3::Steinberg::Linux::ITimerHandler,
        milliseconds: ::vst3::Steinberg::Linux::TimerInterval,
    ) -> tresult {
        let owner = std::ptr::from_ref(self) as usize;
        // SAFETY: the plugin passes a live handler or null.
        unsafe { super::run_loop::register_timer(owner, handler, milliseconds) }
    }

    unsafe fn unregisterTimer(
        &self,
        handler: *mut ::vst3::Steinberg::Linux::ITimerHandler,
    ) -> tresult {
        let owner = std::ptr::from_ref(self) as usize;
        super::run_loop::unregister_timer(owner, handler)
    }
}

/// The window type this platform attaches editors to.
#[cfg(target_os = "windows")]
const PLATFORM: FIDString = ::vst3::Steinberg::kPlatformTypeHWND;
#[cfg(not(target_os = "windows"))]
const PLATFORM: FIDString = ::vst3::Steinberg::kPlatformTypeX11EmbedWindowID;

/// A rect `width` × `height` at the origin.
fn rect(width: u32, height: u32) -> ViewRect {
    ViewRect {
        left: 0,
        top: 0,
        right: width.min(i32::MAX as u32) as i32,
        bottom: height.min(i32::MAX as u32) as i32,
    }
}

/// The size of `rect`, if it has one.
fn rect_size(rect: &ViewRect) -> Option<(u32, u32)> {
    let width = u32::try_from(rect.right.checked_sub(rect.left)?).ok()?;
    let height = u32::try_from(rect.bottom.checked_sub(rect.top)?).ok()?;
    (width > 0 && height > 0).then_some((width, height))
}

/// An editor view of one plugin, and its frame.  UI thread only.  The
/// owner keeps the plugin's handle alive for longer (the view belongs to
/// its controller), and drops this before the native window it attached
/// to: dropping it detaches the view.
pub(crate) struct Vst3Editor {
    view: ComPtr<IPlugView>,
    scale: Option<ComPtr<IPlugViewContentScaleSupport>>,
    frame: ComWrapper<PlugFrame>,
    attached: bool,
}

impl Vst3Editor {
    /// Wraps a view `createView` returned, if it can attach to this
    /// platform's windows.
    pub(crate) fn new(view: ComPtr<IPlugView>) -> Result<Self, String> {
        // SAFETY: a live view and a static platform string.
        if unsafe { view.isPlatformTypeSupported(PLATFORM) } != kResultTrue {
            return Err("The plugin's editor cannot open in this window system.".to_owned());
        }
        Ok(Self {
            scale: view.cast::<IPlugViewContentScaleSupport>(),
            view,
            frame: ComWrapper::new(PlugFrame::new()),
            attached: false,
        })
    }

    /// The size the view wants, in physical pixels.
    pub(crate) fn size(&self) -> Option<(u32, u32)> {
        let mut size = rect(0, 0);
        // SAFETY: a live view and a writable rect.
        (unsafe { self.view.getSize(&mut size) } == kResultOk)
            .then(|| rect_size(&size))
            .flatten()
    }

    /// Tells the view the display scale (before attaching, and on a
    /// change).  A view without content scaling ignores it.
    pub(crate) fn set_scale(&self, scale: f64) {
        if let Some(support) = &self.scale {
            // SAFETY: a live view.
            unsafe { support.setContentScaleFactor(scale as f32) };
        }
    }

    /// Attaches the view to `parent` through the frame.
    pub(crate) fn attach(&mut self, parent: ParentWindow) -> Result<(), String> {
        let handle = match parent {
            ParentWindow::X11(window) => window as usize as *mut std::ffi::c_void,
            ParentWindow::Win32(hwnd) => hwnd,
        };
        let frame = self
            .frame
            .as_com_ref::<IPlugFrame>()
            .map_or(std::ptr::null_mut(), |frame| frame.as_ptr());
        // SAFETY: a live view, the frame it keeps a reference to if it
        // wants one, and a live native window.
        unsafe {
            self.view.setFrame(frame);
            if self.view.attached(handle, PLATFORM) != kResultOk {
                self.view.setFrame(std::ptr::null_mut());
                return Err("The plugin's editor did not attach to its window.".to_owned());
            }
        }
        self.attached = true;
        Ok(())
    }

    /// Whether the user may resize the view.
    pub(crate) fn can_resize(&self) -> bool {
        // SAFETY: a live view.
        (unsafe { self.view.canResize() }) == kResultTrue
    }

    /// The nearest size to `width` × `height` the view accepts.
    pub(crate) fn fit(&self, width: u32, height: u32) -> (u32, u32) {
        let mut size = rect(width, height);
        // SAFETY: a live view and a writable rect.
        unsafe { self.view.checkSizeConstraint(&mut size) };
        rect_size(&size).unwrap_or((width, height))
    }

    /// Gives the view a new size (`onSize`).
    pub(crate) fn resize(&self, width: u32, height: u32) {
        let mut size = rect(width, height);
        // SAFETY: a live view and a readable rect.
        unsafe { self.view.onSize(&mut size) };
    }

    /// The size the view asked for through `resizeView`, once.
    pub(crate) fn take_resize_request(&self) -> Option<(u32, u32)> {
        self.frame.take_request()
    }
}

impl Drop for Vst3Editor {
    fn drop(&mut self) {
        // SAFETY: a live view; detached once, before its window goes.
        unsafe {
            if self.attached {
                self.view.removed();
            }
            self.view.setFrame(std::ptr::null_mut());
        }
    }
}

#[cfg(test)]
mod tests;
