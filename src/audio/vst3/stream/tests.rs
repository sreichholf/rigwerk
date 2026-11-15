use std::ffi::CStr;

use ::vst3::Steinberg::IBStream_::IStreamSeekMode_::{kIBSeekCur, kIBSeekEnd, kIBSeekSet};
use ::vst3::Steinberg::Vst::{
    IAttributeListTrait, IStreamAttributes, IStreamAttributesTrait, PresetAttributes, StateType,
};
use ::vst3::Steinberg::{
    kInvalidArgument, kOutOfMemory, kResultFalse, kResultOk, kResultTrue, IBStream, IBStreamTrait,
    ISizeableStream, ISizeableStreamTrait,
};
use ::vst3::{ComRef, ComWrapper};

use super::{decode_state, encode_state, MemoryStream, StateBlob, MAX_STREAM_BYTES};

/// A seek mode as `IBStream::seek` takes it: the enum is `int` on Windows
/// and `unsigned` elsewhere.
fn mode(mode: impl TryInto<i32>) -> i32 {
    mode.try_into().ok().expect("a seek mode")
}

fn header(component: u32, controller: u32) -> Vec<u8> {
    let mut bytes = b"VST3HOST_STATE\0\0".to_vec();
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes.extend_from_slice(&component.to_le_bytes());
    bytes.extend_from_slice(&controller.to_le_bytes());
    bytes
}

#[test]
fn state_envelope_round_trips_and_reads_raw_component_state() {
    // Both states: the layout vst3-host writes, byte for byte.
    let both = StateBlob {
        component: vec![1, 2, 3],
        controller: Some(vec![9, 8]),
    };
    let encoded = encode_state(&both).expect("encode");
    let mut expected = header(3, 2);
    expected.extend_from_slice(&[1, 2, 3, 9, 8]);
    assert_eq!(encoded, expected);
    assert_eq!(encoded.len(), 28 + 5);
    assert_eq!(decode_state(&encoded), Ok(both));

    // No controller state is u32::MAX, not an empty payload.
    let component_only = StateBlob {
        component: vec![7; 4],
        controller: None,
    };
    let encoded = encode_state(&component_only).expect("encode");
    assert_eq!(&encoded[24..28], &u32::MAX.to_le_bytes());
    assert_eq!(encoded.len(), 28 + 4);
    assert_eq!(decode_state(&encoded), Ok(component_only));
    let empty_controller = StateBlob {
        component: Vec::new(),
        controller: Some(Vec::new()),
    };
    let encoded = encode_state(&empty_controller).expect("encode");
    assert_eq!(&encoded[24..28], &0u32.to_le_bytes());
    assert_eq!(decode_state(&encoded), Ok(empty_controller));

    // Without the magic: a pre-envelope blob, raw component state.
    assert_eq!(
        decode_state(&[5, 6, 7]),
        Ok(StateBlob {
            component: vec![5, 6, 7],
            controller: None,
        })
    );

    // Truncated: a short header, or payloads shorter than the header says.
    assert!(decode_state(&header(3, 2)[..20]).is_err());
    let mut short = header(3, 2);
    short.extend_from_slice(&[1, 2, 3, 9]);
    assert!(decode_state(&short).is_err());
    // Trailing bytes the header does not describe.
    let mut long = header(1, u32::MAX);
    long.extend_from_slice(&[1, 2]);
    assert!(decode_state(&long).is_err());
    // Lengths far past the data (gigabytes, on one byte), and an unknown
    // version.
    let mut huge = header(u32::MAX - 1, u32::MAX - 1);
    huge.push(0);
    assert!(decode_state(&huge).is_err());
    let mut version = header(0, u32::MAX);
    version[16] = 2;
    assert!(decode_state(&version).is_err());

    // Oversized: payloads past what one stream may hold.
    let oversized = StateBlob {
        component: vec![0; MAX_STREAM_BYTES],
        controller: Some(vec![0]),
    };
    assert!(encode_state(&oversized).is_err());
    assert!(decode_state(&vec![0; MAX_STREAM_BYTES + 1]).is_err());
}

