//! A valid table built from the fuzzer's bytes, damaged in one to eight
//! places they choose, then the calls they choose over it.
#![no_main]

use libfuzzer_sys::fuzz_target;
use relhash_fuzz::{Input, World};

fuzz_target!(|data: &[u8]| {
    let mut input = Input::new(data);
    let mut world = World::build(&mut input, false);
    let count = 1 + input.below(8);
    world.damage(&mut input, count);
    for _ in 0..64 {
        if input.is_empty() {
            break;
        }
        world.call(&mut input);
    }
});
