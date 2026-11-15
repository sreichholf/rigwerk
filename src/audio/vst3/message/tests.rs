use std::ffi::{c_void, CStr};

use ::vst3::ComWrapper;
use ::vst3::Steinberg::Vst::{IAttributeList, IAttributeListTrait, IMessage, IMessageTrait};
use ::vst3::Steinberg::{kInvalidArgument, kResultFalse, kResultTrue};

use super::{HostAttributeList, HostMessage};

#[test]
fn attribute_list_round_trips_each_type() {
    let wrapper = ComWrapper::new(HostAttributeList::new());
    let list = wrapper
        .as_com_ref::<IAttributeList>()
        .expect("IAttributeList");
    let text: Vec<u16> = "Grüße".encode_utf16().chain([0]).collect();
    let blob = [1u8, 2, 3, 250];
    // SAFETY: every id is a NUL-terminated literal and every out pointer a
    // live local of the right size.
    unsafe {
        assert_eq!(list.setInt(c"int".as_ptr(), -7), kResultTrue);
        assert_eq!(list.setFloat(c"float".as_ptr(), 0.25), kResultTrue);
        assert_eq!(list.setString(c"text".as_ptr(), text.as_ptr()), kResultTrue);
        assert_eq!(
            list.setBinary(c"blob".as_ptr(), blob.as_ptr().cast(), blob.len() as u32),
            kResultTrue
        );

        let mut int = 0i64;
        assert_eq!(list.getInt(c"int".as_ptr(), &mut int), kResultTrue);
        assert_eq!(int, -7);
        let mut float = 0f64;
        assert_eq!(list.getFloat(c"float".as_ptr(), &mut float), kResultTrue);
        assert_eq!(float, 0.25);

        let mut read = [0xFFFFu16; 16];
        assert_eq!(
            list.getString(c"text".as_ptr(), read.as_mut_ptr(), 32),
            kResultTrue
        );
        assert_eq!(&read[..text.len()], &text[..]);
        // Cut to fit, and still terminated.
        let mut short = [0xFFFFu16; 3];
        assert_eq!(
            list.getString(c"text".as_ptr(), short.as_mut_ptr(), 6),
            kResultTrue
        );
        assert_eq!(short, [text[0], text[1], 0]);

        let mut data: *const c_void = std::ptr::null();
        let mut size = 0u32;
        assert_eq!(
            list.getBinary(c"blob".as_ptr(), &mut data, &mut size),
            kResultTrue
        );
        assert_eq!(
            std::slice::from_raw_parts(data.cast::<u8>(), size as usize),
            &blob
        );

        // A missing id, a different type and a null id.
        assert_eq!(list.getInt(c"missing".as_ptr(), &mut int), kResultFalse);
        assert_eq!(list.getInt(c"float".as_ptr(), &mut int), kResultFalse);
        assert_eq!(list.getFloat(c"int".as_ptr(), &mut float), kResultFalse);
        assert_eq!(
            list.getBinary(c"text".as_ptr(), &mut data, &mut size),
            kResultFalse
        );
        assert_eq!(size, 0);
        assert_eq!(list.setInt(std::ptr::null(), 1), kInvalidArgument);

        // Setting again replaces, whatever the type.
        assert_eq!(list.setFloat(c"int".as_ptr(), 1.5), kResultTrue);
        assert_eq!(list.getInt(c"int".as_ptr(), &mut int), kResultFalse);
        assert_eq!(list.getFloat(c"int".as_ptr(), &mut float), kResultTrue);
        assert_eq!(float, 1.5);
    }
}

#[test]
fn message_keeps_its_id_and_its_attributes() {
    let wrapper = ComWrapper::new(HostMessage::new());
    let message = wrapper.as_com_ref::<IMessage>().expect("IMessage");
    // SAFETY: the ids are NUL-terminated literals and the list is the
    // message's own, alive while the message is.
    unsafe {
        assert!(message.getMessageID().is_null());
        message.setMessageID(c"BinaryMessage".as_ptr());
        assert_eq!(CStr::from_ptr(message.getMessageID()), c"BinaryMessage");
        let list = message.getAttributes();
        assert!(!list.is_null());
        assert_eq!(list, message.getAttributes(), "one list per message");
        let list = ::vst3::ComRef::from_raw(list).expect("a list");
        assert_eq!(list.setInt(c"n".as_ptr(), 3), kResultTrue);
        let mut n = 0;
        assert_eq!(list.getInt(c"n".as_ptr(), &mut n), kResultTrue);
        assert_eq!(n, 3);
    }
}
