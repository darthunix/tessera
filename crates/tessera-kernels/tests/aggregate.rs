#![forbid(unsafe_code)]

use std::fmt::Debug;

use anyhow::Result;
use tessera_core::{ColumnReader, ColumnView, RowMaskView};
use tessera_kernels::count::count;
use tessera_kernels::{int32, int64};

/// The same values without bulk storage: every call takes the row path.
struct RowsOnly<'a, T>(ColumnView<'a, T>);

impl<T: Copy> ColumnReader for RowsOnly<'_, T> {
    type Value = T;
    fn nrows(&self) -> usize {
        self.0.nrows()
    }
    fn get(&self, row: usize) -> Result<Option<T>> {
        ColumnReader::get(&self.0, row)
    }
    fn word_values(
        &self,
        word_index: usize,
        selected: u64,
    ) -> Result<impl Iterator<Item = (usize, Option<T>)> + '_> {
        self.0.word_values(word_index, selected)
    }
}

/// One width under test, int4 or int8: its values, how the tests draw them,
/// and its kernels.
trait Lane: Copy + Ord + Debug + From<i32> + 'static {
    /// The type in failure messages.
    const NAME: &'static str;
    const MIN: Self;
    /// Extremes and small values that a share of the random rows takes.
    const VALUES: &'static [Self];
    /// Whether the width has a sum kernel: int4 sums into int8, int8 sums
    /// into numeric elsewhere.
    const SUMS: bool;
    /// A value from a small range for the model test; int8 scales it past
    /// the int4 range.
    fn small(draw: u64) -> Self;
    /// A value from the whole range for the bulk test.
    fn wide(draw: u64) -> Self;
    /// A value as the sum adds it.
    fn widen(self) -> i64;
    /// The width's `min` kernel.
    fn least<C: ColumnReader<Value = Self>>(
        column: &C,
        rows: &RowMaskView<'_>,
    ) -> Result<Option<Self>>;
    /// The width's `max` kernel.
    fn greatest<C: ColumnReader<Value = Self>>(
        column: &C,
        rows: &RowMaskView<'_>,
    ) -> Result<Option<Self>>;
    /// The sum kernel's result, `None` without a sum kernel.
    fn sum<C: ColumnReader<Value = Self>>(
        column: &C,
        rows: &RowMaskView<'_>,
    ) -> Result<Option<Option<i64>>>;
}

impl Lane for i32 {
    const NAME: &'static str = "int4";
    const MIN: Self = i32::MIN;
    const VALUES: &'static [Self] = &[i32::MIN, -42, -1, 0, 1, 42, i32::MAX];
    const SUMS: bool = true;
    fn small(draw: u64) -> Self {
        (draw >> 8) as i32 % 1000
    }
    fn wide(draw: u64) -> Self {
        (draw >> 8) as i32
    }
    fn widen(self) -> i64 {
        i64::from(self)
    }
    fn least<C: ColumnReader<Value = Self>>(
        column: &C,
        rows: &RowMaskView<'_>,
    ) -> Result<Option<Self>> {
        int32::min(column, rows)
    }
    fn greatest<C: ColumnReader<Value = Self>>(
        column: &C,
        rows: &RowMaskView<'_>,
    ) -> Result<Option<Self>> {
        int32::max(column, rows)
    }
    fn sum<C: ColumnReader<Value = Self>>(
        column: &C,
        rows: &RowMaskView<'_>,
    ) -> Result<Option<Option<i64>>> {
        int32::sum(column, rows).map(Some)
    }
}

