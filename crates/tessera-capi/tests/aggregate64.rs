//! The int8 extremes and the type-free count over the capi representations: whole words where
//! the batch is fully prepared, rows where it is not, and NULL placeholders
//! that must never reach a result.

use std::mem::MaybeUninit;

use anyhow::Result;
use tessera_capi::{DatumInt64Column, DatumNullsColumn, DenseInt64Column};
use tessera_core::{ColumnReader, RowMaskView};
use tessera_kernels::count::count;
use tessera_kernels::int64::{max, min};

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

fn check<C: ColumnReader<Value = i64>>(
    column: &C,
    rows: &RowMaskView<'_>,
    present: &[i64],
    what: &str,
) -> Result<()> {
    assert_eq!(count(column, rows)?, present.len(), "count {what}");
    assert_eq!(
        min(column, rows)?,
        present.iter().copied().min(),
        "min {what}"
    );
    assert_eq!(
        max(column, rows)?,
        present.iter().copied().max(),
        "max {what}"
    );
    Ok(())
}

#[test]
fn representations_match_the_model_with_placeholders_and_gaps() -> Result<()> {
    let mut state = 0x9E37_79B9_7F4A_7C15;
    let nrows = 3 * 64 + 5;
    let values: Vec<i64> = (0..nrows).map(|_| random(&mut state) as i64).collect();
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
            // NULL placeholders that would show in any aggregate if read:
            // extremes for min/max.
            let placeholder = if row % 2 == 0 { i64::MIN } else { i64::MAX };
            dense_values[row].write(if non_null[row] {
                values[row]
            } else {
                placeholder
            });
            datum_values[row].write(if non_null[row] {
                values[row] as u64
            } else {
                placeholder as u64
            });
        }
        let ready_words = words_for(&ready);
        let non_null_words = words_for(&non_null);
        let prepared = gap.map(|_| RowMaskView::try_new(nrows, &ready_words).unwrap());
        let non_nulls = RowMaskView::try_new(nrows, &non_null_words)?;
        // SAFETY: every prepared row has a value and a flag; the gap is unprepared.
        let dense = unsafe { DenseInt64Column::try_new(&dense_values, Some(non_nulls), prepared) }?;
        // SAFETY: the same holds for Datums and flags.
        let datum = unsafe { DatumInt64Column::try_new(&datum_values, &isnull, prepared) }?;
        // SAFETY: the same flags, read without the values.
        let nulls = unsafe { DatumNullsColumn::try_new(&datum_values, &isnull, prepared) }?;
        for density in [1, 2, 8, 64] {
            let selected: Vec<bool> = (0..nrows)
                .map(|row| ready[row] && random(&mut state).is_multiple_of(density))
                .collect();
            let present: Vec<i64> = (0..nrows)
                .filter(|&row| selected[row] && non_null[row])
                .map(|row| values[row])
                .collect();
            let words = words_for(&selected);
            let rows = RowMaskView::try_new(nrows, &words)?;
            assert_eq!(count(&nulls, &rows)?, present.len(), "nulls 1/{density}");
            check(
                &dense,
                &rows,
                &present,
                &format!("dense 1/{density} {gap:?}"),
            )?;
            check(
                &datum,
                &rows,
                &present,
                &format!("datum 1/{density} {gap:?}"),
            )?;
        }
        // Words that are all NULL: the placeholders alone, nothing results.
        let mut only_nulls = vec![false; nrows];
        for row in (0..128).filter(|&row| ready[row] && !non_null[row]) {
            only_nulls[row] = true;
        }
        let words = words_for(&only_nulls);
        let rows = RowMaskView::try_new(nrows, &words)?;
        check(&dense, &rows, &[], "dense NULL rows")?;
        check(&datum, &rows, &[], "datum NULL rows")?;
    }
    Ok(())
}

#[test]
fn unprepared_selected_rows_are_errors() -> Result<()> {
    let values = [MaybeUninit::new(1); 64];
    let datums = [MaybeUninit::new(1_u64); 64];
    let flags = [MaybeUninit::new(false); 64];
    let prepared = RowMaskView::try_new(64, &[u64::MAX >> 1])?;
    // SAFETY: the one unprepared row is never selected below except to fail.
    let dense = unsafe { DenseInt64Column::try_new(&values, None, Some(prepared)) }?;
    // SAFETY: as above.
    let datum = unsafe { DatumInt64Column::try_new(&datums, &flags, Some(prepared)) }?;
    let rows = RowMaskView::try_new(64, &[u64::MAX])?;
    assert!(min(&dense, &rows).is_err());
    assert!(count(&datum, &rows).is_err());
    let rows = RowMaskView::try_new(64, &[u64::MAX >> 1])?;
    assert_eq!(max(&dense, &rows)?, Some(1));
    assert_eq!(count(&datum, &rows)?, 63);
    Ok(())
}
