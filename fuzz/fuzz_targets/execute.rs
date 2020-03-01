#![no_main]

use libfuzzer_sys::fuzz_target;

use blockstack_lib::vm::execute;

fuzz_target!(|data: &[u8]| {
    fuzz(data);
});

fn fuzz(data: &[u8]) {
    match std::str::from_utf8(data) {
        Ok(program) => {
            let _ = execute(program);
        },
        Err(_) => {},
    };
}
