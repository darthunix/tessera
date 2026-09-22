//! The measured widening of int4 values into int8 Datums over the reading
//! fixtures: the kernel and an independent scalar loop.

use std::mem::MaybeUninit;

use anyhow::{Result, ensure};
use tessera_core::{ColumnReader, RowMask};
use tessera_kernels::cast::int4_to_int8;

use crate::reading::{self, Input};
use crate::support::{fixture::Fixture, runner::Runner};

/// The result buffers of one case.
pub struct Output {
    pub values: Vec<MaybeUninit<i64>>,
    pub words: Vec<u64>,
}

impl Output {
    pub fn new(nrows: usize) -> Self {
        Self {
            values: vec![MaybeUninit::uninit(); nrows],
            words: vec![0; nrows.div_ceil(64)],
        }
    }

    /// The value of a row the mask marks non-NULL.
    pub fn value(&self, row: usize) -> i64 {
        assert!(
            self.words[row / 64] & (1 << (row % 64)) != 0,
            "row {row} is NULL"
        );
        // SAFETY: the kernel writes every row it marks non-NULL.
        unsafe { self.values[row].assume_init() }
    }
}

#[inline(never)]
pub fn widen<C: ColumnReader<Value = i32>>(input: &Input<'_, C>, out: &mut Output) -> Result<()> {
    let mask = RowMask::try_new(out.values.len(), &mut out.words)?;
    let mut mask = mask;
    int4_to_int8(input.column, &input.rows, &mut out.values, &mut mask)
}

/// Every selected non-NULL value widened, straight from the dense fixture
/// buffers and their masks.
#[inline(never)]
pub fn dense_reference<C>(input: &Input<'_, C>, out: &mut Output) -> Result<()> {
    let rows = input.reference_rows;
    ensure!(input.values.len() == rows.nrows, "row counts differ");
    for index in 0..rows.nrows.div_ceil(64) {
        let mut selected = rows.word(index);
        let mut present = 0;
        if selected != 0 {
            if let Some(prepared) = input.prepared {
                ensure!(selected & !prepared.word(index) == 0, "unprepared rows");
            }
            let non_nulls = input.non_nulls.map_or(u64::MAX, |mask| mask.word(index));
            while selected != 0 {
                let bit = selected.trailing_zeros() as usize;
                selected &= selected - 1;
                let row = index * 64 + bit;
                if non_nulls & (1 << bit) != 0 {
                    out.values[row].write(i64::from(input.values[row]));
                    present |= 1 << bit;
                }
            }
        }
        out.words[index] = present;
    }
    Ok(())
}

/// The same over Datum storage and its flags.
#[inline(never)]
pub fn datum_reference<C>(input: &Input<'_, C>, out: &mut Output) -> Result<()> {
    let rows = input.reference_rows;
    ensure!(input.datums.len() == rows.nrows, "row counts differ");
    for index in 0..rows.nrows.div_ceil(64) {
        let mut selected = rows.word(index);
        let mut present = 0;
        if selected != 0 {
            if let Some(prepared) = input.prepared {
                ensure!(selected & !prepared.word(index) == 0, "unprepared rows");
            }
            while selected != 0 {
                let bit = selected.trailing_zeros() as usize;
                selected &= selected - 1;
                let row = index * 64 + bit;
                if !input.nulls[row] {
                    out.values[row].write(i64::from(input.datums[row] as i32));
                    present |= 1 << bit;
                }
            }
        }
        out.words[index] = present;
    }
    Ok(())
}

/// Check the kernel and the reference against the fixture before anything
/// is measured: the same rows marked, every value the widened one.
pub fn check<C: ColumnReader<Value = i32>>(
    input: &Input<'_, C>,
    case: &Fixture,
    direct: impl Fn(&Input<'_, C>, &mut Output) -> Result<()>,
) -> Result<()> {
    let words = case.selected.words();
    for (what, run) in [
        (
            "reference",
            &direct as &dyn Fn(&Input<'_, C>, &mut Output) -> Result<()>,
        ),
        ("widen", &widen),
    ] {
        let mut out = Output::new(case.values.len());
        run(input, &mut out)?;
        for row in 0..case.values.len() {
            let selected = words[row / 64] & (1 << (row % 64)) != 0;
            let present = out.words[row / 64] & (1 << (row % 64)) != 0;
            ensure!(
                present == (selected && !case.nulls[row]),
                "{what}: row {row} marked {present}"
            );
            if present {
                ensure!(
                    out.value(row) == i64::from(case.values[row]),
                    "{what}: row {row} differs from the fixture"
                );
            }
        }
    }
    Ok(())
}

pub fn bench(runner: &mut Runner) -> Result<()> {
    for case in reading::cases() {
        measure_column(
            runner,
            "dense",
            &case.dense_column()?,
            dense_reference,
            &case,
        )?;
        measure_column(
            runner,
            "datum",
            &case.datum_column()?,
            datum_reference,
            &case,
        )?;
    }
    Ok(())
}

fn measure_column<C: ColumnReader<Value = i32>>(
    runner: &mut Runner,
    format: &str,
    column: &C,
    direct: impl Fn(&Input<'_, C>, &mut Output) -> Result<()> + Copy,
    case: &Fixture,
) -> Result<()> {
    use std::hint::black_box;
    let input = Input::new(column, case);
    check(&input, case, direct)?;
    let mut out = Output::new(case.values.len());
    let mut group = runner.group(format!("cast_int32/{format}/{}", case.name));
    group.op("widen", || widen(black_box(&input), &mut out).unwrap())?;
    group.op("reference", || direct(black_box(&input), &mut out).unwrap())?;
    Ok(())
}
