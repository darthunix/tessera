#![deny(unsafe_code)]

use std::mem::MaybeUninit;

use anyhow::Result;
use proptest::prelude::*;
use tessera_core::{ColumnReader, ColumnView, RowMask, RowMaskView};
use tessera_kernels::cast::{int4_to_int8, int8_to_int4};
use tessera_kernels::ops::ArithmeticError;
use tessera_testing::{Int, flags, integer, nrows, property, values, words};

/// The same values without bulk storage: every call takes the row path.
struct RowsOnly<'a>(&'a ColumnView<'a, i32>);

impl ColumnReader for RowsOnly<'_> {
    type Value = i32;
    fn nrows(&self) -> usize {
        self.0.nrows()
    }
    fn get(&self, row: usize) -> Result<Option<i32>> {
        ColumnReader::get(self.0, row)
    }
    fn word_values(
        &self,
        word_index: usize,
        selected: u64,
    ) -> Result<impl Iterator<Item = (usize, Option<i32>)> + '_> {
        self.0.word_values(word_index, selected)
    }
}

const SENTINEL: i64 = 0x5a5a_5a5a_5a5a_5a5a;

/// A column, its non-NULL flags (none when `masked` is false) and a
/// selection; the rows a cast reads hold values from `live`, the others the
/// edges of the type.
#[derive(Clone, Debug)]
struct Batch<T> {
    values: Vec<T>,
    non_null: Vec<bool>,
    masked: bool,
    selected: Vec<bool>,
}

fn batches<T: Int>(live: BoxedStrategy<T>) -> impl Strategy<Value = Batch<T>> {
    nrows()
        .prop_flat_map(|nrows| (flags(nrows), flags(nrows), any::<bool>()))
        .prop_flat_map(move |(selected, non_null, masked)| {
            let non_null = if masked {
                non_null
            } else {
                vec![true; selected.len()]
            };
            let read: Vec<bool> = selected
                .iter()
                .zip(&non_null)
                .map(|(&s, &n)| s && n)
                .collect();
            (
                values(&read, &live),
                Just((non_null, masked, selected.clone())),
            )
        })
        .prop_map(|(values, (non_null, masked, selected))| Batch {
            values,
            non_null,
            masked,
            selected,
        })
}

#[allow(unsafe_code)]
fn written(slot: &MaybeUninit<i64>) -> i64 {
    // SAFETY: every slot was created initialized with the sentinel and the
    // kernel only overwrites slots with initialized values.
    unsafe { slot.assume_init() }
}

fn run<C: ColumnReader<Value = i32>>(
    column: &C,
    rows: &RowMaskView<'_>,
) -> Result<(Vec<i64>, Vec<u64>)> {
    let nrows = rows.nrows();
    let mut values = vec![MaybeUninit::new(SENTINEL); nrows];
    let mut words = vec![0; nrows.div_ceil(64)];
    int4_to_int8(
        column,
        rows,
        &mut values,
        &mut RowMask::try_new(nrows, &mut words)?,
    )?;
    Ok((values.iter().map(written).collect(), words))
}

/// Widening keeps every selected non-NULL value and its sign, on the
/// whole-word and the row path; the row path leaves unselected rows alone.
#[test]
fn widening_keeps_the_sign_and_the_nulls_on_every_shape_of_word() {
    property(batches(integer::<i32>()), |batch| -> Result<()> {
        let nrows = batch.selected.len();
        let non_null_words = words(&batch.non_null);
        let non_nulls = batch
            .masked
            .then(|| RowMaskView::try_new(nrows, &non_null_words))
            .transpose()?;
        let column = ColumnView::try_new(&batch.values, non_nulls)?;
        let selection = words(&batch.selected);
        let rows = RowMaskView::try_new(nrows, &selection)?;
        let (out, present) = run(&column, &rows)?;
        let (by_rows, rows_present) = run(&RowsOnly(&column), &rows)?;
        let expected_present: Vec<bool> = (0..nrows)
            .map(|row| batch.selected[row] && batch.non_null[row])
            .collect();
        assert_eq!(present, words(&expected_present));
        assert_eq!(rows_present, present);
        // Rows outside the selection are unspecified: the whole-word path
        // fills its placeholders, the row path leaves them alone.
        for row in 0..nrows {
            if expected_present[row] {
                assert_eq!(out[row], i64::from(batch.values[row]), "row {row}");
                assert_eq!(by_rows[row], out[row], "row {row}");
            } else if !batch.selected[row] {
                assert_eq!(
                    by_rows[row], SENTINEL,
                    "unselected row {row} on the row path"
                );
            }
        }
        Ok(())
    });
}

