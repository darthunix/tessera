//! Filter fixtures, timed entry points and an independent scalar reference.
//!
//! All timed paths borrow the same value/flag buffers. NULL checks in the
//! reference read individual bits, not Tessera's word decoder. Constructors,
//! mask restoration and model checking belong outside the timed regions.

use std::mem::MaybeUninit;

use anyhow::{Result, ensure};
use tessera_core::{ColumnReader, RowMask};
use tessera_kernels::int32::{CompareOp, filter};

use crate::support::{
    fixture::{Fixture, as_uninit},
    reference::Mask,
};

#[path = "filter_timing.rs"]
pub mod timing;

fn case(nrows: usize, pattern: &str, nulls: &str, offset: Option<usize>, partial: bool) -> Fixture {
    let values = (0..nrows)
        .map(|row| ((row * 37) % 101) as i32 - 50)
        .collect();
    Fixture::from_values(values, pattern, nulls, offset, partial)
}

pub fn cases() -> Vec<Fixture> {
    let mut cases = Vec::new();
    for nrows in [65, 1024] {
        for pattern in ["all", "eighth", "one-per128", "empty"] {
            for nulls in ["none", "mixed"] {
                cases.push(case(nrows, pattern, nulls, Some(0), false));
            }
        }
    }
    for nrows in [0, 1, 63, 64] {
        cases.push(case(nrows, "all", "mixed", Some(0), false));
    }
    cases.push(case(1024, "all", "all", None, false));
    cases.push(case(1024, "all", "mixed", None, true));
    cases.push(case(1024, "all", "mixed", Some(7), true));
    cases.push(case(1024, "one-per128", "mixed", Some(7), true));
    cases
}

/// No timed path owns data or retains a borrow beyond its call.
pub struct Input<'a, C> {
    column: &'a C,
    dense: &'a [MaybeUninit<i32>],
    datums: &'a [MaybeUninit<u64>],
    isnull: &'a [MaybeUninit<bool>],
    prepared: Option<Mask<'a>>,
    non_nulls: Option<Mask<'a>>,
    pub op: CompareOp,
    pub scalar: i32,
}

impl<'a, C> Input<'a, C> {
    pub fn new(column: &'a C, fixture: &'a Fixture, op: CompareOp, scalar: i32) -> Self {
        Self {
            column,
            dense: as_uninit(&fixture.values),
            datums: as_uninit(&fixture.datums),
            isnull: as_uninit(&fixture.nulls),
            prepared: fixture.prepared.as_ref().map(|mask| mask.reference()),
            non_nulls: fixture.non_nulls.as_ref().map(|mask| mask.reference()),
            op,
            scalar,
        }
    }
}

#[inline(never)]
pub fn scalar<C: ColumnReader<Value = i32>>(
    input: &Input<'_, C>,
    rows: &mut RowMask<'_>,
) -> Result<()> {
    filter(input.column, rows, input.op, input.scalar)
}

// Pick the comparison once, as in the library, while keeping all traversal and
// bitmap access independent. Earlier words remain changed after readiness errors.
#[inline(always)]
fn reference(
    nrows: usize,
    words: &mut [u64],
    prepared: Option<Mask<'_>>,
    op: CompareOp,
    scalar: i32,
    read: impl Fn(usize) -> Option<i32>,
) -> Result<()> {
    ensure!(words.len() == nrows.div_ceil(64), "word count differs");
    match op {
        CompareOp::Eq => reference_with(nrows, words, prepared, scalar, read, |a, b| a == b),
        CompareOp::Ne => reference_with(nrows, words, prepared, scalar, read, |a, b| a != b),
        CompareOp::Lt => reference_with(nrows, words, prepared, scalar, read, |a, b| a < b),
        CompareOp::Le => reference_with(nrows, words, prepared, scalar, read, |a, b| a <= b),
        CompareOp::Gt => reference_with(nrows, words, prepared, scalar, read, |a, b| a > b),
        CompareOp::Ge => reference_with(nrows, words, prepared, scalar, read, |a, b| a >= b),
    }
}

