//! Safe data structures and algorithms for Tessera.
//!
//! This crate has no PostgreSQL dependency and makes no PostgreSQL calls.
//! Views borrow initialized Rust slices without copying elements or freeing
//! their backing storage. Successful operations do not allocate; creating an
//! [`anyhow::Error`] may allocate. Fields are private so validation cannot be
//! bypassed. Mutable row masks use exclusive borrows, while read-only views use
//! shared borrows. No view can outlive its backing storage.
//!
//! [`ColumnReader`] adds typed indexed and selected reads. Its value type is
//! chosen by each representation, not a runtime tag. [`ColumnView`] implements
//! it for `Copy` values; its own `get` continues returning a reference.
//! [`WordValues`] validates each selection word before invoking its reader.
//! [`RowMaskView::try_from_bytes`] accepts offset bit windows without copying.
//!
//! Row selection and value nullness are independent: a selected row may be
//! null. Separate masks use the same [`RowMaskView`] type and physical row
//! indices, not positions in a packed selection. Constructors, row lookup,
//! and intersection return [`anyhow::Result`] for invalid input, not a panic.
//! [`RowMask::clear`] silently ignores absent and out-of-bounds rows.
//!
//! ```
//! use tessera_core::{ColumnView, RowMask, RowMaskView};
//!
//! let values = [10, 20, 30];
//! let non_null_bits = [0b101];
//! let mut row_bits = [0b111];
//! let non_nulls = RowMaskView::try_new(values.len(), &non_null_bits)?;
//! let column = ColumnView::try_new(&values, Some(non_nulls))?;
//! let mut rows = RowMask::try_new(values.len(), &mut row_bits)?;
//! rows.clear(0);
//!
//! let selected: Vec<_> = rows.as_view().selected_indices().collect();
//! assert_eq!(selected, [1, 2]);
//! assert_eq!(column.get(1)?, None);
//! assert_eq!(column.get(2)?, Some(&30));
//! # Ok::<(), anyhow::Error>(())
//! ```

#![forbid(unsafe_code)]

mod bitmap;
mod column;
mod reader;
mod row_mask;

pub use column::ColumnView;
pub use reader::{ColumnReader, WordValues};
pub use row_mask::{RowMask, RowMaskView};
