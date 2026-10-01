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
//! [`text`] compares strings, matches LIKE patterns of literals and `%`,
//! and counts and cuts characters over their bytes. [`set`] answers `x IN
//! (…)` of integer constants.
//! Errors do not roll back previously completed words; callers must discard a
//! partial selection after failure. This crate does not introduce a C entry
//! point or catch panics. The future C boundary remains responsible for panic
//! handling.
//!
//! Full prepared words of representations that expose their storage
//! ([`tessera_core::WordBlock`]) are compared, aggregated, computed and
//! hashed with vector code on AArch64, division by a scalar included;
//! everything else takes the row paths. [`table`] is a hash table in a
//! region of memory the caller owns, for joins and grouping, addressed by
//! offsets so that the same bytes serve local and shared memory, and
//! [`sort`] orders a table's records by their integer keys. `unsafe`
//! is denied crate-wide and allowed only in two isolated modules: [`simd`],
//! for vector loads, and [`table`], for the region over raw pointers.
//!
//! Code shape. `#[inline(always)]` and `#[inline(never)]` on a hot path
//! record a decision checked in the disassembly of the bench binaries or by
//! their PMU counters (`crates/tessera-capi/benches/README.md`): the comment
//! of the function or of its group gives the reason, and the attribute goes
//! only after the same check. Most such decisions concern the code LLVM
//! makes and hold on any target; a few were taken by timing on an Apple M5
//! Pro and are to be measured again on x86 with the AVX2 kernels:
//! `BULK_MIN_ROWS`, the bounds of [`set::SetValue::WORD_KEYS`] and the
//! branch-free compares of [`set`], and in `tessera-capi` the counted loop
//! of a full word and the branch-free read of a NULL slot.

#![deny(unsafe_code)]

/// Selected rows in the first multi-row word from which whole-word kernels
/// pay for the call, for every width: on an M5 Pro an int4 word costs 19
/// cycles dense and 27 cycles Datum, an int8 word 24 and 29 (compare-ZxCFic),
/// against about 2.4 to 2.5 cycles per selected row on the row path.
pub(crate) const BULK_MIN_ROWS: u32 = 12;

pub mod calendar;
pub mod cast;
pub mod count;
pub mod decimal;
mod int;
pub mod int32;
pub mod int64;
pub mod ops;
pub mod set;
mod simd;
pub mod sort;
pub mod spill_columns;
pub mod table;
pub mod text;
