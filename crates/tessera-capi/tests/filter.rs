//! The int4 and int8 filters over the capi representations: full prepared
//! words take the vector path, words with an unprepared row the row path,
//! and both must match the scalar model; NULL and unselected cells hold
//! edges that some comparison would keep if they were read.

mod support;

use anyhow::Result;
use proptest::prelude::*;
use tessera_core::{ColumnReader, RowMask};
use tessera_kernels::ops::CompareOp;
use tessera_kernels::{int32, int64};
use tessera_testing::{edge, integer, property, words};

use support::{Batch, Storage, Width, batches};

const OPS: [CompareOp; 6] = [
    CompareOp::Eq,
    CompareOp::Ne,
    CompareOp::Lt,
    CompareOp::Le,
    CompareOp::Gt,
    CompareOp::Ge,
];

/// One width under test and its filter.
trait Lane: Width {
    /// Values close to each other: -3 to 3, and for int8 also the same with
    /// a high half, which a 32-bit comparison would call equal.
    fn near() -> BoxedStrategy<Self>;

    fn filter<C: ColumnReader<Value = Self>>(
        column: &C,
        rows: &mut RowMask<'_>,
        op: CompareOp,
        scalar: Self,
    ) -> Result<()>;
}

impl Lane for i32 {
    fn near() -> BoxedStrategy<Self> {
        (-3..=3).boxed()
    }

    fn filter<C: ColumnReader<Value = Self>>(
        column: &C,
        rows: &mut RowMask<'_>,
        op: CompareOp,
        scalar: Self,
    ) -> Result<()> {
        int32::filter(column, rows, op, scalar)
    }
}

impl Lane for i64 {
    fn near() -> BoxedStrategy<Self> {
        prop_oneof![-3_i64..=3, (-3_i64..=3).prop_map(|value| value << 32)].boxed()
    }

    fn filter<C: ColumnReader<Value = Self>>(
        column: &C,
        rows: &mut RowMask<'_>,
        op: CompareOp,
        scalar: Self,
    ) -> Result<()> {
        int64::filter(column, rows, op, scalar)
    }
}

fn compare<T: Ord>(value: T, op: CompareOp, scalar: T) -> bool {
    match op {
        CompareOp::Eq => value == scalar,
        CompareOp::Ne => value != scalar,
        CompareOp::Lt => value < scalar,
        CompareOp::Le => value <= scalar,
        CompareOp::Gt => value > scalar,
        CompareOp::Ge => value >= scalar,
    }
}

fn comparable<T: Lane>() -> BoxedStrategy<T> {
    prop_oneof![1 => edge::<T>(), 2 => T::near(), 1 => integer::<T>()].boxed()
}

fn check<T: Lane, C: ColumnReader<Value = T>>(
    column: &C,
    batch: &Batch<T>,
    selection: &[u64],
    scalar: T,
    what: &str,
) -> Result<()> {
    let nrows = batch.values.len();
    for op in OPS {
        let expected: Vec<bool> = (0..nrows)
            .map(|row| batch.read(row) && compare(batch.values[row], op, scalar))
            .collect();
        let mut kept = selection.to_vec();
        T::filter(column, &mut RowMask::try_new(nrows, &mut kept)?, op, scalar)?;
        assert_eq!(kept, words(&expected), "{what} {op:?}");
    }
    Ok(())
}

fn representations_match_the_scalar_model<T: Lane>() {
    let cases = (batches(comparable::<T>()), comparable::<T>());
    property(cases, |(batch, scalar)| -> Result<()> {
        let storage = Storage::new(&batch);
        check(
            &storage.dense(),
            &batch,
            &storage.selection,
            scalar,
            "dense",
        )?;
        check(
            &storage.datum(),
            &batch,
            &storage.selection,
            scalar,
            "datum",
        )
    });
}

#[test]
fn representations_match_the_scalar_model_int4() {
    representations_match_the_scalar_model::<i32>();
}

#[test]
fn representations_match_the_scalar_model_int8() {
    representations_match_the_scalar_model::<i64>();
}
