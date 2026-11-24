//! The proof by Kani of the bytes form's own arithmetic, which
//! `cargo kani -p tessera-parquet` runs: the range of a slot lies inside
//! its buffer, for every offsets of up to three slots.

use super::slot_range;

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
