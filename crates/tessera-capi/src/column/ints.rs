use std::fmt::Debug;
use std::hint::select_unpredictable;
use std::marker::PhantomData;
use std::mem::MaybeUninit;

use anyhow::{Result, ensure};
use tessera_core::{ColumnReader, RowMaskView, WordBlock, WordValues};

use super::{mask_word, try_fold_words, validate_mask, validate_ready};

/// A signed integer read from the bits of a PostgreSQL Datum, as the
/// `DatumGetInt32` and `DatumGetInt64` macros read them.
///
/// The targeted PostgreSQL master defines Datum as `uint64_t`, even on
/// 32-bit targets: int4 is the low 32 bits and int8 the whole word. The
/// width is a physical fact, not a declaration of PostgreSQL semantics;
/// other logical types of these widths need an interpretation of their own,
/// chosen by the caller.
pub trait FromDatum: Copy + Debug + 'static {
    /// The value the Datum encodes.
    fn from_datum(datum: u64) -> Self;
}

impl FromDatum for i32 {
    #[inline(always)]
    fn from_datum(datum: u64) -> Self {
        datum as i32
    }
}

impl FromDatum for i64 {
    #[inline(always)]
    fn from_datum(datum: u64) -> Self {
        datum as i64
    }
}

/// No width at all: the column reads its NULL flags and no value, which is
/// what a count needs of a column of any type.
impl FromDatum for () {
    #[inline(always)]
    fn from_datum(_: u64) -> Self {}
}

/// Whether every row of the word is prepared (an absent word is not).
#[inline]
fn fully_prepared(prepared: Option<RowMaskView<'_>>, word_index: usize) -> bool {
    mask_word(prepared, word_index) == u64::MAX
}

/// The 64 slots of a full word; `None` for the tail or an out-of-range word.
#[inline]
fn word_slots<T>(values: &[MaybeUninit<T>], word_index: usize) -> Option<&[MaybeUninit<T>; 64]> {
    let base = word_index.checked_mul(64)?;
    values.get(base..base.checked_add(64)?)?.try_into().ok()
}

/// View 64 slots of prepared rows as initialized values.
///
/// # Safety
///
/// Every slot must be initialized; the constructor contracts guarantee this
/// for prepared rows, and `prepared_word` returns only fully prepared words.
#[inline]
unsafe fn assume_word<T>(slots: &[MaybeUninit<T>; 64]) -> &[T; 64] {
    // SAFETY: MaybeUninit<T> has T's layout, and the caller guarantees every
    // slot is initialized; the borrow keeps the storage alive and immutable.
    unsafe { &*slots.as_ptr().cast::<[T; 64]>() }
}

// Read one dense slot of a word whose non-NULL flags are `bits`, without a
// branch or a data-dependent address: the slot is loaded unconditionally
// (a prepared row is initialized even when NULL) and the NULL bit selects
// the result. A conditional branch or a selected load address here
// alternated between a fast and a slower mode depending on per-core
// predictor state in the `next()`-based paths.
#[inline(always)]
fn read_masked<T: Copy>(values: &[MaybeUninit<T>], row: usize, bits: u64) -> Option<T> {
    let present = bits & (1 << (row % 64)) != 0;
    // SAFETY: callers pass only prepared rows below values.len() (RowMaskView
    // bits and WordValues rows are normalized to the row count), and the
    // constructor contract initializes every prepared row, NULL or not.
    let value = unsafe { values.get_unchecked(row).assume_init() };
    select_unpredictable(present, Some(value), None)
}

