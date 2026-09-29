//! The set entry points called with raw pointers, as C calls them: int4
//! and int8 words of the same bits, a NULL row, an unselected row, a whole
//! word of Datums.

use std::ptr;

use tessera_capi::c::{Code, DatumColumn, Mask, Status, tess_int4_in_set, tess_int8_in_set};

fn mask(nrows: usize, words: &mut [u64]) -> Mask {
    Mask {
        nrows: nrows as i32,
        bits: words.as_mut_ptr(),
    }
}

fn column(values: &[u64], isnull: &[bool]) -> DatumColumn {
    DatumColumn {
        struct_size: size_of::<DatumColumn>(),
        values: values.as_ptr(),
        isnull: isnull.as_ptr(),
        nrows: values.len() as i32,
        ..DatumColumn::EMPTY
    }
}

#[test]
fn narrow_and_wide_words() {
    // An int4 word is its low half: 0xffff_ffff is -1 as an int4, not as
    // an int8.
    let values = [u64::from(-1_i32 as u32), 7, 0, u64::MAX, 7];
    let isnull = [false, false, true, false, false];
    let source = column(&values, &isnull);
    let mut selected = [0b0_1111_u64];
    let rows = mask(5, &mut selected);
    let mut status = Status::new();
    // Stale bits are overwritten.
    let (mut found, mut present) = ([0b1_1111_u64], [0b1_0100_u64]);
    // SAFETY: local column, keys, masks and status.
    let code = unsafe {
        tess_int4_in_set(
            &raw const source,
            ptr::null(),
            [-1, 7].as_ptr(),
            2,
            &raw const rows,
            &mut mask(5, &mut found),
            &mut mask(5, &mut present),
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok, "{}", status.message());
    assert_eq!((found[0], present[0]), (0b1011, 0b1011));
    // SAFETY: as above.
    let code = unsafe {
        tess_int8_in_set(
            &raw const source,
            ptr::null(),
            [-1, 7].as_ptr(),
            2,
            &raw const rows,
            &mut mask(5, &mut found),
            &mut mask(5, &mut present),
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok, "{}", status.message());
    assert_eq!((found[0], present[0]), (0b1010, 0b1011));
    // A column of another row count is refused.
    let short = DatumColumn { nrows: 4, ..source };
    // SAFETY: as above.
    let code = unsafe {
        tess_int8_in_set(
            &raw const short,
            ptr::null(),
            [-1, 7].as_ptr(),
            2,
            &raw const rows,
            &mut mask(5, &mut found),
            &mut mask(5, &mut present),
            &raw mut status,
        )
    };
    assert_ne!(code, Code::Ok);
}

/// A full word of Datums, compared whole, against the keys row by row.
#[test]
fn a_whole_word() {
    let values: Vec<u64> = (0..64_u64).map(|row| (row * 5) | (row << 40)).collect();
    let isnull: Vec<bool> = (0..64).map(|row| row % 9 == 4).collect();
    let source = column(&values, &isnull);
    let keys = [0, 15, 20, 55, 300];
    let mut all = [u64::MAX];
    let rows = mask(64, &mut all);
    let (mut found, mut present) = ([0_u64], [0_u64]);
    let mut status = Status::new();
    // SAFETY: local column, keys, masks and status.
    let code = unsafe {
        tess_int4_in_set(
            &raw const source,
            ptr::null(),
            keys.as_ptr(),
            keys.len() as i32,
            &raw const rows,
            &mut mask(64, &mut found),
            &mut mask(64, &mut present),
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok, "{}", status.message());
    let expected_present = (0..64)
        .filter(|&row| !isnull[row])
        .fold(0_u64, |bits, row| bits | 1 << row);
    let expected_found = (0..64)
        .filter(|&row| !isnull[row] && keys.contains(&(row as i32 * 5)))
        .fold(0_u64, |bits, row| bits | 1 << row);
    assert_eq!((found[0], present[0]), (expected_found, expected_present));
}
