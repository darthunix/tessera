//! A hash table for the engines of relational databases, kept in memory
//! the caller gives. Its calls take a batch of rows at a time, and a mask
//! chooses the rows of a batch: [`RowMask`], its view [`RowMaskView`],
//! and [`ones`], the walk over the set bits of a word. The table is
//! [`table`].

#![deny(unsafe_code)]

mod bits;
mod row_mask;
pub mod table;

pub use bits::ones;
pub use row_mask::{RowMask, RowMaskView, check_rows};
