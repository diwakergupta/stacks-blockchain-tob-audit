#![no_main]
use libfuzzer_sys::fuzz_target;

use blockstack_lib::address::c32::{c32_address_decode};

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = c32_address_decode(&s);
    }
});
