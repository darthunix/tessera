//! A machine of the crate, made from any params or any words, stepped by
//! events and numbers the fuzzer chooses over regions of any numbers.
#![no_main]

use libfuzzer_sys::fuzz_target;
use relspill_fuzz::{Input, machines};

fuzz_target!(|data: &[u8]| {
    machines(&mut Input::new(data));
});
