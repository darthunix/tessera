//! The int4 arithmetic over the capi representations: whole words where
//! the batch is fully prepared, rows where it is not, and placeholders in
//! NULL cells that would overflow or divide by zero if they were used.

use std::mem::MaybeUninit;

use anyhow::Result;
use tessera_capi::{DatumInt32Column, DenseInt32Column};
use tessera_core::{ColumnReader, RowMask, RowMaskView};
use tessera_kernels::int32::{
    ArithOp, ArithmeticError, arith_columns, arith_scalar, arith_scalar_left,
};

const OPS: [ArithOp; 5] = [
    ArithOp::Add,
    ArithOp::Sub,
    ArithOp::Mul,
    ArithOp::Div,
    ArithOp::Mod,
];
const SCALAR: i32 = 7;

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

fn model(op: ArithOp, a: i32, b: i32) -> Result<i32, ArithmeticError> {
    let (a, b) = (i64::from(a), i64::from(b));
    let wide = match op {
        ArithOp::Add => a + b,
        ArithOp::Sub => a - b,
        ArithOp::Mul => a * b,
        ArithOp::Div | ArithOp::Mod if b == 0 => return Err(ArithmeticError::DivisionByZero),
        ArithOp::Div => a / b,
        ArithOp::Mod if b == -1 => 0,
        ArithOp::Mod => a % b,
    };
    i32::try_from(wide).map_err(|_| ArithmeticError::IntegerOutOfRange)
}

/// Every kernel shape against the model: `column op 7`, `7 op column` and
/// `column op column` with the column as both sides.
fn check<C: ColumnReader<Value = i32>>(
    column: &C,
    rows: &RowMaskView<'_>,
    values: &[i32],
    present: &[bool],
    what: &str,
) -> Result<()> {
    let nrows = values.len();
    for op in OPS {
        for shape in 0..3 {
            let mut out = vec![MaybeUninit::uninit(); nrows];
            let mut words = vec![0; nrows.div_ceil(64)];
            let mut mask = RowMask::try_new(nrows, &mut words)?;
            let outcome = match shape {
                0 => arith_scalar(op, column, SCALAR, rows, &mut out, &mut mask),
                1 => arith_scalar_left(op, SCALAR, column, rows, &mut out, &mut mask),
                _ => arith_columns(op, column, column, rows, &mut out, &mut mask),
            };
            let expected: Vec<Option<Result<i32, ArithmeticError>>> = (0..nrows)
                .map(|row| {
                    present[row].then(|| match shape {
                        0 => model(op, values[row], SCALAR),
                        1 => model(op, SCALAR, values[row]),
                        _ => model(op, values[row], values[row]),
                    })
                })
                .collect();
            let failure = expected.iter().flatten().find_map(|result| result.err());
            match (failure, outcome) {
                (Some(_), Err(error)) => {
                    assert!(
                        error.downcast_ref::<ArithmeticError>().is_some(),
                        "{what} {op:?} {shape}"
                    );
                }
                (None, Ok(())) => {
                    assert_eq!(words, words_for(present), "{what} {op:?} {shape}");
                    for (row, expected) in expected.iter().enumerate() {
                        if let Some(Ok(value)) = expected {
                            // SAFETY: the mask marks the row, so the kernel wrote it.
                            let written = unsafe { out[row].assume_init() };
                            assert_eq!(written, *value, "{what} {op:?} {shape} row {row}");
                        }
                    }
                }
                (failure, outcome) => {
                    panic!("{what} {op:?} {shape}: model {failure:?}, kernel {outcome:?}")
                }
            }
        }
    }
    Ok(())
}

