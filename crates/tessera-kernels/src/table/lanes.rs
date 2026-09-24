//! The prefetch hint of the vertical probe: [`crate::simd`]'s where it
//! exists, nothing elsewhere (other targets and Miri), in the manner of the
//! hash kernels' `bulk_op`.

#[cfg(all(target_arch = "aarch64", not(miri)))]
pub(super) use crate::simd::prefetch;

/// A prefetch hint where none is implemented: nothing.
#[cfg(not(all(target_arch = "aarch64", not(miri))))]
#[inline(always)]
pub(super) fn prefetch(_address: *const u8) {}
