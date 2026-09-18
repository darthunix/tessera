//! `TessRowMask` and its borrowed Rust views.

use std::ffi::c_int;
use std::slice;

use anyhow::{Context, Result, ensure};
use tessera_core::{RowMask, RowMaskView};

/// `TessRowMask` from `tessera/row_mask.h`: a row count and borrowed words
/// with one bit per row, none set beyond the row count.
#[repr(C)]
#[derive(Debug)]
pub struct Mask {
    /// The number of rows the bits describe.
    pub nrows: c_int,
    /// `nrows.div_ceil(64)` words; may be null when there are no rows.
    pub bits: *mut u64,
}

impl Mask {
    /// The row count and the number of words.
    fn dimensions(&self) -> Result<(usize, usize)> {
        let nrows = usize::try_from(self.nrows).context("a row mask has a negative row count")?;
        let words = nrows.div_ceil(64);
        ensure!(
            words == 0 || !self.bits.is_null(),
            "a row mask has null bits"
        );
        Ok((nrows, words))
    }

    /// Borrow the mask for reading.
    ///
    /// # Safety
    ///
    /// `bits` must point to `nrows.div_ceil(64)` initialized words that stay
    /// valid and unchanged for the borrow.
    pub unsafe fn view(&self) -> Result<RowMaskView<'_>> {
        let (nrows, words) = self.dimensions()?;
        let bits = if words == 0 {
            &[]
        } else {
            // SAFETY: the caller guarantees `words` valid, initialized words.
            unsafe { slice::from_raw_parts(self.bits.cast_const(), words) }
        };
        RowMaskView::try_new(nrows, bits)
    }

    /// Borrow the mask for narrowing.
    ///
    /// # Safety
    ///
    /// As for [`Mask::view`], and nothing else may access the words for the
    /// borrow.
    pub unsafe fn mask(&mut self) -> Result<RowMask<'_>> {
        let (nrows, words) = self.dimensions()?;
        let bits = if words == 0 {
            &mut []
        } else {
            // SAFETY: the caller guarantees `words` valid, initialized words
            // that nothing else accesses.
            unsafe { slice::from_raw_parts_mut(self.bits, words) }
        };
        RowMask::try_new(nrows, bits)
    }

    /// Borrow an optional mask for reading: null means absent.
    ///
    /// # Safety
    ///
    /// `mask` must be null or point to a `Mask` valid for `'a` that
    /// satisfies [`Mask::view`]'s contract.
    pub unsafe fn view_optional<'a>(mask: *const Mask) -> Result<Option<RowMaskView<'a>>> {
        // SAFETY: the caller guarantees a null or valid pointer for `'a`.
        match unsafe { mask.as_ref() } {
            // SAFETY: the caller guarantees the words for `'a`.
            Some(mask) => Ok(Some(unsafe { mask.view() }?)),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Mask;

    #[test]
    fn layout_matches_the_header() {
        assert_eq!(size_of::<Mask>(), 16);
        assert_eq!(std::mem::offset_of!(Mask, bits), 8);
    }

    #[test]
    fn invalid_masks_are_rejected() {
        let mut words = [u64::MAX];
        let mut mask = Mask {
            nrows: 4,
            bits: words.as_mut_ptr(),
        };
        // SAFETY: local words.
        assert!(unsafe { mask.view() }.is_err(), "bits beyond the rows");
        mask.nrows = -1;
        // SAFETY: local words.
        assert!(unsafe { mask.mask() }.is_err());
        let mut null = Mask {
            nrows: 1,
            bits: std::ptr::null_mut(),
        };
        // SAFETY: a null pointer is rejected before use.
        assert!(unsafe { null.mask() }.is_err());
        null.nrows = 0;
        // SAFETY: no words are read for zero rows.
        assert_eq!(unsafe { null.view() }.unwrap().nrows(), 0);
    }
}
