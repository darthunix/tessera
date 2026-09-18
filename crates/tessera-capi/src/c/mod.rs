//! The C entry points, declared in `include/tessera/kernels.h`.
//!
//! Every entry point takes its inputs as the C structures of the batch
//! contract ([`DatumColumn`], [`Mask`]), runs a kernel under a panic guard,
//! and returns a [`Code`] that it also stores in the caller's [`Status`].
//! The rules of the boundary are in the crate documentation; the C-side
//! contract, in `docs/kernels.md`.

mod column;
mod int32;
mod mask;
mod status;

pub use column::DatumColumn;
pub use int32::{
    tess_int4_count, tess_int4_filter, tess_int4_max, tess_int4_min, tess_int4_sum,
    tess_kernels_abi_version, tess_kernels_layout, tess_kernels_test_panic,
};
pub use mask::Mask;
pub use status::{Code, MESSAGE_SIZE, Status};
