//! A set's file written from the fuzzer's bytes, damaged in one to eight
//! places they choose, then every partition read by every read.
#![no_main]

use libfuzzer_sys::fuzz_target;
use relspill_fuzz::{Input, Written};

fuzz_target!(|data: &[u8]| {
    let mut input = Input::new(data);
    let Some(mut written) = Written::new(&mut input) else {
        return;
    };
    written.damage(&mut input);
    written.read(&mut input);
});
