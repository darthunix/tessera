//! Measured hash kernels over the reading fixtures.
//!
//! The same inputs as the reading benchmark; the kernels write a hash per
//! selected row and a valid mask into buffers allocated once per case, and
//! the reference is an independent scalar loop over the fixture's buffers
//! with its own finalizer. Results are checked against a model before
//! counting.

use anyhow::{Result, ensure};
use tessera_core::{ColumnReader, RowMask};
use tessera_kernels::int32::{self, NullKeys};

use crate::reading::{self, Input};
use crate::support::{fixture::Fixture, runner::Runner};

/// The result buffers of one case: a hash per row and the valid mask.
pub struct Output {
    pub hashes: Vec<u32>,
    pub words: Vec<u64>,
}

impl Output {
    pub fn new(nrows: usize) -> Self {
        Self {
            hashes: vec![0; nrows],
            words: vec![0; nrows.div_ceil(64)],
        }
    }

    /// The hash buffer and the mask over the word buffer, borrowed apart.
    fn parts(&mut self) -> Result<(&mut [u32], RowMask<'_>)> {
        let mask = RowMask::try_new(self.hashes.len(), &mut self.words)?;
        Ok((&mut self.hashes, mask))
    }
}

/// One key, NULL rows rejected.
#[inline(never)]
pub fn hash<C: ColumnReader<Value = i32>>(input: &Input<'_, C>, out: &mut Output) -> Result<()> {
    let (hashes, mut valid) = out.parts()?;
    int32::hash(
        input.column,
        &input.rows,
        NullKeys::Reject,
        hashes,
        &mut valid,
    )
}

/// One key, NULL as a group key.
#[inline(never)]
pub fn hash_group<C: ColumnReader<Value = i32>>(
    input: &Input<'_, C>,
    out: &mut Output,
) -> Result<()> {
    let (hashes, mut valid) = out.parts()?;
    int32::hash(
        input.column,
        &input.rows,
        NullKeys::Group,
        hashes,
        &mut valid,
    )
}

/// Two keys from the same column, NULL rows rejected.
#[inline(never)]
pub fn two_keys<C: ColumnReader<Value = i32>>(
    input: &Input<'_, C>,
    out: &mut Output,
) -> Result<()> {
    let (hashes, mut valid) = out.parts()?;
    int32::hash(
        input.column,
        &input.rows,
        NullKeys::Reject,
        hashes,
        &mut valid,
    )?;
    int32::hash_next(input.column, NullKeys::Reject, hashes, &mut valid)
}

/// The reference's own port of PostgreSQL's `murmurhash32`.
#[inline]
fn murmur(mut h: u32) -> u32 {
    h ^= h >> 16;
    h = h.wrapping_mul(0x85eb_ca6b);
    h ^= h >> 13;
    h = h.wrapping_mul(0xc2b2_ae35);
    h ^= h >> 16;
    h
}

/// The reference's own port of PostgreSQL's `hash_combine`.
fn combine(a: u32, b: u32) -> u32 {
    a ^ (b
        .wrapping_add(0x9e37_79b9)
        .wrapping_add(a << 6)
        .wrapping_add(a >> 2))
}

/// The hash of a NULL key under the group policy.
const NULL_HASH: u32 = 0x92ca_2f0e;

/// One key of every selected non-NULL row, straight from the dense fixture
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
                    out.hashes[row] = murmur(input.values[row] as u32);
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
                    out.hashes[row] = murmur(input.datums[row] as u32);
                    present |= 1 << bit;
                }
            }
        }
        out.words[index] = present;
    }
    Ok(())
}

/// Check one operation against the fixture model: the same rows valid, the
/// same hashes; under the group policy NULL rows stay valid with the fixed
/// hash.
fn check_op(
    case: &Fixture,
    out: &Output,
    what: &str,
    group: bool,
    expected: impl Fn(u32) -> u32,
) -> Result<()> {
    let words = case.selected.words();
    for row in 0..case.values.len() {
        let selected = words[row / 64] & (1 << (row % 64)) != 0;
        let valid = out.words[row / 64] & (1 << (row % 64)) != 0;
        let model = selected && (group || !case.nulls[row]);
        ensure!(
            valid == model,
            "{what}: row {row} valid {valid}, model {model}"
        );
        if model {
            let hash = if case.nulls[row] {
                NULL_HASH
            } else {
                expected(case.values[row] as u32)
            };
            ensure!(
                out.hashes[row] == hash,
                "{what}: row {row} differs from the model"
            );
        }
    }
    Ok(())
}

/// Check every kernel and the reference against the model before anything
/// is measured.
pub fn check<C: ColumnReader<Value = i32>>(
    input: &Input<'_, C>,
    case: &Fixture,
    direct: impl Fn(&Input<'_, C>, &mut Output) -> Result<()>,
) -> Result<()> {
    let mut out = Output::new(case.values.len());
    direct(input, &mut out)?;
    check_op(case, &out, "reference", false, murmur)?;
    hash(input, &mut out)?;
    check_op(case, &out, "hash", false, murmur)?;
    hash_group(input, &mut out)?;
    check_op(case, &out, "hash_group", true, murmur)?;
    two_keys(input, &mut out)?;
    check_op(case, &out, "two_keys", false, |key| {
        combine(murmur(key), murmur(key))
    })?;
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
    let mut group = runner.group(format!("hash_int32/{format}/{}", case.name));
    group.op("hash", || hash(black_box(&input), &mut out).unwrap())?;
    group.op("hash_group", || {
        hash_group(black_box(&input), &mut out).unwrap()
    })?;
    group.op("two_keys", || {
        two_keys(black_box(&input), &mut out).unwrap()
    })?;
    group.op("reference", || direct(black_box(&input), &mut out).unwrap())?;
    Ok(())
}
