//! The proofs by Kani of the bytes form's own arithmetic, which
//! `cargo kani -p tessera-parquet` runs: the range of a slot lies inside
//! its buffer, for every offsets of up to three slots; and the check of a
//! window in one pass passes exactly where each value's checks of UTF-8
//! and of zero bytes pass, for every window of up to two values over a
//! buffer of four bytes, which holds the two ways a boundary of values
//! can cut a character: in the middle of the bytes, and at the start of
//! the second value. The standard library's `from_utf8` is run on both
//! sides: its loads of whole words make the harness take some three
//! minutes on the maintainer's machine, whatever the buffer's size, and a
//! third value four, near CI's limit of ten for a harness; more and longer
//! values are the property test's.

use super::{slot_range, window_passes};

#[kani::proof]
#[kani::unwind(5)]
fn the_range_of_a_slot_lies_in_its_buffer() {
    let offsets: [i64; 4] = kani::any();
    let count: usize = kani::any();
    let len: usize = kani::any();
    let slot: usize = kani::any();
    kani::assume(count <= 4);
    if let Some((start, length)) = slot_range(&offsets[..count], len, slot) {
        assert!(slot + 1 < count);
        assert!(start.checked_add(length).is_some_and(|end| end <= len));
        assert!(start as i64 == offsets[slot] && (start + length) as i64 == offsets[slot + 1]);
    }
    kani::cover!(
        slot_range(&offsets[..count], len, slot).is_some(),
        "a range given"
    );
}

#[kani::proof]
#[kani::unwind(7)]
fn the_one_pass_is_each_value_s_check() {
    let data: [u8; 4] = kani::any();
    let count: usize = kani::any();
    kani::assume((1..=3).contains(&count));
    let offsets: [i64; 3] = kani::any();
    let offsets = &offsets[..count];
    // Offsets that hold: from 0 up, within the buffer.
    for slot in 0..count {
        kani::assume(offsets[slot] >= 0 && offsets[slot] <= data.len() as i64);
        if slot > 0 {
            kani::assume(offsets[slot - 1] <= offsets[slot]);
        }
    }
    let each = (0..count - 1).all(|slot| {
        let value = &data[offsets[slot] as usize..offsets[slot + 1] as usize];
        !value.contains(&0) && core::str::from_utf8(value).is_ok()
    });
    assert!(window_passes(offsets, &data) == each);
    kani::cover!(each && count == 3, "two values that pass");
    kani::cover!(!each && count == 3, "two values, one failing");
}
