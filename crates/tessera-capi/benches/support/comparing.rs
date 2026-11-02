//! Two-column comparison fixtures, the measured kernel and an independent
//! scalar reference.
//!
//! The left column is the filter family's fixture; the right one has other
//! values over the same rows, the same selection and readiness, and NULLs
//! of its own. Every call narrows a fresh copy of the selection, restored
//! outside the counted region, as for the filters. The case whose selection
//! is drawn at random takes a ring of pairs, both sides permuted alike.

use std::hint::black_box;
use std::mem::MaybeUninit;

use anyhow::{Result, ensure};
use pmu::Reading;
use tessera_core::{ColumnReader, RowMask};
use tessera_kernels::int32::{CompareOp, compare_columns};

use crate::support::{
    fixture::{Fixture, RING, as_uninit},
    runner::Runner,
};

/// Masks of one call's selection, at most this many live at once.
const BLOCK_SIZE: usize = 4096;

fn case(nrows: usize, pattern: &str, nulls: &str, partial: bool) -> [Fixture; 2] {
    let left = (0..nrows)
        .map(|row| ((row * 37) % 101) as i32 - 50)
        .collect();
    let right = (0..nrows)
        .map(|row| ((row * 53) % 97) as i32 - 48)
        .collect();
    [
        Fixture::from_values(left, pattern, nulls, partial),
        Fixture::from_values(right, pattern, "random", partial),
    ]
}

pub fn cases() -> Vec<[Fixture; 2]> {
    let mut cases = Vec::new();
    for nrows in [65, 1024] {
        for pattern in ["all", "eighth", "one-per128", "empty"] {
            for nulls in ["none", "mixed"] {
                cases.push(case(nrows, pattern, nulls, false));
            }
        }
    }
    for nrows in [0, 1, 63, 64] {
        cases.push(case(nrows, "all", "mixed", false));
    }
    cases.push(case(1024, "random", "random", false));
    cases.push(case(1024, "all", "mixed", true));
    cases
}

/// The pairs that the calls of a case take in turn: a ring of [`RING`]
/// if its left side, the selection and its NULL flags, is drawn at random,
/// the pair alone otherwise. The right side's NULL flags are drawn in every
/// case, but where the selection is regular the case keeps one pair. Both
/// sides of a pair are permuted by the same seed, so their rows stay paired.
pub fn inputs(pair: [Fixture; 2]) -> Vec<[Fixture; 2]> {
    let [left, right] = pair;
    if !left.drawn {
        return vec![[left, right]];
    }
    left.ring(RING)
        .into_iter()
        .zip(right.ring(RING))
        .map(|(left, right)| [left, right])
        .collect()
}

/// What every measured call borrows.
pub struct Input<'a, L, R> {
    left: &'a L,
    right: &'a R,
    left_dense: &'a [MaybeUninit<i32>],
    right_dense: &'a [MaybeUninit<i32>],
    left_datums: &'a [MaybeUninit<u64>],
    right_datums: &'a [MaybeUninit<u64>],
    left_nulls: &'a [bool],
    right_nulls: &'a [bool],
    pub op: CompareOp,
}

#[inline(never)]
pub fn kernel<L, R>(input: &Input<'_, L, R>, rows: &mut RowMask<'_>) -> Result<()>
where
    L: ColumnReader<Value = i32>,
    R: ColumnReader<Value = i32>,
{
    compare_columns(input.left, input.right, rows, input.op)
}

/// The selected rows kept where both values are present and compare, one
/// by one from the fixture's buffers.
#[inline(always)]
fn reference(words: &mut [u64], op: CompareOp, read: impl Fn(usize) -> Option<(i32, i32)>) {
    for (index, word) in words.iter_mut().enumerate() {
        let mut selected = *word;
        let mut passing = 0;
        while selected != 0 {
            let bit = selected.trailing_zeros() as usize;
            selected &= selected - 1;
            if let Some((a, b)) = read(index * 64 + bit)
                && match op {
                    CompareOp::Eq => a == b,
                    CompareOp::Ne => a != b,
                    CompareOp::Lt => a < b,
                    CompareOp::Le => a <= b,
                    CompareOp::Gt => a > b,
                    CompareOp::Ge => a >= b,
                }
            {
                passing |= 1 << bit;
            }
        }
        *word = passing;
    }
}

