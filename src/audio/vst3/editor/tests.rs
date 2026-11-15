use std::sync::atomic::{AtomicI32, AtomicU32, AtomicUsize, Ordering};
use std::sync::Mutex;

use ::vst3::Steinberg::{
    char16, int16, kInvalidArgument, kResultFalse, kResultOk, kResultTrue, tresult, FIDString,
    IPlugFrame, IPlugFrameTrait, IPlugView, IPlugViewTrait, TBool, ViewRect,
};
use ::vst3::{Class, ComPtr, ComWrapper};

use super::PlugFrame;

/// A view that keeps its size, records `onSize` and, the first time, asks
/// its frame for another size from inside it.
#[derive(Default)]
struct View {
    sized: AtomicUsize,
    width: AtomicU32,
    height: AtomicU32,
    /// What the nested `resizeView` returned.
    nested: AtomicI32,
    frame: Mutex<Option<ComPtr<IPlugFrame>>>,
    /// This view as an `IPlugView`, for the nested request.
    this: AtomicUsize,
}

impl Class for View {
    type Interfaces = (IPlugView,);
}

impl IPlugViewTrait for View {
    unsafe fn isPlatformTypeSupported(&self, _type: FIDString) -> tresult {
        kResultFalse
    }
    unsafe fn attached(&self, _parent: *mut std::ffi::c_void, _type: FIDString) -> tresult {
        kResultFalse
    }
    unsafe fn removed(&self) -> tresult {
        kResultOk
    }
    unsafe fn onWheel(&self, _distance: f32) -> tresult {
        kResultFalse
    }
    unsafe fn onKeyDown(&self, _key: char16, _code: int16, _modifiers: int16) -> tresult {
        kResultFalse
    }
    unsafe fn onKeyUp(&self, _key: char16, _code: int16, _modifiers: int16) -> tresult {
        kResultFalse
    }
    unsafe fn getSize(&self, size: *mut ViewRect) -> tresult {
        // SAFETY: the frame passes a writable rect.
        unsafe {
            *size = ViewRect {
                left: 0,
                top: 0,
                right: self.width.load(Ordering::SeqCst) as i32,
                bottom: self.height.load(Ordering::SeqCst) as i32,
            };
        }
        kResultOk
    }
    unsafe fn onSize(&self, new_size: *mut ViewRect) -> tresult {
        // SAFETY: the frame passes the rect it was given.
        let rect = unsafe { *new_size };
        self.width
            .store((rect.right - rect.left) as u32, Ordering::SeqCst);
        self.height
            .store((rect.bottom - rect.top) as u32, Ordering::SeqCst);
        if self.sized.fetch_add(1, Ordering::SeqCst) == 0 {
            let frame = self.frame.lock().expect("frame").clone().expect("a frame");
            let mut nested = ViewRect {
                left: 0,
                top: 0,
                right: 300,
                bottom: 200,
            };
            let this = self.this.load(Ordering::SeqCst) as *mut IPlugView;
            // SAFETY: this view is alive inside its own callback.
            let result = unsafe { frame.resizeView(this, &mut nested) };
            self.nested.store(result, Ordering::SeqCst);
        }
        kResultOk
    }
    unsafe fn onFocus(&self, _state: TBool) -> tresult {
        kResultOk
    }
    unsafe fn setFrame(&self, _frame: *mut IPlugFrame) -> tresult {
        kResultOk
    }
    unsafe fn canResize(&self) -> tresult {
        kResultFalse
    }
    unsafe fn checkSizeConstraint(&self, _rect: *mut ViewRect) -> tresult {
        kResultFalse
    }
}

fn rect(left: i32, top: i32, right: i32, bottom: i32) -> ViewRect {
    ViewRect {
        left,
        top,
        right,
        bottom,
    }
}

#[test]
fn plug_frame_records_a_resize_and_sizes_the_view_in_the_same_call() {
    let frame = ComWrapper::new(PlugFrame::new());
    let link = frame.to_com_ptr::<IPlugFrame>().expect("IPlugFrame");
    let view = ComWrapper::new(View::default());
    *view.frame.lock().expect("frame") = Some(link.clone());
    let view_ptr = view.as_com_ref::<IPlugView>().expect("IPlugView").as_ptr();
    view.this.store(view_ptr as usize, Ordering::SeqCst);
    assert_eq!(frame.take_request(), None);

    let mut first = rect(10, 20, 650, 500);
    // SAFETY: a live view and rect.
    assert_eq!(unsafe { link.resizeView(view_ptr, &mut first) }, kResultOk);
    // The view asked again from inside `onSize`: refused, sized once.
    assert_eq!(view.nested.load(Ordering::SeqCst), kResultFalse);
    assert_eq!(view.sized.load(Ordering::SeqCst), 1);
    assert_eq!(view.width.load(Ordering::SeqCst), 640);
    assert_eq!(frame.take_request(), Some((640, 480)));
    assert_eq!(frame.take_request(), None, "taken once");

    // The size the view already has: nothing recorded, no `onSize`.
    let mut same = rect(0, 0, 640, 480);
    // SAFETY: as above.
    assert_eq!(unsafe { link.resizeView(view_ptr, &mut same) }, kResultTrue);
    assert_eq!(frame.take_request(), None);
    assert_eq!(view.sized.load(Ordering::SeqCst), 1);

    // The guard is released after the outer call: a new size goes through.
    let mut bigger = rect(0, 0, 800, 600);
    // SAFETY: as above.
    assert_eq!(unsafe { link.resizeView(view_ptr, &mut bigger) }, kResultOk);
    assert_eq!(frame.take_request(), Some((800, 600)));
    assert_eq!(view.sized.load(Ordering::SeqCst), 2);

    // An empty or inverted rect is refused and not recorded.
    let mut empty = rect(5, 5, 5, 50);
    // SAFETY: as above.
    assert_eq!(
        unsafe { link.resizeView(view_ptr, &mut empty) },
        kInvalidArgument
    );
    assert_eq!(frame.take_request(), None);
    assert_eq!(view.sized.load(Ordering::SeqCst), 2);
    // Break the view → frame cycle.
    view.frame.lock().expect("frame").take();
}
