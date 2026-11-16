use std::ffi::c_void;

use ::vst3::Steinberg::Vst::{
    IAttributeList, IComponent, IComponentHandler, IHostApplication, IHostApplicationTrait,
    IMessage, IPlugInterfaceSupport, IPlugInterfaceSupportTrait,
};
use ::vst3::Steinberg::{kResultFalse, kResultTrue, FUnknown, TUID};
use ::vst3::{ComPtr, ComRef, Interface};

use super::host_context;

fn tuid(guid: [u8; 16]) -> TUID {
    guid.map(|byte| byte as std::ffi::c_char)
}

#[test]
fn host_context_names_the_host_and_vends_messages() {
    // SAFETY: the host context is a live static.
    let context = unsafe { ComRef::<FUnknown>::from_raw(host_context()) }.expect("a context");
    assert_eq!(context.as_ptr(), host_context(), "one context");
    let host = context
        .cast::<IHostApplication>()
        .expect("IHostApplication");
    let support = context
        .cast::<IPlugInterfaceSupport>()
        .expect("IPlugInterfaceSupport");
    #[cfg(target_os = "linux")]
    assert!(context
        .cast::<::vst3::Steinberg::Linux::IRunLoop>()
        .is_some());

    let mut name = [0u16; 128];
    // SAFETY: a writable String128.
    assert_eq!(unsafe { host.getName(&mut name) }, kResultTrue);
    let end = name.iter().position(|&unit| unit == 0).expect("terminated");
    assert_eq!(String::from_utf16_lossy(&name[..end]), "RigWerk");

    for (iid, supported) in [
        (IComponent::IID, kResultTrue),
        (IComponentHandler::IID, kResultFalse),
    ] {
        let iid = tuid(iid);
        // SAFETY: a live TUID.
        assert_eq!(unsafe { support.isPlugInterfaceSupported(&iid) }, supported);
    }

    // `allocateMessage`: class id and interface id are both IMessage.
    let mut message_id = tuid(IMessage::IID);
    let mut object: *mut c_void = std::ptr::null_mut();
    // SAFETY: live TUIDs and out pointer; the reference goes to `message`.
    let message = unsafe {
        let mut iid = message_id;
        assert_eq!(
            host.createInstance(&mut message_id, &mut iid, &mut object),
            kResultTrue
        );
        ComPtr::from_raw(object.cast::<IMessage>()).expect("a message")
    };
    drop(message);
    let mut list_id = tuid(IAttributeList::IID);
    // SAFETY: as above.
    unsafe {
        let mut iid = list_id;
        assert_eq!(
            host.createInstance(&mut list_id, &mut iid, &mut object),
            kResultTrue
        );
        drop(ComPtr::from_raw(object.cast::<IAttributeList>()).expect("a list"));
        // A mismatched interface is refused.
        let mut iid = tuid(IAttributeList::IID);
        let mut cid = tuid(IMessage::IID);
        assert_eq!(
            host.createInstance(&mut cid, &mut iid, &mut object),
            kResultFalse
        );
        assert!(object.is_null());
    }
}
