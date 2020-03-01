#![no_main]

use libfuzzer_sys::fuzz_target;

use blockstack_lib::net::StacksMessageCodec;

use blockstack_lib::vm::types::Value;

fuzz_target!(|data: &[u8]| {
    fuzz(data);
});

fn fuzz(data: &[u8]) {
    let vec = Vec::from(data);
    let _ = Value::consensus_deserialize(&mut &vec[..]);
}