#[inline(never)]
pub fn dense_reference<L, R>(input: &Input<'_, L, R>, words: &mut [u64]) {
    reference(words, input.op, |row| {
        if input.left_nulls[row] || input.right_nulls[row] {
            return None;
        }
        // SAFETY: the fixtures initialize these buffers, and the selected
        // rows are prepared.
        Some(unsafe {
            (
                input.left_dense[row].assume_init(),
                input.right_dense[row].assume_init(),
            )
        })
    })
}

#[inline(never)]
pub fn datum_reference<L, R>(input: &Input<'_, L, R>, words: &mut [u64]) {
    reference(words, input.op, |row| {
        if input.left_nulls[row] || input.right_nulls[row] {
            return None;
        }
        // SAFETY: as for the dense buffers.
        Some(unsafe {
            (
                input.left_datums[row].assume_init() as i32,
                input.right_datums[row].assume_init() as i32,
            )
        })
    })
}

/// The expected selection of a case, from the values and flags.
pub fn expected(pair: &[Fixture; 2], op: CompareOp) -> Vec<u64> {
    let [left, right] = pair;
    let mut words = left.selected.words().to_vec();
    let nulls = |row: usize| left.nulls[row] || right.nulls[row];
    reference(&mut words, op, |row| {
        (!nulls(row)).then(|| (left.values[row], right.values[row]))
    });
    words
}

/// Fresh copies of one selection for a block of calls, at most
/// [`BLOCK_SIZE`] live at once.
struct Masks {
    originals: Vec<Vec<u64>>,
    storage: Vec<u64>,
}

impl Masks {
    /// The masks of calls over the pairs of a ring, whose selections are
    /// `originals`, taken in turn.
    fn new(originals: &[&[u64]]) -> Self {
        Self {
            originals: originals.iter().map(|original| original.to_vec()).collect(),
            storage: vec![0; originals[0].len().max(1) * BLOCK_SIZE],
        }
    }

    /// `count` fresh copies, of each pair's selection in turn; restoring
    /// them is not counted.
    fn reset(&mut self, count: usize) -> impl Iterator<Item = &mut [u64]> {
        let words = self.originals[0].len();
        let stride = words.max(1);
        for (chunk, original) in self.storage[..count * stride]
            .chunks_exact_mut(stride)
            .zip(self.originals.iter().cycle())
        {
            chunk[..words].copy_from_slice(original);
        }
        self.storage[..count * stride]
            .chunks_exact_mut(stride)
            .map(move |chunk| &mut chunk[..words])
    }

