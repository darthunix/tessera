//! Safe computational kernels over borrowed Tessera columns and row masks.
//!
//! Kernels do not depend on PostgreSQL or call it. Column storage stays owned
//! by the caller; kernels neither copy columns nor retain their borrows after
//! returning. Successful operations do not allocate; constructing an error may.
//! Mutating a row mask requires exclusive access. Concurrent calls must use
//! disjoint mutable masks and respect the chosen reader's sharing guarantees.
//!
//! [`int32::filter`] implements scalar comparisons, [`count::count`] the
//! non-NULL count of any type, [`int32::sum`], [`int32::min`] and
//! [`int32::max`] the aggregates and
//! [`int32::arith_scalar`] and its siblings the arithmetic with PostgreSQL's
//! error codes, and [`int32::hash`] and [`int32::hash_next`] the key hashes
//! of joins and grouping, all through [`tessera_core::ColumnReader`],
//! independently of physical storage; [`int64`] mirrors the family for
//! int8, kernel by kernel, with the vocabulary of [`ops`] shared, and
//! [`count::count`] counts non-NULL rows of any type and
//! [`cast::int4_to_int8`] widens int4 values into an int8 column.
//! [`decimal`] reads PostgreSQL's numeric of at most 18 digits as an `i64`
//! at its display scale from the stored bytes, computes with it exactly and
//! writes the numeric back, a batch at a time over a caller's source of
//! rows, leaving to the caller every row it does not take. [`calendar`]
//! is PostgreSQL's calendar of dates and timestamps: its Julian day
//! routines, the truncations, intervals and fields of the date functions.
//! [`float`] holds the comparisons, arithmetic and casts of float4 and
//! float8 with the core's checks. [`text`] compares strings, matches LIKE patterns of literals and `%`,
//! and counts and cuts characters over their bytes. [`set`] answers `x IN
//! (…)` of integer constants.
//! Errors do not roll back previously completed words; callers must discard a
//! partial selection after failure. This crate does not introduce a C entry
//! point or catch panics: the C boundary, `tessera-capi`, does.
//!
//! Full prepared words of representations that expose their storage
//! ([`tessera_core::WordBlock`]) are compared, aggregated, computed,
//! hashed and looked up in sets of constants with vector code, division
//! by a scalar included: NEON on AArch64, and AVX2 on x86-64 where the
//! processor has it. Everything else takes the row paths.
//! [`table`] is a hash table in a region of memory the caller owns, for
//! joins and grouping, addressed by offsets so that the same bytes serve
//! local and shared memory, and [`sort`] orders a table's records by their
//! integer keys. `unsafe` is denied crate-wide and allowed only in
//! `simd`, for vector loads and intrinsics; [`table`] keeps its own in its
//! crate. [`phases`], [`shared_spill`] and [`spill_columns`] are the
//! spill's crate's, re-exported as [`table`] is.
//!
//! Code shape. `#[inline(always)]` and `#[inline(never)]` on a hot path
//! record a decision checked in the disassembly of the bench binaries or by
//! their PMU counters (`crates/tessera-capi/benches/README.md`): the comment
//! of the function or of its group gives the reason, and the attribute goes
//! only after the same check. Most such decisions concern the code LLVM
//! makes and hold on any target; a few were taken by timing. The bounds of
//! whole words of each driver (`WholeRows`), the bounds of the sets,
//! `BULK_MIN_ROWS` and [`set::SetValue::WORD_KEYS`], the step of their
//! search by halving, and in `tessera-capi` the counted loop of a full word
//! and the branch-free read of a NULL slot were timed on an Apple M5 Pro and
//! on an Intel Core i5 with AVX2.

#![deny(unsafe_code)]

/// Selected rows in a word from which the sets compare it whole, the bound
/// every driver had before each took its own (`WholeRows`): on an M5 Pro an
/// int4 word costs 19
/// cycles dense and 27 cycles Datum, an int8 word 24 and 29 (compare-ZxCFic),
/// against about 2.4 to 2.5 cycles per selected row on the row path. On an
/// Intel Core i5-1038NG7 with AVX2, words of 8 rows taken whole gain on the
/// filters, the hashes, the cast and `count`, and lose on the sums, `min`
/// and `max`, the arithmetic by a constant, the compare of Datum columns
/// and the sets of 16 and 64 keys, by up to 2.5 times the cycles (the
/// `eighth` cases, compare-KyEXSY to compare-tw1q3k), so x86-64 keeps 12.
pub(crate) const BULK_MIN_ROWS: u32 = 12;

/// The least selected rows of a word from which a driver takes it whole,
/// for each storage of the word's block; 64 takes only full words. A driver
/// decides on its first word with rows: a full word, or one with at least
/// the rows of its storage, goes whole with the rest of the call. It tests
/// the rows against the lesser bound before it asks for the block
/// (`may_take`), so a word under both bounds goes by rows at the cost of
/// its count alone, as it did against one bound. Each driver keeps its
/// bounds beside it, for each architecture, its widths and, where they
/// part, its operations. They were measured on an Intel Core i5-1038NG7
/// with AVX2 and on an Apple M5 Pro: the drivers taking no word whole
/// against taking every word whole, over words of 8 to 32 selected rows
/// drawn at random. A bound is the least of those from which every denser
/// one gains, past the noise of the case on both of two runs.
#[derive(Clone, Copy)]
pub(crate) struct WholeRows {
    /// Over a block of dense values.
    pub(crate) dense: u32,
    /// Over a block of Datum slots, or a call with a Datum operand.
    pub(crate) datum: u32,
}

impl WholeRows {
    /// One bound for both storages.
    pub(crate) const fn both(rows: u32) -> Self {
        Self {
            dense: rows,
            datum: rows,
        }
    }

    /// Whether a word of these selected rows may go whole over either
    /// storage, which a driver asks before it reads the word's block.
    #[inline(always)]
    pub(crate) fn may_take(self, selected: u64) -> bool {
        selected == u64::MAX || selected.count_ones() >= self.dense.min(self.datum)
    }

    /// Whether a word of these selected rows goes whole, over Datum slots
    /// or dense values.
    #[inline(always)]
    pub(crate) fn takes(self, selected: u64, datum: bool) -> bool {
        selected == u64::MAX || selected.count_ones() >= if datum { self.datum } else { self.dense }
    }

    /// Whether a word of these selected rows goes whole over this block.
    #[inline(always)]
    pub(crate) fn takes_block<T>(
        self,
        selected: u64,
        block: &tessera_core::WordBlock<'_, T>,
    ) -> bool {
        self.takes(
            selected,
            matches!(block, tessera_core::WordBlock::Datum { .. }),
        )
    }
}

pub mod accumulate;
pub mod calendar;
pub mod cast;
pub mod column_keys;
pub mod count;
pub mod decimal;
pub mod error;
pub mod float;
mod int;
pub mod int32;
pub mod int64;
pub mod ops;
pub mod order;
pub mod set;
mod simd;
pub mod smallint;
pub mod sort;
pub mod source;
pub use relhash as table;
pub use relspill::{phases, shared_spill, spill_columns};
pub mod text;