#[test]
fn representations_match_the_model_with_placeholders_and_gaps() -> Result<()> {
    let mut state = 0x9E37_79B9_7F4A_7C15;
    let nrows = 3 * 64 + 5;
    // Small values, so that nothing but a placeholder can overflow.
    let values: Vec<i32> = (0..nrows)
        .map(|_| (random(&mut state) >> 8) as i32 % 20_000)
        .collect();
    let non_null: Vec<bool> = (0..nrows)
        .map(|_| !random(&mut state).is_multiple_of(3))
        .collect();
    for gap in [None, Some(64 + 30)] {
        let ready: Vec<bool> = (0..nrows).map(|row| Some(row) != gap).collect();
        let mut dense_values = vec![MaybeUninit::uninit(); nrows];
        let mut datum_values = vec![MaybeUninit::uninit(); nrows];
        let mut isnull = vec![MaybeUninit::uninit(); nrows];
        for row in (0..nrows).filter(|&row| ready[row]) {
            isnull[row].write(!non_null[row]);
            // A NULL placeholder that overflows with every operation, or a
            // zero that would divide by zero if it were used.
            let placeholder = if row % 2 == 0 { i32::MIN } else { 0 };
            dense_values[row].write(if non_null[row] {
                values[row]
            } else {
                placeholder
            });
            datum_values[row].write(if non_null[row] {
                (values[row] as u32 as u64) | (random(&mut state) << 32)
            } else {
                placeholder as u32 as u64
            });
        }
        let ready_words = words_for(&ready);
        let non_null_words = words_for(&non_null);
        let prepared = gap.map(|_| RowMaskView::try_new(nrows, &ready_words).unwrap());
        let non_nulls = RowMaskView::try_new(nrows, &non_null_words)?;
        // SAFETY: every prepared row has a value and a flag; the gap is unprepared.
        let dense = unsafe { DenseInt32Column::try_new(&dense_values, Some(non_nulls), prepared) }?;
        // SAFETY: the same holds for Datums and flags.
        let datum = unsafe { DatumInt32Column::try_new(&datum_values, &isnull, prepared) }?;
        for density in [1, 2, 8] {
            let selected: Vec<bool> = (0..nrows)
                .map(|row| ready[row] && random(&mut state).is_multiple_of(density))
                .collect();
            let present: Vec<bool> = (0..nrows)
                .map(|row| selected[row] && non_null[row])
                .collect();
            let words = words_for(&selected);
            let rows = RowMaskView::try_new(nrows, &words)?;
            check(
                &dense,
                &rows,
                &values,
                &present,
                &format!("dense 1/{density} {gap:?}"),
            )?;
            check(
                &datum,
                &rows,
                &values,
                &present,
                &format!("datum 1/{density} {gap:?}"),
            )?;
        }
    }
    Ok(())
}

#[test]
fn unprepared_selected_rows_are_errors() -> Result<()> {
    let values = [MaybeUninit::new(1); 64];
    let datums = [MaybeUninit::new(1_u64); 64];
    let flags = [MaybeUninit::new(false); 64];
    let prepared = RowMaskView::try_new(64, &[u64::MAX >> 1])?;
    // SAFETY: the one unprepared row is selected only to fail.
    let dense = unsafe { DenseInt32Column::try_new(&values, None, Some(prepared)) }?;
    // SAFETY: as above.
    let datum = unsafe { DatumInt32Column::try_new(&datums, &flags, Some(prepared)) }?;
    let mut out = [MaybeUninit::uninit(); 64];
    let mut words = [0];
    let rows = RowMaskView::try_new(64, &[u64::MAX])?;
    let mut mask = RowMask::try_new(64, &mut words)?;
    assert!(arith_scalar(ArithOp::Add, &dense, 1, &rows, &mut out, &mut mask).is_err());
    assert!(arith_scalar(ArithOp::Add, &datum, 1, &rows, &mut out, &mut mask).is_err());
    let rows = RowMaskView::try_new(64, &[u64::MAX >> 1])?;
    arith_scalar(ArithOp::Add, &dense, 1, &rows, &mut out, &mut mask)?;
    assert_eq!(mask.as_view().selected_count(), 63);
    Ok(())
}
