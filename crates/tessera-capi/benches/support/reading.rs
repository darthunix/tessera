//! Measured summation kernels using each public column-reading path.
//!
//! Exercise the early-exit fold and copied-word iteration separately: their
//! implementations and generated loops differ. The reference wrappers receive
//! the same prebuilt input and use the same summation rule; allocation and
//! construction stay outside the counted loop.

use anyhow::{Result, ensure};
use tessera_core::{ColumnReader, RowMaskView};

use crate::support::{
    fixture::Fixture,
    reference::{self, Mask},
    runner::{Group, Runner},
};

// Every measured entry point takes the same borrowed input. It is built before
// counting and black-boxed once per invocation inside the counted loop.
pub struct Input<'a, C> {
    pub column: &'a C,
    pub rows: RowMaskView<'a>,
    pub values: &'a [i32],
    pub datums: &'a [u64],
    pub nulls: &'a [bool],
    pub reference_rows: Mask<'a>,
    pub prepared: Option<Mask<'a>>,
    pub non_nulls: Option<Mask<'a>>,
}

impl<'a, C> Input<'a, C> {
    pub fn new(column: &'a C, case: &'a Fixture) -> Self {
        Self {
            column,
            rows: case.selected.view(),
            values: &case.values,
            datums: &case.datums,
            nulls: &case.nulls,
            reference_rows: case.selected.reference(),
            prepared: case
                .prepared
                .as_ref()
                .map(crate::support::fixture::Bitmap::reference),
            non_nulls: case
                .non_nulls
                .as_ref()
                .map(crate::support::fixture::Bitmap::reference),
        }
    }
}

#[inline(never)]
pub fn fold_sum<C: ColumnReader<Value = i32>>(input: &Input<'_, C>) -> Result<i64> {
    // map_or keeps the accumulator free of a select on NULL rows.
    input
        .column
        .try_fold_selected(&input.rows, 0, |sum, _, value| {
            Ok(sum + value.map_or(0, i64::from))
        })
}

#[inline(never)]
pub fn word_sum<C: ColumnReader<Value = i32>>(input: &Input<'_, C>) -> Result<i64> {
    ensure!(
        input.column.nrows() == input.rows.nrows(),
        "row counts differ"
    );
    let mut sum = 0;
    for index in 0..input.rows.nrows().div_ceil(64) {
        let selected = input.rows.get_word(index).unwrap();
        if selected == 0 {
            continue;
        }
        // fold is the bulk path of the word iterator, like the kernels use it.
        sum = input
            .column
            .word_values(index, selected)?
            .fold(sum, |sum, (_, value)| sum + value.map_or(0, i64::from));
    }
    Ok(sum)
}

#[inline(never)]
pub fn dense_reference<C>(input: &Input<'_, C>) -> Result<i64> {
    reference::dense(
        input.values,
        input.reference_rows,
        input.prepared,
        input.non_nulls,
    )
}

#[inline(never)]
pub fn datum_reference<C>(input: &Input<'_, C>) -> Result<i64> {
    reference::datum(
        input.datums,
        input.nulls,
        input.reference_rows,
        input.prepared,
    )
}

fn case(nrows: usize, pattern: &str, nulls: &str, partial: bool) -> Fixture {
    let values = (0..nrows)
        .map(|row| (row as i32).wrapping_mul(7919).wrapping_sub(104729))
        .collect();
    Fixture::from_values(values, pattern, nulls, partial)
}

pub fn cases() -> Vec<Fixture> {
    let mut cases = Vec::new();
    for nulls in ["none", "mixed"] {
        // `eighth` selects 8 rows a word, near the selected rows from which
        // the kernels take a word whole.
        for pattern in ["all", "half", "eighth", "sparse", "empty"] {
            cases.push(case(1024, pattern, nulls, false));
        }
    }
    for nrows in [0, 1, 63, 64, 65] {
        cases.push(case(nrows, "all", "mixed", false));
    }
    cases.push(case(1024, "all", "all", false));
    cases.push(case(1024, "all", "random", false));
    cases.push(case(1024, "random", "random", false));
    cases.push(case(1024, "all", "mixed", true));
    cases
}

