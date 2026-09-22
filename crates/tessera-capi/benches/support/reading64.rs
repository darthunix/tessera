//! The int8 reading inputs shared by the int8 arithmetic and aggregate
//! benchmarks: the reading benchmark's case matrix with values shifted
//! past the int4 range, and independent scalar sums as the references.

use anyhow::{Result, ensure};
use tessera_core::RowMaskView;

use crate::support::{
    fixture::{Bitmap, Fixture},
    reference::Mask,
};

/// The borrowed input of every measured entry point, as the reading
/// benchmark builds it for int4.
pub struct Input<'a, C> {
    pub column: &'a C,
    pub rows: RowMaskView<'a>,
    pub values: &'a [i64],
    pub datums: &'a [u64],
    pub nulls: &'a [bool],
    pub reference_rows: Mask<'a>,
    pub prepared: Option<Mask<'a>>,
    pub non_nulls: Option<Mask<'a>>,
}

impl<'a, C> Input<'a, C> {
    pub fn new(column: &'a C, case: &'a Fixture<i64>) -> Self {
        Self {
            column,
            rows: case.selected.view(),
            values: &case.values,
            datums: &case.datums,
            nulls: &case.nulls,
            reference_rows: case.selected.reference(),
            prepared: case.prepared.as_ref().map(Bitmap::reference),
            non_nulls: case.non_nulls.as_ref().map(Bitmap::reference),
        }
    }
}

fn case(
    nrows: usize,
    pattern: &str,
    nulls: &str,
    offset: Option<usize>,
    partial: bool,
) -> Fixture<i64> {
    // The reading values shifted past the int4 range; no operation below
    // overflows them.
    let values = (0..nrows)
        .map(|row| ((row as i64).wrapping_mul(7919).wrapping_sub(104_729)) << 20)
        .collect();
    Fixture::from_values(values, pattern, nulls, offset, partial)
}

/// The reading benchmark's case matrix.
pub fn cases() -> Vec<Fixture<i64>> {
    let mut cases = Vec::new();
    for nulls in ["none", "mixed"] {
        for pattern in ["all", "half", "sparse", "empty"] {
            cases.push(case(1024, pattern, nulls, None, false));
        }
    }
    for nrows in [0, 1, 63, 64, 65] {
        cases.push(case(nrows, "all", "mixed", None, false));
    }
    cases.push(case(1024, "all", "all", None, false));
    cases.push(case(1024, "all", "random", None, false));
    cases.push(case(1024, "random", "random", None, false));
    cases.push(case(1024, "all", "mixed", None, true));
    for (nrows, offset, pattern) in [(65, 3, "all"), (1024, 7, "all"), (1024, 7, "sparse")] {
        cases.push(case(nrows, pattern, "mixed", Some(offset), true));
    }
    cases
}

/// `value` summed over the selected non-NULL rows, straight from the dense
/// fixture buffers and their masks.
#[inline(never)]
pub fn dense_reference<C>(input: &Input<'_, C>) -> Result<i64> {
    let rows = input.reference_rows;
    ensure!(input.values.len() == rows.nrows, "row counts differ");
    let mut sum = 0;
    for index in 0..rows.nrows.div_ceil(64) {
        let mut selected = rows.word(index);
        if selected == 0 {
            continue;
        }
        if let Some(prepared) = input.prepared {
            ensure!(selected & !prepared.word(index) == 0, "unprepared rows");
        }
        let non_nulls = input.non_nulls.map(|mask| mask.word(index));
        while selected != 0 {
            let bit = selected.trailing_zeros() as usize;
            selected &= selected - 1;
            if non_nulls.is_none_or(|bits| bits & (1 << bit) != 0) {
                sum += input.values[index * 64 + bit];
            }
        }
    }
    Ok(sum)
}

/// The same over Datum storage and its flags.
#[inline(never)]
pub fn datum_reference<C>(input: &Input<'_, C>) -> Result<i64> {
    let rows = input.reference_rows;
    ensure!(input.datums.len() == rows.nrows, "row counts differ");
    let mut sum = 0;
    for index in 0..rows.nrows.div_ceil(64) {
        let mut selected = rows.word(index);
        if selected == 0 {
            continue;
        }
        if let Some(prepared) = input.prepared {
            ensure!(selected & !prepared.word(index) == 0, "unprepared rows");
        }
        while selected != 0 {
            let row = index * 64 + selected.trailing_zeros() as usize;
            selected &= selected - 1;
            if !input.nulls[row] {
                sum += input.datums[row] as i64;
            }
        }
    }
    Ok(sum)
}
