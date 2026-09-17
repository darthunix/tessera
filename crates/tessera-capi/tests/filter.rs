//! The int32 filter over the capi representations: full prepared words take
//! the vector path, words with an unprepared row the row path, and both must
//! match the scalar model.

use std::mem::MaybeUninit;

use tessera_capi::{DatumInt32Column, DenseInt32Column};
use tessera_core::{RowMask, RowMaskView};
use tessera_kernels::int32::{CompareOp, filter};

const OPS: [CompareOp; 6] = [
    CompareOp::Eq,
    CompareOp::Ne,
    CompareOp::Lt,
    CompareOp::Le,
    CompareOp::Gt,
    CompareOp::Ge,
];

fn compare(value: i32, op: CompareOp, scalar: i32) -> bool {
    match op {
        CompareOp::Eq => value == scalar,
        CompareOp::Ne => value != scalar,
        CompareOp::Lt => value < scalar,
        CompareOp::Le => value <= scalar,
        CompareOp::Gt => value > scalar,
        CompareOp::Ge => value >= scalar,
    }
}

fn words_for(flags: &[bool]) -> Vec<u64> {
    let mut words = vec![0; flags.len().div_ceil(64)];
    for (row, &flag) in flags.iter().enumerate() {
        if flag {
            words[row / 64] |= 1 << (row % 64);
        }
    }
    words
}

fn random(state: &mut u64) -> u64 {
    *state ^= *state >> 12;
    *state ^= *state << 25;
    *state ^= *state >> 27;
    state.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

#[test]
fn representations_match_the_scalar_model_on_bulk_and_row_words() {
    let mut state = 0x2545_F491_4F6C_DD1D;
    let nrows = 3 * 64 + 5;
    let boundary = [i32::MIN, -1, 0, 1, i32::MAX];
    let values: Vec<i32> = (0..nrows)
        .map(|_| {
            let draw = random(&mut state);
            if draw.is_multiple_of(5) {
                boundary[(draw >> 8) as usize % boundary.len()]
            } else {
                (draw >> 8) as i32 % 50
            }
        })
        .collect();
    let non_null: Vec<bool> = (0..nrows)
        .map(|_| !random(&mut state).is_multiple_of(3))
        .collect();
    // Word 1 has an unprepared row and takes the row path; the others are bulk.
    for gap in [None, Some(64 + 30)] {
        let ready: Vec<bool> = (0..nrows).map(|row| Some(row) != gap).collect();
        let mut dense_values = vec![MaybeUninit::uninit(); nrows];
        let mut datum_values = vec![MaybeUninit::uninit(); nrows];
        let mut isnull = vec![MaybeUninit::uninit(); nrows];
        for row in (0..nrows).filter(|&row| ready[row]) {
            isnull[row].write(!non_null[row]);
            // A NULL row holds a placeholder that would pass any comparison
            // if it were not masked.
            dense_values[row].write(if non_null[row] { values[row] } else { 0 });
            datum_values[row].write(if non_null[row] {
                // High bits are not part of an int4 Datum and must be ignored.
                (values[row] as u32 as u64) | (random(&mut state) << 32)
            } else {
                0
            });
        }
        let ready_words = words_for(&ready);
        let non_null_words = words_for(&non_null);
        let prepared = gap.map(|_| RowMaskView::try_new(nrows, &ready_words).unwrap());
        let non_nulls = RowMaskView::try_new(nrows, &non_null_words).unwrap();
        // SAFETY: every prepared row has a value and a flag; the gap is unprepared.
        let dense =
            unsafe { DenseInt32Column::try_new(&dense_values, Some(non_nulls), prepared) }.unwrap();
        // SAFETY: the same holds for Datums and flags.
        let datum = unsafe { DatumInt32Column::try_new(&datum_values, &isnull, prepared) }.unwrap();
        for op in OPS {
            for scalar in [i32::MIN, -50, -1, 0, 1, 25, 49, i32::MAX] {
                let selected: Vec<bool> = (0..nrows)
                    .map(|row| ready[row] && random(&mut state).is_multiple_of(2))
                    .collect();
                let expected: Vec<bool> = (0..nrows)
                    .map(|row| selected[row] && non_null[row] && compare(values[row], op, scalar))
                    .collect();
                let mut dense_words = words_for(&selected);
                filter(
                    &dense,
                    &mut RowMask::try_new(nrows, &mut dense_words).unwrap(),
                    op,
                    scalar,
                )
                .unwrap();
                assert_eq!(
                    dense_words,
                    words_for(&expected),
                    "dense {op:?} {scalar} {gap:?}"
                );
                let mut datum_words = words_for(&selected);
                filter(
                    &datum,
                    &mut RowMask::try_new(nrows, &mut datum_words).unwrap(),
                    op,
                    scalar,
                )
                .unwrap();
                assert_eq!(
                    datum_words,
                    words_for(&expected),
                    "datum {op:?} {scalar} {gap:?}"
                );
            }
        }
    }
}
