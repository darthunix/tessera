//! A valid table built from the fuzzer's bytes, and the calls they choose
//! over it with arguments they give: references, chunks, places where a
//! walk starts, partitions and steps of any value.
#![no_main]

use libfuzzer_sys::fuzz_target;
use relhash_fuzz::{Input, World};

fuzz_target!(|data: &[u8]| {
    let mut input = Input::new(data);
    let mut world = World::build(&mut input, true);
    for _ in 0..64 {
        if input.is_empty() {
            break;
        }
        world.call(&mut input);
    }
});