/// Borrowed dense integer storage with independent non-NULL and readiness
/// masks; [`DenseInt32Column`] and [`DenseInt64Column`] name its widths.
///
/// This is a physical format, not a declaration of PostgreSQL type semantics.
/// Sliced inputs use a sliced values buffer and correspondingly offset masks.
/// No view owns, frees, or copies its buffers, including uninitialized gaps.
/// Arrow-style primitive buffers can use this layout without an Arrow
/// dependency; encoded arrays such as dictionary arrays are not dense slices.
/// Construct offset masks with [`RowMaskView::try_from_bytes`].
///
/// [`ColumnReader::try_fold_selected`] chooses its NULL mode once and runs one
/// loop per selection word; without a NULL mask every read is unconditional.
///
/// NULL rows are read without a data-dependent branch: the slot is loaded
/// unconditionally and the NULL bit selects the result. Consumers keep their
/// loop branch-free by folding with `map_or`/`unwrap_or`; an `if let Some`
/// around the accumulation lowers to a select on the accumulator and
/// lengthens its dependency chain.
#[derive(Debug)]
pub struct DenseIntColumn<'a, T> {
    values: &'a [MaybeUninit<T>],
    non_nulls: Option<RowMaskView<'a>>,
    prepared: Option<RowMaskView<'a>>,
}

/// Dense int32 storage.
pub type DenseInt32Column<'a> = DenseIntColumn<'a, i32>;

/// Dense int64 storage.
pub type DenseInt64Column<'a> = DenseIntColumn<'a, i64>;

impl<'a, T: Copy + Debug> DenseIntColumn<'a, T> {
    /// Borrow dense values, rejecting masks with different physical row counts.
    ///
    /// Without `non_nulls`, all prepared rows are non-NULL. Without `prepared`,
    /// every row is prepared. A prepared NULL row holds an initialized value
    /// of no meaning; unprepared rows may be uninitialized.
    ///
    /// # Safety
    ///
    /// Every prepared row must contain an initialized value, NULL rows
    /// included (their value is arbitrary and never exposed). Buffers and
    /// masks must remain alive and immutable for `'a`, including against
    /// changes through C aliases. `prepared` must describe actual readiness,
    /// not a changing active-row selection. Raw slices must first satisfy
    /// Rust's alignment, allocation, and lifetime requirements; borrow them as
    /// `MaybeUninit`, never as initialized values when gaps may be uninitialized.
    /// No borrow may survive the eventual enclosing call from C.
    ///
    /// A borrowed readiness mask cannot be changed while the column is used:
    ///
    /// ```compile_fail
    /// use std::mem::MaybeUninit;
    /// use tessera_capi::DenseInt32Column;
    /// use tessera_core::{ColumnReader, RowMaskView};
    /// let values = [MaybeUninit::new(42)];
    /// let mut words = [1];
    /// let prepared = RowMaskView::try_new(1, &words).unwrap();
    /// // SAFETY: the only prepared value is initialized and non-NULL.
    /// let column = unsafe { DenseInt32Column::try_new(&values, None, Some(prepared)) }.unwrap();
    /// words[0] = 0;
    /// assert_eq!(column.get(0).unwrap(), Some(42));
    /// ```
    pub unsafe fn try_new(
        values: &'a [MaybeUninit<T>],
        non_nulls: Option<RowMaskView<'a>>,
        prepared: Option<RowMaskView<'a>>,
    ) -> Result<Self> {
        validate_mask(values.len(), non_nulls)?;
        validate_mask(values.len(), prepared)?;
        Ok(Self {
            values,
            non_nulls,
            prepared,
        })
    }
}