#[test]
fn memory_stream_reads_writes_and_seeks() {
    let stream = ComWrapper::new(MemoryStream::new(Vec::new()));
    let io = stream.as_com_ref::<IBStream>().expect("IBStream");
    let sized = stream
        .as_com_ref::<ISizeableStream>()
        .expect("ISizeableStream");
    let mut done = -1;
    let mut at = -1i64;
    // SAFETY: every pointer passed is a live local of the right size.
    unsafe {
        let mut data = *b"hello";
        assert_eq!(io.write(data.as_mut_ptr().cast(), 5, &mut done), kResultOk);
        assert_eq!(done, 5);
        assert_eq!(io.tell(&mut at), kResultOk);
        assert_eq!(at, 5);

        // A seek past the end and a write there zero-fill the gap.
        assert_eq!(io.seek(2, mode(kIBSeekCur), &mut at), kResultOk);
        assert_eq!(at, 7);
        let mut tail = *b"!";
        assert_eq!(io.write(tail.as_mut_ptr().cast(), 1, &mut done), kResultOk);
        assert_eq!(stream.bytes(), b"hello\0\0!");

        // Reads stop at the end.
        assert_eq!(io.seek(-3, mode(kIBSeekEnd), &mut at), kResultOk);
        assert_eq!(at, 5);
        let mut read = [0xAAu8; 8];
        assert_eq!(io.read(read.as_mut_ptr().cast(), 8, &mut done), kResultOk);
        assert_eq!(done, 3);
        assert_eq!(&read[..3], b"\0\0!");
        assert_eq!(io.read(read.as_mut_ptr().cast(), 8, &mut done), kResultOk);
        assert_eq!(done, 0);

        // Before the start lands on the start; an unknown mode, past the
        // cap or a negative count is refused.
        assert_eq!(io.seek(-100, mode(kIBSeekSet), &mut at), kResultOk);
        assert_eq!(at, 0);
        assert_eq!(io.read(read.as_mut_ptr().cast(), 2, &mut done), kResultOk);
        assert_eq!(&read[..2], b"he");
        assert_eq!(io.seek(0, 7, &mut at), kInvalidArgument);
        assert_eq!(
            io.seek(MAX_STREAM_BYTES as i64 + 1, mode(kIBSeekSet), &mut at),
            kInvalidArgument
        );
        assert_eq!(
            io.read(read.as_mut_ptr().cast(), -1, &mut done),
            kInvalidArgument
        );

        // A write that would grow past the cap writes nothing.
        assert_eq!(
            io.seek(MAX_STREAM_BYTES as i64, mode(kIBSeekSet), &mut at),
            kResultOk
        );
        assert_eq!(
            io.write(tail.as_mut_ptr().cast(), 1, &mut done),
            kOutOfMemory
        );
        assert_eq!(done, 0);
        assert_eq!(stream.bytes().len(), 8);

        let mut size = 0i64;
        assert_eq!(sized.getStreamSize(&mut size), kResultOk);
        assert_eq!(size, 8);
        assert_eq!(sized.setStreamSize(2), kResultOk);
        assert_eq!(stream.bytes(), b"he");
        assert_eq!(sized.setStreamSize(-1), kInvalidArgument);
    }

    // A stream seeded for setState reads from the start.
    let seeded = ComWrapper::new(MemoryStream::new(vec![4, 5]));
    let io = seeded.as_com_ref::<IBStream>().expect("IBStream");
    let mut read = [0u8; 2];
    // SAFETY: live locals of the right size.
    unsafe {
        assert_eq!(io.read(read.as_mut_ptr().cast(), 2, &mut done), kResultOk);
    }
    assert_eq!(read, [4, 5]);
}

#[test]
fn memory_stream_says_it_carries_project_state() {
    let stream = ComWrapper::new(MemoryStream::new(Vec::new()));
    let attributes = stream
        .as_com_ref::<IStreamAttributes>()
        .expect("IStreamAttributes");
    let mut name = [0xFFFFu16; 128];
    let mut text = [0u16; 32];
    // SAFETY: live locals of the right size; the list belongs to the
    // stream, alive while it is.
    unsafe {
        assert_eq!(attributes.getFileName(&mut name), kResultFalse);
        assert_eq!(name[0], 0);
        let list = ComRef::from_raw(attributes.getAttributes()).expect("a list");
        assert_eq!(
            list.getString(PresetAttributes::kStateType, text.as_mut_ptr(), 64),
            kResultTrue
        );
        let end = text.iter().position(|&unit| unit == 0).expect("terminated");
        let kind = String::from_utf16_lossy(&text[..end]);
        assert_eq!(kind, CStr::from_ptr(StateType::kProject).to_string_lossy());
    }
}