pub fn bench(runner: &mut Runner) -> Result<()> {
    for case in cases() {
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
    pilot(runner)
}

/// The sizes of the rings of inputs of the pilot of unlearned inputs.
pub const PILOT_RINGS: [usize; 6] = [1, 8, 16, 32, 64, 128];

/// The case of the pilot: a random selection over random NULL flags, whose
/// loop over the selected rows of a word ends after a count of rows that a
/// predictor which has not seen the word cannot know.
pub fn pilot_case() -> Fixture {
    case(1024, "random", "random", false)
}

/// The pilot of unlearned inputs, on dense blocks: a ring of inputs that
/// the predictor cannot learn beside its control, a ring of as many
/// copies of one input. A form and its control are cases of this one
/// program, so their code lies at the same addresses.
fn pilot(runner: &mut Runner) -> Result<()> {
    let name = pilot_case().name;
    for count in PILOT_RINGS {
        let repeated = || (0..count).map(|_| pilot_case()).collect::<Vec<_>>();
        for (form, inputs) in [
            ("ring", pilot_case().ring(count)),
            ("ring-same", repeated()),
        ] {
            let columns = inputs
                .iter()
                .map(Fixture::dense_column)
                .collect::<Result<Vec<_>>>()?;
            let reads: Vec<_> = columns
                .iter()
                .zip(&inputs)
                .map(|(column, input)| Input::new(column, input))
                .collect();
            for (read, input) in reads.iter().zip(&inputs) {
                check_pilot(read, input)?;
            }
            let mut group =
                runner.group(format!("column_reader/dense/pilot-{form}-{count}/{name}"));
            ring_ops(&mut group, &reads)?;
        }
    }
    Ok(())
}

fn check_pilot<C: ColumnReader<Value = i32>>(read: &Input<'_, C>, input: &Fixture) -> Result<()> {
    ensure!(
        fold_sum(read)? == input.expected && dense_reference(read)? == input.expected,
        "a pilot's input differs from its model"
    );
    Ok(())
}

/// The fold and the reference over `reads` taken in turn, in whole turns.
fn ring_ops<C: ColumnReader<Value = i32>>(
    group: &mut Group<'_>,
    reads: &[Input<'_, C>],
) -> Result<()> {
    use std::hint::black_box;
    let count = reads.len();
    let mut at = 0;
    group.op_ring("fold", count, || {
        at = (at + 1) % count;
        fold_sum(black_box(&reads[at])).unwrap()
    })?;
    let mut at = 0;
    group.op_ring("reference", count, || {
        at = (at + 1) % count;
        dense_reference(black_box(&reads[at])).unwrap()
    })
}

fn measure_column<C: ColumnReader<Value = i32>>(
    runner: &mut Runner,
    format: &str,
    column: &C,
    direct: impl Fn(&Input<'_, C>) -> Result<i64>,
    case: &Fixture,
) -> Result<()> {
    use std::hint::black_box;
    let input = Input::new(column, case);
    ensure!(
        direct(&input)? == case.expected,
        "reference result differs from scalar model"
    );
    ensure!(
        fold_sum(&input)? == case.expected,
        "fold reader result differs"
    );
    ensure!(
        word_sum(&input)? == case.expected,
        "word reader result differs"
    );
    let mut group = runner.group(format!("column_reader/{format}/{}", case.name));
    // Separate closures preserve static dispatch inside every measured loop.
    group.op("fold", || fold_sum(black_box(&input)).unwrap())?;
    group.op("words", || word_sum(black_box(&input)).unwrap())?;
    group.op("reference", || direct(black_box(&input)).unwrap())?;
    Ok(())
}