    /// Counters of `iterations` kernel calls on fresh masks, over `inputs`
    /// in turn, whose views are built before the counters are read.
    fn run_kernel<L, R>(
        &mut self,
        read: &mut dyn FnMut() -> Result<Reading, pmu::Error>,
        nrows: usize,
        inputs: &[Input<'_, L, R>],
        iterations: u64,
    ) -> Result<Reading>
    where
        L: ColumnReader<Value = i32>,
        R: ColumnReader<Value = i32>,
    {
        let mut total = Reading::default();
        let mut remaining = iterations;
        while remaining != 0 {
            let count = remaining.min(BLOCK_SIZE as u64) as usize;
            let mut views: Vec<_> = self
                .reset(count)
                .map(|words| RowMask::try_new(nrows, words).unwrap())
                .collect();
            let start = read()?;
            for (rows, input) in views.iter_mut().zip(inputs.iter().cycle()) {
                black_box(kernel(black_box(input), black_box(rows))).unwrap();
            }
            total += read()?.since(start);
            drop(views);
            black_box(&self.storage);
            remaining -= count as u64;
        }
        Ok(total)
    }

    /// Counters of `iterations` reference calls on fresh masks, over
    /// `inputs` in turn.
    fn run_reference<L, R>(
        &mut self,
        read: &mut dyn FnMut() -> Result<Reading, pmu::Error>,
        inputs: &[Input<'_, L, R>],
        direct: impl Fn(&Input<'_, L, R>, &mut [u64]),
        iterations: u64,
    ) -> Result<Reading> {
        let mut total = Reading::default();
        let mut remaining = iterations;
        while remaining != 0 {
            let count = remaining.min(BLOCK_SIZE as u64) as usize;
            let mut views: Vec<_> = self.reset(count).collect();
            let start = read()?;
            for (words, input) in views.iter_mut().zip(inputs.iter().cycle()) {
                direct(black_box(input), black_box(words));
            }
            total += read()?.since(start);
            drop(views);
            black_box(&self.storage);
            remaining -= count as u64;
        }
        Ok(total)
    }
}

pub fn bench(runner: &mut Runner) -> Result<()> {
    for pair in cases() {
        let pairs = inputs(pair);
        let sides = |side: usize| pairs.iter().map(move |pair| &pair[side]);
        measure(
            runner,
            "dense",
            &sides(0)
                .map(Fixture::dense_column)
                .collect::<Result<Vec<_>>>()?,
            &sides(1)
                .map(Fixture::dense_column)
                .collect::<Result<Vec<_>>>()?,
            dense_reference,
            &pairs,
        )?;
        measure(
            runner,
            "datum",
            &sides(0)
                .map(Fixture::datum_column)
                .collect::<Result<Vec<_>>>()?,
            &sides(1)
                .map(Fixture::datum_column)
                .collect::<Result<Vec<_>>>()?,
            datum_reference,
            &pairs,
        )?;
    }
    Ok(())
}

pub fn input<'a, L, R>(
    left: &'a L,
    right: &'a R,
    pair: &'a [Fixture; 2],
    op: CompareOp,
) -> Input<'a, L, R> {
    let [lf, rf] = pair;
    Input {
        left,
        right,
        left_dense: as_uninit(&lf.values),
        right_dense: as_uninit(&rf.values),
        left_datums: as_uninit(&lf.datums),
        right_datums: as_uninit(&rf.datums),
        left_nulls: &lf.nulls,
        right_nulls: &rf.nulls,
        op,
    }
}

/// Check the kernel and the reference against the expected selection.
pub fn check<L, R>(
    input: &Input<'_, L, R>,
    pair: &[Fixture; 2],
    direct: impl Fn(&Input<'_, L, R>, &mut [u64]),
) -> Result<()>
where
    L: ColumnReader<Value = i32>,
    R: ColumnReader<Value = i32>,
{
    let expected = expected(pair, input.op);
    let original = pair[0].selected.words();
    let mut reference = original.to_vec();
    direct(input, &mut reference);
    ensure!(reference == expected, "reference differs from the model");
    let mut actual = original.to_vec();
    kernel(
        input,
        &mut RowMask::try_new(pair[0].values.len(), &mut actual)?,
    )?;
    ensure!(actual == expected, "kernel differs from the model");
    Ok(())
}

fn measure<L, R>(
    runner: &mut Runner,
    format: &str,
    lefts: &[L],
    rights: &[R],
    direct: impl Fn(&Input<'_, L, R>, &mut [u64]) + Copy,
    pairs: &[[Fixture; 2]],
) -> Result<()>
where
    L: ColumnReader<Value = i32>,
    R: ColumnReader<Value = i32>,
{
    let inputs: Vec<_> = lefts
        .iter()
        .zip(rights)
        .zip(pairs)
        .map(|((left, right), pair)| input(left, right, pair, CompareOp::Lt))
        .collect();
    for (input, pair) in inputs.iter().zip(pairs) {
        check(input, pair, direct)?;
    }
    let nrows = pairs[0][0].values.len();
    let originals: Vec<_> = pairs.iter().map(|pair| pair[0].selected.words()).collect();
    let mut masks = Masks::new(&originals);
    let mut group = runner.group(format!("compare_int32/{format}/{}", pairs[0][0].name));
    group.op_blocks_ring("kernel", inputs.len(), |counters, iterations| {
        masks.run_kernel(&mut || counters.read(), nrows, &inputs, iterations)
    })?;
    group.op_blocks_ring("reference", inputs.len(), |counters, iterations| {
        masks.run_reference(&mut || counters.read(), &inputs, direct, iterations)
    })?;
    Ok(())
}
