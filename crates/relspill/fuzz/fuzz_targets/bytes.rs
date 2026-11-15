//! Any bytes as a set's file, of a set the bytes before it choose, read
//! by every read of the crate; then as a block and as packed bodies.
#![no_main]

use libfuzzer_sys::fuzz_target;
use relspill_fuzz::{Input, read_bytes};

fuzz_target!(|data: &[u8]| {
    read_bytes(&mut Input::new(data));
});