impl<T: Copy + Debug> ColumnReader for DenseIntColumn<'_, T> {
    type Value = T;

    fn nrows(&self) -> usize {
        self.values.len()
    }

    #[inline(always)]
    fn get(&self, row: usize) -> Result<Option<T>> {
        validate_ready(self.nrows(), self.prepared, row)?;
        if let Some(mask) = self.non_nulls
            && !mask.contains(row)?
        {
            return Ok(None);
        }
        // SAFETY: readiness and non-nullness were checked; the constructor
        // requires initialized values for these rows throughout the borrow.
        Ok(Some(unsafe { self.values[row].assume_init() }))
    }

    #[inline(always)]
    fn word_values(
        &self,
        word_index: usize,
        selected: u64,
    ) -> Result<impl Iterator<Item = (usize, Option<T>)> + '_> {
        let prepared = mask_word(self.prepared, word_index);
        // Without a NULL mask every flag is set and the read is unconditional.
        let non_nulls = mask_word(self.non_nulls, word_index);
        let values = self.values;
        WordValues::try_new(
            self.nrows(),
            word_index,
            selected,
            prepared,
            // WordValues invokes this closure only for in-bounds, prepared rows.
            move |row: usize| read_masked(values, row, non_nulls),
        )
    }

    #[inline]
    fn try_fold_selected<B, F>(&self, rows: &RowMaskView<'_>, init: B, fold: F) -> Result<B>
    where
        F: FnMut(B, usize, Option<T>) -> Result<B>,
    {
        let values = self.values;
        if self.non_nulls.is_none() {
            return try_fold_words(
                self.nrows(),
                rows,
                self.prepared,
                None,
                // SAFETY: try_fold_words validates dimensions and readiness
                // before invoking read; without NULLs every prepared row is
                // initialized. RowMaskView supplies only in-bounds bits.
                |row, _| Some(unsafe { values.get_unchecked(row).assume_init() }),
                init,
                fold,
            );
        }
        try_fold_words(
            self.nrows(),
            rows,
            self.prepared,
            self.non_nulls,
            // try_fold_words validates dimensions and readiness before
            // invoking read; RowMaskView supplies only in-bounds bits.
            |row, bits| read_masked(values, row, bits),
            init,
            fold,
        )
    }

    // Always inlined: returned through memory, the block costs its caller a
    // call and a copy per word, as much as a third of the vector compare.
    #[inline(always)]
    fn word_block(&self, word_index: usize) -> Option<WordBlock<'_, T>> {
        if !fully_prepared(self.prepared, word_index) {
            return None;
        }
        let slots = word_slots(self.values, word_index)?;
        Some(WordBlock::Dense {
            // SAFETY: every row of this word is prepared, hence initialized.
            values: unsafe { assume_word(slots) },
            non_nulls: mask_word(self.non_nulls, word_index),
        })
    }
}

/// Borrowed PostgreSQL Datum storage interpreted as integers of one width;
/// [`DatumInt32Column`] and [`DatumInt64Column`] name the widths.
///
/// The interpretation is [`FromDatum`]'s: int4 from the low 32 bits of
/// each Datum, int8 from the whole word, as PostgreSQL's macros read them.
/// Other logical PostgreSQL types require their own compatible interpretation.
#[derive(Debug)]
pub struct DatumIntColumn<'a, T> {
    values: &'a [MaybeUninit<u64>],
    isnull: &'a [MaybeUninit<bool>],
    prepared: Option<RowMaskView<'a>>,
    width: PhantomData<T>,
}

/// Datum storage read as int4.
pub type DatumInt32Column<'a> = DatumIntColumn<'a, i32>;

/// Datum storage read as int8.
pub type DatumInt64Column<'a> = DatumIntColumn<'a, i64>;

/// Datum storage of any type read for its NULL flags alone.
pub type DatumNullsColumn<'a> = DatumIntColumn<'a, ()>;