#[test]
fn dimension_errors_come_before_any_mutation() -> Result<()> {
    let values = [1, 2, 3];
    let column = ColumnView::try_new(&values, None)?;
    let rows = RowMaskView::try_new(3, &[0b111])?;
    let mut out = [MaybeUninit::new(SENTINEL); 3];
    let mut words = [0];
    let short_rows = RowMaskView::try_new(2, &[0b11])?;
    let mut mask = RowMask::try_new(3, &mut words)?;
    assert!(int4_to_int8(&column, &short_rows, &mut out, &mut mask).is_err());
    let mut short_out = [MaybeUninit::new(SENTINEL); 2];
    assert!(int4_to_int8(&column, &rows, &mut short_out, &mut mask).is_err());
    let short_values = [1, 2];
    let short_column = ColumnView::try_new(&short_values, None)?;
    assert!(int4_to_int8(&short_column, &rows, &mut out, &mut mask).is_err());
    assert_eq!(mask.as_view().selected_count(), 0);
    assert!(out.iter().all(|slot| written(slot) == SENTINEL));
    Ok(())
}

#[allow(unsafe_code)]
fn written32(slot: &MaybeUninit<i32>) -> i32 {
    // SAFETY: every slot was created initialized and the kernel only
    // overwrites slots with initialized values.
    unsafe { slot.assume_init() }
}

fn narrow(values: &[i64], nulls: &[bool], selected: &[bool]) -> Result<(Vec<i32>, Vec<u64>)> {
    let nrows = values.len();
    let non_null: Vec<bool> = nulls.iter().map(|null| !null).collect();
    let non_null_words = words(&non_null);
    let column = ColumnView::try_new(values, Some(RowMaskView::try_new(nrows, &non_null_words)?))?;
    let selection = words(selected);
    let rows = RowMaskView::try_new(nrows, &selection)?;
    let mut out = vec![MaybeUninit::new(0x5a5a_5a5a); nrows];
    let mut words = vec![0; nrows.div_ceil(64)];
    int8_to_int4(
        &column,
        &rows,
        &mut out,
        &mut RowMask::try_new(nrows, &mut words)?,
    )?;
    Ok((out.iter().map(written32).collect(), words))
}

/// Narrowing keeps every selected non-NULL value in the int4 range and
/// fails exactly when one lies outside it; the rows it does not read hold
/// int8 edges, past the int4 range, and never fail.
#[test]
fn narrowing_matches_the_model() {
    let live = prop_oneof![integer::<i64>(), integer::<i32>().prop_map(i64::from),].boxed();
    property(batches(live), |batch| -> Result<()> {
        let nrows = batch.selected.len();
        let nulls: Vec<bool> = batch.non_null.iter().map(|non_null| !non_null).collect();
        let outcome = narrow(&batch.values, &nulls, &batch.selected);
        let read = |row: usize| batch.selected[row] && batch.non_null[row];
        let fits = (0..nrows)
            .filter(|&row| read(row))
            .all(|row| i32::try_from(batch.values[row]).is_ok());
        match outcome {
            Ok((out, present)) => {
                assert!(fits, "a value past the int4 range was narrowed");
                for row in 0..nrows {
                    assert_eq!(
                        present[row / 64] >> (row % 64) & 1 == 1,
                        read(row),
                        "row {row}"
                    );
                    if read(row) {
                        assert_eq!(i64::from(out[row]), batch.values[row], "row {row}");
                    }
                }
            }
            Err(error) => {
                assert!(!fits, "{error}");
                assert_eq!(
                    error.downcast_ref::<ArithmeticError>().copied(),
                    Some(ArithmeticError::IntegerOutOfRange)
                );
            }
        }
        Ok(())
    });
}

#[test]
fn narrowing_keeps_every_int4_value_and_the_nulls() -> Result<()> {
    let nrows = 2 * 64 + 5;
    let edges = [i64::from(i32::MIN), -1, 0, 1, i64::from(i32::MAX)];
    let values: Vec<i64> = (0..nrows).map(|row| edges[row % edges.len()]).collect();
    // A NULL placeholder outside the int4 range never counts.
    let nulls: Vec<bool> = (0..nrows).map(|row| row % 7 == 3).collect();
    let values: Vec<i64> = values
        .iter()
        .zip(&nulls)
        .map(|(&value, &null)| if null { 1 << 40 } else { value })
        .collect();
    let selected: Vec<bool> = (0..nrows).map(|row| row % 3 != 1).collect();
    let (out, words) = narrow(&values, &nulls, &selected)?;
    for row in 0..nrows {
        let present = words[row / 64] >> (row % 64) & 1 == 1;
        assert_eq!(present, selected[row] && !nulls[row], "row {row}");
        if present {
            assert_eq!(i64::from(out[row]), values[row], "row {row}");
        }
    }
    Ok(())
}

#[test]
fn narrowing_fails_on_a_selected_value_past_the_int4_range() -> Result<()> {
    for past in [
        i64::from(i32::MAX) + 1,
        i64::from(i32::MIN) - 1,
        i64::MAX,
        i64::MIN,
    ] {
        let values = [1, past, 2];
        // Left out of the selection, the value is never read.
        assert!(narrow(&values, &[false; 3], &[true, false, true]).is_ok());
        let error = narrow(&values, &[false; 3], &[true; 3]).unwrap_err();
        assert_eq!(
            error.downcast_ref::<ArithmeticError>().copied(),
            Some(ArithmeticError::IntegerOutOfRange),
            "{past}"
        );
    }
    Ok(())
}
