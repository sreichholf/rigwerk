use super::{parse_class_id, parse_class_id_for};

fn bytes(cid: ::vst3::Steinberg::TUID) -> [u8; 16] {
    cid.map(|unit| unit as u8)
}

#[test]
fn class_ids_parse_in_canonical_and_com_order() {
    let text = "123456789ABCDEF01122334455667788";
    let canonical = [
        0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77,
        0x88,
    ];
    // The first three GUID fields little-endian, the rest as written.
    let com = [
        0x78, 0x56, 0x34, 0x12, 0xBC, 0x9A, 0xF0, 0xDE, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77,
        0x88,
    ];
    assert_eq!(parse_class_id_for(text, false).map(bytes), Some(canonical));
    assert_eq!(parse_class_id_for(text, true).map(bytes), Some(com));
    assert_eq!(
        parse_class_id_for(&text.to_ascii_lowercase(), false).map(bytes),
        Some(canonical)
    );
    let native = if cfg!(target_os = "windows") {
        com
    } else {
        canonical
    };
    assert_eq!(parse_class_id(text).map(bytes), Some(native));

    // The bindings' own `uid` builds the platform order: AGain's processor
    // id (`againcids.h`) must come out the same.
    let again = ::vst3::uid(0x84E8DE5F, 0x92554F53, 0x96FAE413, 0x3C935A18);
    assert_eq!(
        parse_class_id("84E8DE5F92554F5396FAE4133C935A18"),
        Some(again)
    );

    for bad in [
        "",
        "1234",
        "12345678-9ABC-DEF0-1122-334455667788",
        "123456789ABCDEF0112233445566778G",
        "123456789ABCDEF0112233445566778800",
    ] {
        assert_eq!(parse_class_id_for(bad, false), None, "{bad}");
        assert_eq!(parse_class_id_for(bad, true), None, "{bad}");
    }
}
