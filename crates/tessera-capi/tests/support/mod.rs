//! Batches of the property tests over the capi representations: the same
//! rows as a dense column and as Datums with NULL flags, both with
//! unprepared gaps.
//!
//! A selected row is prepared; a gap row is left uninitialized, so that a
//! kernel reading it would read no value at all. The rows a kernel must not
//! read — NULL or unselected — hold edges of the type, and an int4 Datum
//! carries arbitrary high bits, which the int4 value does not include.

// Each test file uses a part of the module.
#![allow(dead_code)]

use std::mem::MaybeUninit;

use proptest::prelude::*;
use tessera_capi::{DatumIntColumn, DenseIntColumn, FromDatum};
use tessera_core::RowMaskView;
use tessera_testing::{Int, flags, nrows, values, words};

/// An integer width of the representations.
pub trait Width: Int + FromDatum + Ord {
    /// The Datum of a value, with `high` in the bits the width leaves free.
    fn datum(self, high: u32) -> u64;
}

impl Width for i32 {
    fn datum(self, high: u32) -> u64 {
        u64::from(self as u32) | u64::from(high) << 32
    }
}

impl Width for i64 {
    fn datum(self, _: u32) -> u64 {
        self as u64
    }
}

/// One batch, row by row: values, non-NULL and prepared flags, the
/// selection (prepared rows only) and the high bits of each Datum.
#[derive(Clone, Debug)]
pub struct Batch<T> {
    pub values: Vec<T>,
    pub non_null: Vec<bool>,
    /// `None` when every row is prepared.
    pub prepared: Option<Vec<bool>>,
    pub selected: Vec<bool>,
    pub high: Vec<u32>,
}

impl<T> Batch<T> {
    /// Whether a kernel reads the row's value: selected and not NULL.
    pub fn read(&self, row: usize) -> bool {
        self.selected[row] && self.non_null[row]
    }
}

/// Batches whose read rows hold values from `live`.
pub fn batches<T: Width>(live: BoxedStrategy<T>) -> impl Strategy<Value = Batch<T>> {
    nrows()
        .prop_flat_map(|nrows| {
            let gaps = prop_oneof![Just(None), flags(nrows).prop_map(Some)];
            (flags(nrows), flags(nrows), gaps)
        })
        .prop_flat_map(move |(selected, non_null, gaps)| {
            let prepared: Option<Vec<bool>> =
                gaps.map(|gaps| gaps.iter().map(|&gap| !gap).collect());
            let selected: Vec<bool> = (0..selected.len())
                .map(|row| selected[row] && prepared.as_ref().is_none_or(|ready| ready[row]))
                .collect();
            let read: Vec<bool> = (0..selected.len())
                .map(|row| selected[row] && non_null[row])
                .collect();
            (
                values(&read, &live),
                proptest::collection::vec(any::<u32>(), selected.len()),
                Just((non_null, prepared, selected)),
            )
        })
        .prop_map(|(values, high, (non_null, prepared, selected))| Batch {
            values,
            non_null,
            prepared,
            selected,
            high,
        })
}

/// The storage of a batch's two representations; gap rows stay
/// uninitialized.
pub struct Storage<T> {
    nrows: usize,
    dense: Vec<MaybeUninit<T>>,
    datums: Vec<MaybeUninit<u64>>,
    isnull: Vec<MaybeUninit<bool>>,
    non_null_words: Vec<u64>,
    prepared_words: Option<Vec<u64>>,
    pub selection: Vec<u64>,
}

impl<T: Width> Storage<T> {
    pub fn new(batch: &Batch<T>) -> Self {
        let nrows = batch.values.len();
        let mut dense = vec![MaybeUninit::uninit(); nrows];
        let mut datums = vec![MaybeUninit::uninit(); nrows];
        let mut isnull = vec![MaybeUninit::uninit(); nrows];
        for row in 0..nrows {
            if batch.prepared.as_ref().is_none_or(|ready| ready[row]) {
                dense[row].write(batch.values[row]);
                datums[row].write(batch.values[row].datum(batch.high[row]));
                isnull[row].write(!batch.non_null[row]);
            }
        }
        Self {
            nrows,
            dense,
            datums,
            isnull,
            non_null_words: words(&batch.non_null),
            prepared_words: batch.prepared.as_deref().map(words),
            selection: words(&batch.selected),
        }
    }

    pub fn rows(&self) -> RowMaskView<'_> {
        RowMaskView::try_new(self.nrows, &self.selection).unwrap()
    }

    fn prepared(&self) -> Option<RowMaskView<'_>> {
        let words = self.prepared_words.as_deref()?;
        Some(RowMaskView::try_new(self.nrows, words).unwrap())
    }

    pub fn dense(&self) -> DenseIntColumn<'_, T> {
        let non_nulls = RowMaskView::try_new(self.nrows, &self.non_null_words).unwrap();
        // SAFETY: every prepared row was written, NULL rows included.
        unsafe { DenseIntColumn::try_new(&self.dense, Some(non_nulls), self.prepared()) }.unwrap()
    }

    pub fn datum(&self) -> DatumIntColumn<'_, T> {
        // SAFETY: every prepared row has a Datum and a NULL flag.
        unsafe { DatumIntColumn::try_new(&self.datums, &self.isnull, self.prepared()) }.unwrap()
    }

    /// The NULL flags alone, as a count reads a column of any type.
    pub fn nulls(&self) -> DatumIntColumn<'_, ()> {
        // SAFETY: as for `datum`.
        unsafe { DatumIntColumn::try_new(&self.datums, &self.isnull, self.prepared()) }.unwrap()
    }
}
