//! Measured set membership over the reading fixtures of both widths.
//!
//! `x IN (…)` of int4 and of int8 values against sets of 4, 16, 17, 64
//! and 65 keys, on either side of the bounds of the sets: a row compares
//! up to 16 keys all and searches more by halving, and a whole word is
//! compared with vector code up to 64 int4 keys or 16 int8 ones. The keys
//! are the values of every seventh row of the cases of 1024 rows, so every
//! set holds some of a case's values. The kernel writes the rows found
//! and the rows present into masks allocated once per case; the reference
//! is an independent scalar loop over the fixture's buffers that searches
//! the 16 keys with the standard library's binary search. Results are
//! checked against a model before counting.

use anyhow::{Result, ensure};
use tessera_core::{ColumnReader, RowMask, RowMaskView};
use tessera_kernels::set::{self, KeySet, SetValue};

use crate::support::{
    fixture::{Bitmap, Fixture, FixtureValue},
    reference::Mask,
    runner::Runner,
};
use crate::{reading, reading64};

/// The sizes of the sets measured.
pub const KEY_COUNTS: [usize; 5] = [4, 16, 17, 64, 65];

/// The size of the set the reference searches.
const REFERENCE_KEYS: usize = 16;

/// Rows between two rows whose values are keys.
const KEY_STRIDE: usize = 7;

/// A width of the sets: its name in the ids, its cases, and the value of
/// a Datum as the reference reads it, by itself.
pub trait Width: SetValue + FixtureValue {
    const NAME: &'static str;
    fn cases() -> Vec<Fixture<Self>>;
    fn decode(datum: u64) -> Self;
}

impl Width for i32 {
    const NAME: &'static str = "int4";
    fn cases() -> Vec<Fixture<Self>> {
        reading::cases()
    }
    fn decode(datum: u64) -> Self {
        datum as i32
    }
}

impl Width for i64 {
    const NAME: &'static str = "int8";
    fn cases() -> Vec<Fixture<Self>> {
        reading64::cases()
    }
    fn decode(datum: u64) -> Self {
        datum as i64
    }
}

/// The largest set, of the values of every seventh row of the first case
/// of 1024 rows; a smaller set is its first keys. The values grow with the
/// row, so the keys are sorted without repeats.
pub fn keys<T: Width>(cases: &[Fixture<T>]) -> Result<Vec<T>> {
    let values = &cases
        .iter()
        .find(|case| case.values.len() == 1024)
        .ok_or_else(|| anyhow::anyhow!("no case of 1024 rows"))?
        .values;
    let count = KEY_COUNTS[KEY_COUNTS.len() - 1];
    let keys: Vec<_> = (0..count).map(|key| values[key * KEY_STRIDE]).collect();
    ensure!(
        keys.windows(2).all(|pair| pair[0] < pair[1]),
        "the keys are not sorted without repeats"
    );
    Ok(keys)
}

/// The borrowed input of every measured entry point, built before
/// counting, as the reading benchmark builds it.
pub struct Input<'a, C, T> {
    pub column: &'a C,
    pub rows: RowMaskView<'a>,
    pub values: &'a [T],
    pub datums: &'a [u64],
    pub nulls: &'a [bool],
    pub reference_rows: Mask<'a>,
    pub prepared: Option<Mask<'a>>,
    pub non_nulls: Option<Mask<'a>>,
}