impl Lane for i64 {
    const NAME: &'static str = "int8";
    const MIN: Self = i64::MIN;
    const VALUES: &'static [Self] = &[i64::MIN, -(1 << 40), -42, -1, 0, 1, 42, 1 << 40, i64::MAX];
    const SUMS: bool = false;
    fn small(draw: u64) -> Self {
        ((draw >> 8) as i64 % 1000) << 33
    }
    fn wide(draw: u64) -> Self {
        draw as i64
    }
    fn widen(self) -> i64 {
        self
    }
    fn least<C: ColumnReader<Value = Self>>(
        column: &C,
        rows: &RowMaskView<'_>,
    ) -> Result<Option<Self>> {
        int64::min(column, rows)
    }
    fn greatest<C: ColumnReader<Value = Self>>(
        column: &C,
        rows: &RowMaskView<'_>,
    ) -> Result<Option<Self>> {
        int64::max(column, rows)
    }
    fn sum<C: ColumnReader<Value = Self>>(
        _: &C,
        _: &RowMaskView<'_>,
    ) -> Result<Option<Option<i64>>> {
        Ok(None)
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

/// xorshift64*, fixed seed: the same data on every run.
fn random(state: &mut u64) -> u64 {
    *state ^= *state >> 12;
    *state ^= *state << 25;
    *state ^= *state >> 27;
    state.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

struct Model<T> {
    count: usize,
    /// `None` without a sum kernel.
    sum: Option<Option<i64>>,
    min: Option<T>,
    max: Option<T>,
}

fn model<W: Lane>(values: &[W], selected: &[bool], non_null: &[bool]) -> Model<W> {
    let present: Vec<W> = (0..values.len())
        .filter(|&row| selected[row] && non_null[row])
        .map(|row| values[row])
        .collect();
    Model {
        count: present.len(),
        sum: W::SUMS
            .then(|| (!present.is_empty()).then(|| present.iter().map(|&v| v.widen()).sum())),
        min: present.iter().copied().min(),
        max: present.iter().copied().max(),
    }
}

fn assert_aggregates<W: Lane>(
    column: &ColumnView<'_, W>,
    rows: &RowMaskView<'_>,
    expected: &Model<W>,
    what: &str,
) -> Result<()> {
    assert_eq!(count(column, rows)?, expected.count, "count {what}");
    assert_eq!(W::sum(column, rows)?, expected.sum, "sum {what}");
    assert_eq!(W::least(column, rows)?, expected.min, "min {what}");
    assert_eq!(W::greatest(column, rows)?, expected.max, "max {what}");
    Ok(())
}

fn random_data<W: Lane>() -> Result<()> {
    let mut state = 0x9E37_79B9_7F4A_7C15;
    for nrows in [0, 1, 63, 64, 65, 130, 1024] {
        let values: Vec<W> = (0..nrows)
            .map(|_| {
                let draw = random(&mut state);
                if draw.is_multiple_of(4) {
                    W::VALUES[(draw >> 8) as usize % W::VALUES.len()]
                } else {
                    W::small(draw)
                }
            })
            .collect();
        let non_null: Vec<bool> = (0..nrows)
            .map(|_| !random(&mut state).is_multiple_of(3))
            .collect();
        let non_null_words = words_for(&non_null);
        for masked in [false, true] {
            let non_nulls = masked.then(|| RowMaskView::try_new(nrows, &non_null_words).unwrap());
            let column = ColumnView::try_new(&values, non_nulls)?;
            let all_present = vec![true; nrows];
            let flags = if masked { &non_null } else { &all_present };
            for density in [1, 2, 8, 64] {
                let selected: Vec<bool> = (0..nrows)
                    .map(|_| random(&mut state).is_multiple_of(density))
                    .collect();
                let words = words_for(&selected);
                let rows = RowMaskView::try_new(nrows, &words)?;
                let expected = model(&values, &selected, flags);
                assert_aggregates(
                    &column,
                    &rows,
                    &expected,
                    &format!("{} {nrows} rows, 1/{density}, masked {masked}", W::NAME),
                )?;
            }
        }
    }
    Ok(())
}

#[test]
fn aggregates_match_the_model_on_random_data() -> Result<()> {
    random_data::<i32>()?;
    random_data::<i64>()
}

fn bulk_words<W: Lane>() -> Result<()> {
    let name = W::NAME;
    let mut state = 0x2545_F491_4F6C_DD1D;
    let nrows = 5 * 64 + 9;
    let values: Vec<W> = (0..nrows)
        .map(|_| {
            let draw = random(&mut state);
            if draw.is_multiple_of(5) {
                W::VALUES[(draw >> 8) as usize % W::VALUES.len()]
            } else {
                W::wide(draw)
            }
        })
        .collect();
    let non_null: Vec<bool> = (0..nrows)
        .map(|_| !random(&mut state).is_multiple_of(4))
        .collect();
    let non_null_words = words_for(&non_null);
    let non_nulls = RowMaskView::try_new(nrows, &non_null_words)?;
    let column = ColumnView::try_new(&values, Some(non_nulls))?;
    let rows_only = RowsOnly(ColumnView::try_new(&values, Some(non_nulls))?);
    // A full first word puts the call on the whole-word path; the later
    // words range from full to sparse, single-row and empty.
    let mut selected: Vec<bool> = (0..nrows)
        .map(|row| match row / 64 {
            0 | 1 => true,
            2 => random(&mut state).is_multiple_of(2),
            3 => row % 64 == 17,
            4 => false,
            _ => row % 3 == 0,
        })
        .collect();
    for pass in 0..2 {
        if pass == 1 {
            // Every word NULL-free rows only: the unmasked kernels.
            selected = (0..nrows).map(|row| non_null[row]).collect();
        }
        let words = words_for(&selected);
        let rows = RowMaskView::try_new(nrows, &words)?;
        assert_eq!(count(&column, &rows)?, count(&rows_only, &rows)?, "{name}");
        assert_eq!(
            W::sum(&column, &rows)?,
            W::sum(&rows_only, &rows)?,
            "{name}"
        );
        assert_eq!(
            W::least(&column, &rows)?,
            W::least(&rows_only, &rows)?,
            "{name}"
        );
        assert_eq!(
            W::greatest(&column, &rows)?,
            W::greatest(&rows_only, &rows)?,
            "{name}"
        );
        let expected = model(&values, &selected, &non_null);
        assert_aggregates(&column, &rows, &expected, &format!("{name} mixed words"))?;
    }
    Ok(())
}

#[test]
fn bulk_words_agree_with_the_row_path() -> Result<()> {
    bulk_words::<i32>()?;
    bulk_words::<i64>()
}

fn empty_and_all_null<W: Lane>() -> Result<()> {
    let name = W::NAME;
    let values = [W::MIN; 130];
    let column = ColumnView::try_new(&values, None)?;
    let none = RowMaskView::try_new(130, &[0, 0, 0])?;
    assert_eq!(count(&column, &none)?, 0, "{name}");
    assert_eq!(W::sum(&column, &none)?, W::SUMS.then_some(None), "{name}");
    assert_eq!(W::least(&column, &none)?, None, "{name}");
    assert_eq!(W::greatest(&column, &none)?, None, "{name}");
    let all = RowMaskView::try_new(130, &[u64::MAX, u64::MAX, 3])?;
    assert_eq!(count(&column, &all)?, 130, "{name}");
    assert_eq!(
        W::sum(&column, &all)?,
        W::SUMS.then(|| Some(130 * W::MIN.widen())),
        "{name}"
    );
    assert_eq!(W::least(&column, &all)?, Some(W::MIN), "{name}");
    assert_eq!(W::greatest(&column, &all)?, Some(W::MIN), "{name}");
    // Every selected row NULL: nothing, and the values are never read.
    let nulls = RowMaskView::try_new(130, &[0, 0, 0])?;
    let column = ColumnView::try_new(&values, Some(nulls))?;
    assert_eq!(count(&column, &all)?, 0, "{name}");
    assert_eq!(W::sum(&column, &all)?, W::SUMS.then_some(None), "{name}");
    assert_eq!(W::least(&column, &all)?, None, "{name}");
    assert_eq!(W::greatest(&column, &all)?, None, "{name}");
    Ok(())
}

#[test]
fn empty_and_all_null_selections_give_nothing_and_extremes_do_not_overflow() -> Result<()> {
    empty_and_all_null::<i32>()?;
    empty_and_all_null::<i64>()
}

/// The count needs no values: a column of units counts like any other.
#[test]
fn count_reads_no_values() -> Result<()> {
    let units = [(); 200];
    let non_null: Vec<bool> = (0..200).map(|row| row % 3 != 0).collect();
    let non_null_words = words_for(&non_null);
    let column = ColumnView::try_new(&units, Some(RowMaskView::try_new(200, &non_null_words)?))?;
    for selected in [u64::MAX, 0b1010_1010, 1 << 7] {
        let words = [selected, selected, selected, selected & ((1 << 8) - 1)];
        let rows = RowMaskView::try_new(200, &words)?;
        let expected = (0..200)
            .filter(|&row| words[row / 64] & (1 << (row % 64)) != 0 && non_null[row])
            .count();
        assert_eq!(count(&column, &rows)?, expected, "{selected:#b}");
    }
    Ok(())
}

fn row_count_mismatches<W: Lane>() -> Result<()> {
    let name = W::NAME;
    let values = [W::from(1); 64];
    let column = ColumnView::try_new(&values, None)?;
    let rows = RowMaskView::try_new(65, &[u64::MAX, 1])?;
    assert!(count(&column, &rows).is_err(), "{name}");
    // Without a sum kernel there is nothing to fail.
    assert_eq!(W::sum(&column, &rows).is_err(), W::SUMS, "{name}");
    assert!(W::least(&column, &rows).is_err(), "{name}");
    assert!(W::greatest(&column, &rows).is_err(), "{name}");
    Ok(())
}

#[test]
fn row_count_mismatches_are_errors() -> Result<()> {
    row_count_mismatches::<i32>()?;
    row_count_mismatches::<i64>()
}
