#![deny(unsafe_code)]

use std::mem::MaybeUninit;

use anyhow::Result;
use tessera_core::{ColumnReader, ColumnView, RowMask, RowMaskView};
use tessera_kernels::cast::int4_to_int8;

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

fn random(state: &mut u64) -> u64 {
    *state ^= *state >> 12;
    *state ^= *state << 25;
    *state ^= *state >> 27;
    state.wrapping_mul(0x2545_F491_4F6C_DD1D)
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

#[test]
fn widening_keeps_the_sign_and_the_nulls_on_every_shape_of_word() -> Result<()> {
    let mut state = 0x9E37_79B9_7F4A_7C15;
    let nrows = 4 * 64 + 11;
    let values: Vec<i32> = (0..nrows)
        .map(|_| {
            let draw = random(&mut state);
            if draw.is_multiple_of(4) {
                VALUES[(draw >> 8) as usize % VALUES.len()]
            } else {
                (draw >> 8) as i32
            }
        })
        .collect();
    let non_null: Vec<bool> = (0..nrows)
        .map(|_| !random(&mut state).is_multiple_of(5))
        .collect();
    let non_null_words = words_for(&non_null);
    for masked in [false, true] {
        let non_nulls = masked.then(|| RowMaskView::try_new(nrows, &non_null_words).unwrap());
        let column = ColumnView::try_new(&values, non_nulls)?;
        let rows_only = RowsOnly(&column);
        // A full first word puts the call on the whole-word path; later
        // words range from full to sparse, single-row and empty, then the tail.
        let selected: Vec<bool> = (0..nrows)
            .map(|row| match row / 64 {
                0 => true,
                1 => random(&mut state).is_multiple_of(2),
                2 => row % 64 == 5,
                3 => false,
                _ => row % 2 == 0,
            })
            .collect();
        let words = words_for(&selected);
        let rows = RowMaskView::try_new(nrows, &words)?;
        let (out, present) = run(&column, &rows)?;
        let (by_rows, rows_present) = run(&rows_only, &rows)?;
        let expected_present: Vec<bool> = (0..nrows)
            .map(|row| selected[row] && (!masked || non_null[row]))
            .collect();
        assert_eq!(present, words_for(&expected_present), "masked {masked}");
        assert_eq!(rows_present, present, "masked {masked}");
        // Rows outside the selection are unspecified: the whole-word path
        // fills its placeholders, the row path leaves them alone.
        for row in 0..nrows {
            if expected_present[row] {
                assert_eq!(out[row], i64::from(values[row]), "row {row}");
                assert_eq!(by_rows[row], out[row], "row {row}");
            } else if !selected[row] {
                assert_eq!(
                    by_rows[row], SENTINEL,
                    "unselected row {row} on the row path"
                );
            }
        }
    }
    Ok(())
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