impl<'a, C, T> Input<'a, C, T> {
    pub fn new(column: &'a C, case: &'a Fixture<T>) -> Self {
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

/// The result masks of one case: the rows found and the rows present.
pub struct Output {
    nrows: usize,
    pub found: Vec<u64>,
    pub present: Vec<u64>,
}

impl Output {
    pub fn new(nrows: usize) -> Self {
        Self {
            nrows,
            found: vec![0; nrows.div_ceil(64)],
            present: vec![0; nrows.div_ceil(64)],
        }
    }

    /// Both masks over their word buffers, borrowed apart.
    fn masks(&mut self) -> Result<(RowMask<'_>, RowMask<'_>)> {
        Ok((
            RowMask::try_new(self.nrows, &mut self.found)?,
            RowMask::try_new(self.nrows, &mut self.present)?,
        ))
    }
}

/// The membership of the selected rows' values in `keys`.
#[inline(never)]
pub fn in_set<T: SetValue, C: ColumnReader<Value = T>>(
    keys: &KeySet<'_, T>,
    input: &Input<'_, C, T>,
    out: &mut Output,
) -> Result<()> {
    let (mut found, mut present) = out.masks()?;
    set::in_set(keys, input.column, input.rows, &mut found, &mut present)
}

/// The membership of every selected non-NULL row's value in `keys`,
/// straight from the dense fixture buffers and their masks.
#[inline(never)]
pub fn dense_reference<C, T: Width>(
    keys: &[T],
    input: &Input<'_, C, T>,
    out: &mut Output,
) -> Result<()> {
    let rows = input.reference_rows;
    ensure!(input.values.len() == rows.nrows, "row counts differ");
    for index in 0..rows.nrows.div_ceil(64) {
        let mut selected = rows.word(index);
        let (mut found, mut present) = (0, 0);
        if selected != 0 {
            if let Some(prepared) = input.prepared {
                ensure!(selected & !prepared.word(index) == 0, "unprepared rows");
            }
            let non_nulls = input.non_nulls.map_or(u64::MAX, |mask| mask.word(index));
            while selected != 0 {
                let bit = selected.trailing_zeros() as usize;
                selected &= selected - 1;
                if non_nulls & (1 << bit) != 0 {
                    present |= 1 << bit;
                    let value = input.values[index * 64 + bit];
                    found |= u64::from(keys.binary_search(&value).is_ok()) << bit;
                }
            }
        }
        out.found[index] = found;
        out.present[index] = present;
    }
    Ok(())
}

/// The same over Datum storage and its flags.
#[inline(never)]
pub fn datum_reference<C, T: Width>(
    keys: &[T],
    input: &Input<'_, C, T>,
    out: &mut Output,
) -> Result<()> {
    let rows = input.reference_rows;
    ensure!(input.datums.len() == rows.nrows, "row counts differ");
    for index in 0..rows.nrows.div_ceil(64) {
        let mut selected = rows.word(index);
        let (mut found, mut present) = (0, 0);
        if selected != 0 {
            if let Some(prepared) = input.prepared {
                ensure!(selected & !prepared.word(index) == 0, "unprepared rows");
            }
            while selected != 0 {
                let bit = selected.trailing_zeros() as usize;
                selected &= selected - 1;
                let row = index * 64 + bit;
                if !input.nulls[row] {
                    present |= 1 << bit;
                    let value = T::decode(input.datums[row]);
                    found |= u64::from(keys.binary_search(&value).is_ok()) << bit;
                }
            }
        }
        out.found[index] = found;
        out.present[index] = present;
    }
    Ok(())
}

/// Check one result against the fixture model: present are the selected
/// non-NULL rows, found those of them whose value is a key.
fn check_op<T: Width>(case: &Fixture<T>, keys: &[T], out: &Output, what: &str) -> Result<()> {
    let words = case.selected.words();
    for row in 0..case.values.len() {
        let bit = |mask: &[u64]| mask[row / 64] & (1 << (row % 64)) != 0;
        let present = bit(words) && !case.nulls[row];
        let found = present && keys.contains(&case.values[row]);
        ensure!(
            bit(&out.present) == present && bit(&out.found) == found,
            "{what}: row {row} differs from the model"
        );
    }
    Ok(())
}

/// Check the kernel on every set and the reference against the model
/// before anything is measured.
pub fn check<T: Width, C: ColumnReader<Value = T>>(
    input: &Input<'_, C, T>,
    case: &Fixture<T>,
    keys: &[T],
    direct: impl Fn(&[T], &Input<'_, C, T>, &mut Output) -> Result<()>,
) -> Result<()> {
    let mut out = Output::new(case.values.len());
    let reference_keys = &keys[..REFERENCE_KEYS];
    direct(reference_keys, input, &mut out)?;
    check_op(case, reference_keys, &out, "reference")?;
    for count in KEY_COUNTS {
        let keys = &keys[..count];
        in_set(&KeySet::new(keys), input, &mut out)?;
        check_op(case, keys, &out, &format!("keys{count}"))?;
    }
    Ok(())
}

pub fn bench(runner: &mut Runner) -> Result<()> {
    measure_width::<i32>(runner)?;
    measure_width::<i64>(runner)
}

fn measure_width<T: Width>(runner: &mut Runner) -> Result<()> {
    let cases = T::cases();
    let keys = keys(&cases)?;
    for case in &cases {
        measure_column(
            runner,
            "dense",
            &case.dense_column()?,
            dense_reference,
            case,
            &keys,
        )?;
        measure_column(
            runner,
            "datum",
            &case.datum_column()?,
            datum_reference,
            case,
            &keys,
        )?;
    }
    Ok(())
}

fn measure_column<T: Width, C: ColumnReader<Value = T>>(
    runner: &mut Runner,
    format: &str,
    column: &C,
    direct: impl Fn(&[T], &Input<'_, C, T>, &mut Output) -> Result<()> + Copy,
    case: &Fixture<T>,
    keys: &[T],
) -> Result<()> {
    use std::hint::black_box;
    let input = Input::new(column, case);
    check(&input, case, keys, direct)?;
    let mut out = Output::new(case.values.len());
    let mut group = runner.group(format!("set_int32/{}/{format}/{}", T::NAME, case.name));
    for count in KEY_COUNTS {
        let set = KeySet::new(&keys[..count]);
        group.op(&format!("keys{count}"), || {
            in_set(black_box(&set), black_box(&input), &mut out).unwrap()
        })?;
    }
    let reference_keys = &keys[..REFERENCE_KEYS];
    group.op("reference", || {
        direct(black_box(reference_keys), black_box(&input), &mut out).unwrap()
    })?;
    Ok(())
}
