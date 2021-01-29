#![no_main]
use libfuzzer_sys::fuzz_target;

use blockstack_lib::address::c32::{c32_address, c32_address_decode};

fuzz_target!(|data: &[u8]| {
    let s_data0 = match std::str::from_utf8(data) {
        Ok(s) => s,
        _ => return
    };

    let (version0, decoded0) = match c32_address_decode(s_data0) {
        Ok(res) => res,
        _ => return
    };

    let s_data1 = c32_address(version0, &decoded0).unwrap_or_else(|err| panic!("1 failed to encode previously-decoded: {}", err));

    let (version1, decoded1) = c32_address_decode(&s_data1).unwrap_or_else(|err| panic!("2 failed to encode previously-decoded: {}", err));

    // Note: s_data0 and s_data1 are not necessarily equal;
    // there is normalization in the decoder.
    assert_eq!(version0, version1, "Roundtrip decode->encode failed!");
    assert_eq!(decoded0, decoded1, "Roundtrip decode->encode failed!");
});
