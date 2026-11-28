//! One process-wide host context, never freed.  The SDK factory stores it
//! in a module global without a reference and reads it during teardown,
//! and VSTGUI keeps the run loop for the module's life.  On Linux it is
//! also `IRunLoop` (SDK 3.8 VSTGUI requires that on the factory context);
//! the editor frame serves the same registry.

use std::ffi::c_void;
use std::ptr;
use std::sync::OnceLock;

use ::vst3::Steinberg::Vst::{
    IAttributeList, IAudioProcessor, IComponent, IConnectionPoint, IEditController,
    IHostApplication, IHostApplicationTrait, IMessage, IPlugInterfaceSupport,
    IPlugInterfaceSupportTrait, IProcessContextRequirements, String128,
};
use ::vst3::Steinberg::{
    kInvalidArgument, kResultFalse, kResultTrue, tresult, FUnknown, IPlugViewContentScaleSupport,
    TUID,
};
use ::vst3::{com_scrape_types::Guid, Class, ComWrapper, Interface};

use super::message::{HostAttributeList, HostMessage};

const HOST_NAME: &str = "RigWerk";

/// Stateless.  On Linux its run loop is the `run_loop` registry.
pub(crate) struct HostApplication;

impl Class for HostApplication {
    #[cfg(target_os = "linux")]
    type Interfaces = (
        IHostApplication,
        IPlugInterfaceSupport,
        ::vst3::Steinberg::Linux::IRunLoop,
    );
    #[cfg(not(target_os = "linux"))]
    type Interfaces = (IHostApplication, IPlugInterfaceSupport);
}

pub(crate) fn host_context() -> *mut FUnknown {
    static HOST: OnceLock<ComWrapper<HostApplication>> = OnceLock::new();
    HOST.get_or_init(|| ComWrapper::new(HostApplication))
        .as_com_ref::<IHostApplication>()
        .expect("HostApplication implements IHostApplication")
        .upcast::<FUnknown>()
        .as_ptr()
}

/// Interfaces this host queries on the plugin (`IPlugInterfaceSupport`),
/// as the SDK's `PlugInterfaceSupport` lists them.
const SUPPORTED: [Guid; 6] = [
    IComponent::IID,
    IAudioProcessor::IID,
    IEditController::IID,
    IConnectionPoint::IID,
    IPlugViewContentScaleSupport::IID,
    IProcessContextRequirements::IID,
];

/// The 16 bytes at `uid`, or `None` for a null pointer.
///
/// # Safety
/// A non-null `uid` must point to 16 readable bytes.
unsafe fn uid_at(uid: *const c_void) -> Option<Guid> {
    // SAFETY: the caller's contract; `Guid` has alignment 1.
    (!uid.is_null()).then(|| unsafe { uid.cast::<Guid>().read() })
}

impl IHostApplicationTrait for HostApplication {
    unsafe fn getName(&self, name: *mut String128) -> tresult {
        // SAFETY: the plugin passes a writable `String128` or null.
        let Some(name) = (unsafe { name.as_mut() }) else {
            return kInvalidArgument;
        };
        name.fill(0);
        for (unit, code) in name.iter_mut().zip(HOST_NAME.encode_utf16()) {
            *unit = code;
        }
        kResultTrue
    }

    unsafe fn createInstance(
        &self,
        cid: *mut TUID,
        iid: *mut TUID,
        obj: *mut *mut c_void,
    ) -> tresult {
        if obj.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: `obj` is the plugin's out pointer, checked above.
        unsafe { *obj = ptr::null_mut() };
        // SAFETY: the plugin passes two `TUID`s or nulls.
        let (Some(cid), Some(iid)) = (unsafe { uid_at(cid.cast()) }, unsafe { uid_at(iid.cast()) })
        else {
            return kInvalidArgument;
        };
        // Host classes use their interface id as class id (`hostclasses.cpp`).
        let created = if cid == IMessage::IID && iid == IMessage::IID {
            ComWrapper::new(HostMessage::new())
                .to_com_ptr::<IMessage>()
                .map(|message| message.into_raw().cast::<c_void>())
        } else if cid == IAttributeList::IID && iid == IAttributeList::IID {
            ComWrapper::new(HostAttributeList::new())
                .to_com_ptr::<IAttributeList>()
                .map(|list| list.into_raw().cast::<c_void>())
        } else {
            None
        };
        match created {
            Some(object) => {
                // SAFETY: as above; the reference goes to the plugin.
                unsafe { *obj = object };
                kResultTrue
            }
            // An unknown class, as the SDK answers (`hostclasses.cpp`).
            None => kResultFalse,
        }
    }
}

impl IPlugInterfaceSupportTrait for HostApplication {
    unsafe fn isPlugInterfaceSupported(&self, iid: *const TUID) -> tresult {
        // SAFETY: the plugin passes a `TUID` or null.
        match unsafe { uid_at(iid.cast()) } {
            None => kInvalidArgument,
            Some(iid) if SUPPORTED.contains(&iid) => kResultTrue,
            Some(_) => kResultFalse,
        }
    }
}

#[cfg(target_os = "linux")]
impl ::vst3::Steinberg::Linux::IRunLoopTrait for HostApplication {
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

#[cfg(test)]
mod tests;
