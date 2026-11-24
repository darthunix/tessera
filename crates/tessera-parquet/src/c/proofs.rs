//! The proof by Kani of the one piece of arithmetic of the bytes form of
//! its own, `fill_rows`: `cargo kani -p tessera-parquet` runs it. The
//! bytes a window gives are slices that `datum-arrow`'s read checked
//! against the window's offsets, so their bounds hold by Rust's types;
//! what the proof shows is that the places and counts written for the C
//! side, for every window of up to three rows over a buffer of eight
//! bytes, every mask and every row's place in it, stay inside that buffer
//! (an empty value's place is that of `EMPTY`), that the indexes written
//! are the keys given, that a row not asked or `NULL` is left as it was,
//! and that no call panics: each output's length up to three, keys and
//! indexes each given or not and of any such length, apart, and any row
//! without its bytes or none, a refusal exactly when one is short, only
//! one of the two is given or a row asked and not `NULL` lacks its bytes,
//! writing nothing, for a `value` that gives a row the same answer each
//! time, as `fill_rows` requires. Longer windows and outputs are the property test's. The
//! lifetime of those bytes across the C boundary is Miri's and the
//! sanitizers', in the tests.

use super::{EMPTY, fill_rows};

#[kani::proof]
#[kani::unwind(5)]
fn the_places_of_a_window_s_bytes_lie_in_its_buffer() {
    let buffer: [u8; 8] = kani::any();
    let rows: usize = kani::any();
    kani::assume(rows <= 3);
    let mut values: [&[u8]; 3] = [&[]; 3];
    for value in values.iter_mut().take(rows) {
        let start: usize = kani::any();
        let end: usize = kani::any();
        kani::assume(start <= end && end <= buffer.len());
        *value = &buffer[start..end];
    }
    let values = &values[..rows];
    let asked: [u64; 1] = kani::any();
    let made: [u64; 1] = kani::any();
    let keys: [i32; 3] = kani::any();
    // Each output's length, and whether keys and indexes are given and of
    // what length, apart.
    let data_len: usize = kani::any();
    let lengths_len: usize = kani::any();
    let keys_len: usize = kani::any();
    let indexes_len: usize = kani::any();
    let with_keys: bool = kani::any();
    let with_indexes: bool = kani::any();
    // The row whose bytes are not given, none at 3.
    let missing: usize = kani::any();
    kani::assume(missing <= 3);
    kani::assume(data_len <= 3 && lengths_len <= 3 && keys_len <= 3 && indexes_len <= 3);
    let mut data = [core::ptr::null::<u8>(); 3];
    let mut lengths = [u32::MAX; 3];
    let mut indexes = [-1i32; 3];
    let result = fill_rows(
        rows,
        |row| values.get(row).copied().filter(|_| row != missing),
        &made,
        &asked,
        with_keys.then_some(&keys[..keys_len]),
        &mut data[..data_len],
        &mut lengths[..lengths_len],
        with_indexes.then_some(&mut indexes[..indexes_len]),
    );
    let missed = missing < rows && asked[0] >> missing & 1 == 1 && made[0] >> missing & 1 == 0;
    let refused = data_len < rows
        || lengths_len < rows
        || (with_keys && keys_len < rows)
        || (with_indexes && indexes_len < rows)
        || with_keys != with_indexes
        || missed;
    assert!(result.is_err() == refused);
    let base = buffer.as_ptr() as usize;
    for row in 0..3 {
        let wanted = !refused && row < rows && asked[0] >> row & 1 == 1 && made[0] >> row & 1 == 0;
        if !wanted {
            assert!(data[row].is_null() && lengths[row] == u32::MAX && indexes[row] == -1);
            continue;
        }
        let len = lengths[row] as usize;
        assert!(len == values[row].len());
        if len == 0 {
            assert!(core::ptr::eq(data[row], &EMPTY));
        } else {
            let at = data[row] as usize;
            assert!(at >= base && at + len <= base + buffer.len());
        }
        if with_keys {
            assert!(indexes[row] == keys[row]);
        } else {
            assert!(indexes[row] == -1);
        }
    }
    kani::cover!(
        !refused && rows == 3 && asked[0] & 7 == 7 && made[0] & 7 == 0,
        "three rows asked"
    );
    kani::cover!(
        !refused && rows > 0 && values[0].is_empty() && asked[0] & 1 == 1 && made[0] & 1 == 0,
        "an empty value asked"
    );
    kani::cover!(
        refused && data_len >= rows && lengths_len >= rows && !missed,
        "keys without indexes refused"
    );
    kani::cover!(
        missed && data_len >= rows && lengths_len >= rows && with_keys == with_indexes,
        "a row without its bytes refused"
    );
}