#[inline(always)]
fn reference_with(
    nrows: usize,
    words: &mut [u64],
    prepared: Option<Mask<'_>>,
    scalar: i32,
    read: impl Fn(usize) -> Option<i32>,
    compare: impl Fn(i32, i32) -> bool,
) -> Result<()> {
    for (index, word) in words.iter_mut().enumerate() {
        let mut selected = *word;
        if selected == 0 {
            continue;
        }
        let base = index * 64;
        ensure!(
            64 - selected.leading_zeros() as usize <= nrows - base,
            "selected padding"
        );
        if let Some(ready) = prepared {
            ensure!(selected & !ready.word(index) == 0, "unprepared rows");
        }
        let mut passing = 0;
        while selected != 0 {
            let bit = selected.trailing_zeros() as usize;
            selected &= selected - 1;
            if let Some(value) = read(base + bit)
                && compare(value, scalar)
            {
                passing |= 1 << bit;
            }
        }
        *word = passing;
    }
    Ok(())
}

#[inline(never)]
pub fn dense_reference<C>(input: &Input<'_, C>, words: &mut [u64]) -> Result<()> {
    reference(
        input.dense.len(),
        words,
        input.prepared,
        input.op,
        input.scalar,
        |row| {
            if input.non_nulls.is_some_and(|mask| !mask.contains(row)) {
                return None;
            }
            // SAFETY: Fixture initializes these exact buffers. Traversal also
            // validates readiness and checks nullness before reading the value.
            Some(unsafe { input.dense[row].assume_init() })
        },
    )
}

#[inline(never)]
pub fn datum_reference<C>(input: &Input<'_, C>, words: &mut [u64]) -> Result<()> {
    reference(
        input.datums.len(),
        words,
        input.prepared,
        input.op,
        input.scalar,
        |row| {
            // SAFETY: Fixture initializes these flags; the row is in bounds and ready.
            if unsafe { input.isnull[row].assume_init() } {
                return None;
            }
            // SAFETY: the same initialized fixture supplies a prepared non-NULL value.
            Some(unsafe { input.datums[row].assume_init() } as i32)
        },
    )
}

pub fn expected(fixture: &Fixture, selected: &[u64], op: CompareOp, scalar: i32) -> Vec<u64> {
    let mut words = vec![0; fixture.values.len().div_ceil(64)];
    for (row, &value) in fixture.values.iter().enumerate() {
        let passes = match op {
            CompareOp::Eq => value == scalar,
            CompareOp::Ne => value != scalar,
            CompareOp::Lt => value < scalar,
            CompareOp::Le => value <= scalar,
            CompareOp::Gt => value > scalar,
            CompareOp::Ge => value >= scalar,
        };
        if selected[row / 64] & (1 << (row % 64)) != 0 && !fixture.nulls[row] && passes {
            words[row / 64] |= 1 << (row % 64);
        }
    }
    words
}

pub fn bench(criterion: &mut criterion::Criterion) {
    for case in cases() {
        measure_column(
            criterion,
            "dense",
            &case.dense_column().unwrap(),
            dense_reference,
            &case,
        )
        .unwrap();
        measure_column(
            criterion,
            "datum",
            &case.datum_column().unwrap(),
            datum_reference,
            &case,
        )
        .unwrap();
    }
}

fn measure_column<C: ColumnReader<Value = i32>>(
    criterion: &mut criterion::Criterion,
    format: &str,
    column: &C,
    direct: impl Fn(&Input<'_, C>, &mut [u64]) -> Result<()> + Copy,
    case: &Fixture,
) -> Result<()> {
    let input = Input::new(column, case, CompareOp::Gt, 0);
    let original = case.selected.words();
    let expected = expected(case, original, input.op, input.scalar);
    let mut reference = original.to_vec();
    direct(&input, &mut reference)?;
    ensure!(reference == expected, "reference differs from scalar model");
    let mut actual = original.to_vec();
    scalar(
        &input,
        &mut RowMask::try_new(case.values.len(), &mut actual)?,
    )?;
    ensure!(actual == expected, "filter differs from scalar model");
    let mut masks = timing::Masks::new(case.values.len(), original)?;
    let mut group = criterion.benchmark_group(format!("filter_int32/{format}/{}", case.name));
    group.bench_function("scalar", |b| {
        b.iter_custom(|iterations| masks.time_scalar(&input, iterations))
    });
    group.bench_function("reference", |b| {
        b.iter_custom(|iterations| masks.time_reference(&input, direct, iterations))
    });
    group.finish();
    Ok(())
}