impl<'a, T: FromDatum> DatumIntColumn<'a, T> {
    /// Borrow Datum values and NULL flags, rejecting mismatched dimensions.
    ///
    /// Without `prepared`, every row is prepared. Both buffers may contain
    /// uninitialized unprepared rows; a prepared NULL row holds an initialized
    /// Datum of no meaning, as PostgreSQL slots provide.
    ///
    /// # Safety
    ///
    /// Every prepared row must have an initialized, valid Rust `bool` in
    /// `isnull` and an initialized `u64` in `values`; for a non-NULL row the
    /// `u64` encodes a PostgreSQL integer of the column's width, for a NULL
    /// row it is arbitrary and never exposed. Buffers and masks must remain
    /// alive and immutable for `'a`, including against C aliases. `prepared`
    /// describes actual readiness, not a changing active selection. Raw
    /// slices must satisfy Rust's alignment, allocation, and lifetime
    /// requirements and must use `MaybeUninit` for potentially uninitialized
    /// storage. No borrow may survive the eventual enclosing call from C.
    ///
    /// A view cannot outlive its source:
    ///
    /// ```compile_fail
    /// use std::mem::MaybeUninit;
    /// use tessera_capi::DatumInt32Column;
    /// use tessera_core::ColumnReader;
    /// let column;
    /// {
    ///     let values = [MaybeUninit::new(42)];
    ///     let isnull = [MaybeUninit::new(false)];
    ///     // SAFETY: the only row is initialized and non-NULL.
    ///     column = unsafe { DatumInt32Column::try_new(&values, &isnull, None) }.unwrap();
    /// }
    /// assert_eq!(column.get(0).unwrap(), Some(42));
    /// ```
    pub unsafe fn try_new(
        values: &'a [MaybeUninit<u64>],
        isnull: &'a [MaybeUninit<bool>],
        prepared: Option<RowMaskView<'a>>,
    ) -> Result<Self> {
        ensure!(
            values.len() == isnull.len(),
            "column and NULL flag row counts differ"
        );
        validate_mask(values.len(), prepared)?;
        Ok(Self {
            values,
            isnull,
            prepared,
            width: PhantomData,
        })
    }

    /// Read only a row already checked by get or WordValues, without a
    /// data-dependent branch: flag and Datum are loaded unconditionally and
    /// the flag selects the result, so random NULLs cost no mispredictions.
    #[inline]
    unsafe fn read_prepared(&self, row: usize) -> Option<T> {
        // SAFETY: the caller established bounds and readiness; construction
        // guarantees a valid initialized bool and an initialized Datum for
        // every prepared row. The conversion is the PostgreSQL macro's.
        let null = unsafe { self.isnull.get_unchecked(row).assume_init() };
        let value = T::from_datum(unsafe { self.values.get_unchecked(row).assume_init() });
        select_unpredictable(null, None, Some(value))
    }
}

impl<T: FromDatum> ColumnReader for DatumIntColumn<'_, T> {
    type Value = T;

    fn nrows(&self) -> usize {
        self.values.len()
    }

    #[inline(always)]
    fn get(&self, row: usize) -> Result<Option<T>> {
        validate_ready(self.nrows(), self.prepared, row)?;
        // SAFETY: bounds and readiness were checked above.
        Ok(unsafe { self.read_prepared(row) })
    }

    #[inline(always)]
    fn word_values(
        &self,
        word_index: usize,
        selected: u64,
    ) -> Result<impl Iterator<Item = (usize, Option<T>)> + '_> {
        WordValues::try_new(
            self.nrows(),
            word_index,
            selected,
            mask_word(self.prepared, word_index),
            move |row| {
                // SAFETY: WordValues calls this closure only for in-bounds,
                // prepared rows, after validating the entire selection word.
                unsafe { self.read_prepared(row) }
            },
        )
    }

    #[inline]
    fn try_fold_selected<B, F>(&self, rows: &RowMaskView<'_>, init: B, fold: F) -> Result<B>
    where
        F: FnMut(B, usize, Option<T>) -> Result<B>,
    {
        try_fold_words(
            self.nrows(),
            rows,
            self.prepared,
            None,
            // SAFETY: try_fold_words validates dimensions and readiness before
            // invoking read, and visits only in-bounds RowMaskView bits.
            |row, _| unsafe { self.read_prepared(row) },
            init,
            fold,
        )
    }

    #[inline(always)]
    fn word_block(&self, word_index: usize) -> Option<WordBlock<'_, T>> {
        if !fully_prepared(self.prepared, word_index) {
            return None;
        }
        let values = word_slots(self.values, word_index)?;
        let isnull = word_slots(self.isnull, word_index)?;
        // SAFETY: every row of this word is prepared, so both its Datum and
        // its flag are initialized by the constructor contract.
        Some(WordBlock::Datum {
            values: unsafe { assume_word(values) },
            isnull: unsafe { assume_word(isnull) },
        })
    }
}
