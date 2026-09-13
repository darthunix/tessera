use std::mem::MaybeUninit;

use tessera_capi::{DatumInt32Column, DenseInt32Column};
use tessera_core::{ColumnReader, ColumnView, RowMask, RowMaskView};
use tessera_kernels::int32::{CompareOp, filter};

const OPS: [CompareOp; 6] = [
    CompareOp::Eq,
    CompareOp::Ne,
    CompareOp::Lt,
    CompareOp::Le,
    CompareOp::Gt,
    CompareOp::Ge,
];
const VALUES: [i32; 7] = [i32::MIN, -42, -1, 0, 1, 42, i32::MAX];

fn words_for(flags: &[bool]) -> Vec<u64> {
    let mut words = vec![0; flags.len().div_ceil(64)];
    for (row, &flag) in flags.iter().enumerate() {
        if flag {
            words[row / 64] |= 1 << (row % 64);
        }
    }
    words
}

fn bytes_for(flags: &[bool], offset: usize) -> Vec<u8> {
    // Set out-of-window bits to catch accidental exposure from sliced masks.
    let mut bytes = vec![u8::MAX; (flags.len() + offset).div_ceil(8) + 1];
    for (row, &flag) in flags.iter().enumerate() {
        if !flag {
            bytes[(offset + row) / 8] &= !(1 << ((offset + row) % 8));
        }
    }
    bytes
}

fn filtered<C: ColumnReader<Value = i32>>(
    column: &C,
    selected: &[u64],
    op: CompareOp,
    scalar: i32,
) -> Vec<u64> {
    let mut words = selected.to_vec();
    let mut rows = RowMask::try_new(column.nrows(), &mut words).unwrap();
    filter(column, &mut rows, op, scalar).unwrap();
    // Validates zero padding after mutation as well as exact results below.
    RowMaskView::try_new(column.nrows(), &words).unwrap();
    words
}

#[test]
fn filter_agrees_across_representations_with_uninitialized_gaps_and_slices() {
    for nrows in [0, 1, 63, 64, 65, 1024] {
        let values: Vec<_> = (0..nrows).map(|row| VALUES[row % VALUES.len()]).collect();
        for offset in [0, 5] {
            for kind in 0..4 {
                let ready: Vec<_> = (0..nrows).map(|row| kind == 0 || row % 3 != 0).collect();
                let non_null: Vec<_> = (0..nrows)
                    .map(|row| kind == 0 || (kind != 1 && row % 5 != 0))
                    .collect();
                // Prefixes and gaps intentionally remain uninitialized.
                let mut dense_values = vec![MaybeUninit::uninit(); offset + nrows];
                let mut datum_values = vec![MaybeUninit::uninit(); offset + nrows];
                let mut isnull = vec![MaybeUninit::uninit(); offset + nrows];
                for row in 0..nrows {
                    if ready[row] {
                        isnull[offset + row].write(!non_null[row]);
                        if non_null[row] {
                            dense_values[offset + row].write(values[row]);
                            // PostgreSQL's low 32 bits can be zero- or sign-extended.
                            datum_values[offset + row].write(if row % 2 == 0 {
                                u64::from(values[row] as u32)
                            } else {
                                values[row] as u64
                            });
                        }
                    }
                }
                let ready_bytes = bytes_for(&ready, offset);
                let non_null_bytes = bytes_for(&non_null, offset);
                let prepared = (kind != 0)
                    .then(|| RowMaskView::try_from_bytes(nrows, &ready_bytes, offset).unwrap());
                let non_nulls = (kind != 0)
                    .then(|| RowMaskView::try_from_bytes(nrows, &non_null_bytes, offset).unwrap());
                let column = ColumnView::try_new(&values, non_nulls).unwrap();
                // SAFETY: all prepared non-NULL values have been initialized;
                // the sliced values and masks describe the same physical rows.
                let dense = unsafe {
                    DenseInt32Column::try_new(&dense_values[offset..], non_nulls, prepared)
                }
                .unwrap();
                // SAFETY: flags are initialized for every prepared row, and
                // values for every prepared non-NULL row, with matching slices.
                let datum = unsafe {
                    DatumInt32Column::try_new(&datum_values[offset..], &isnull[offset..], prepared)
                }
                .unwrap();
                let selected: Vec<_> = (0..nrows)
                    .map(|row| ready[row] && (kind != 3 || row % 64 == 1))
                    .collect();
                let selection = words_for(&selected);
                for op in OPS {
                    for scalar in VALUES {
                        let expected = filtered(&column, &selection, op, scalar);
                        assert_eq!(filtered(&dense, &selection, op, scalar), expected);
                        assert_eq!(filtered(&datum, &selection, op, scalar), expected);
                    }
                }
            }
        }
    }
}

fn assert_partial_error<C: ColumnReader<Value = i32>>(column: &C, invalid_word: usize) {
    let mut words = [u64::MAX; 3];
    let mut rows = RowMask::try_new(192, &mut words).unwrap();
    assert!(filter(column, &mut rows, CompareOp::Lt, 10).is_err());
    let mut expected = [u64::MAX; 3];
    for (index, word) in expected.iter_mut().enumerate().take(invalid_word) {
        *word = if index == 0 { (1 << 10) - 1 } else { 0 };
    }
    assert_eq!(words, expected);
    let mut empty = [0; 3];
    let mut rows = RowMask::try_new(192, &mut empty).unwrap();
    filter(column, &mut rows, CompareOp::Eq, 0).unwrap();
    assert_eq!(empty, [0; 3]);
}

#[test]
fn adapters_report_unprepared_rows_before_modifying_their_word() {
    for invalid_word in 0..3 {
        let invalid_row = invalid_word * 64 + 10;
        let mut ready = [u64::MAX; 3];
        ready[invalid_word] &= !(1 << 10);
        let prepared = RowMaskView::try_new(192, &ready).unwrap();
        let mut dense_values = [MaybeUninit::uninit(); 192];
        let mut datum_values = [MaybeUninit::uninit(); 192];
        let mut isnull = [MaybeUninit::uninit(); 192];
        for row in 0..192 {
            if row != invalid_row {
                dense_values[row].write(row as i32);
                datum_values[row].write(row as u64);
                isnull[row].write(false);
            }
        }
        // SAFETY: only the unprepared row is uninitialized, in both layouts.
        let dense =
            unsafe { DenseInt32Column::try_new(&dense_values, None, Some(prepared)) }.unwrap();
        // SAFETY: all prepared rows have initialized values and NULL flags.
        let datum =
            unsafe { DatumInt32Column::try_new(&datum_values, &isnull, Some(prepared)) }.unwrap();
        assert_partial_error(&dense, invalid_word);
        assert_partial_error(&datum, invalid_word);
    }
}
