//! A varlena's bytes read in place behind a Datum, for the kernels of
//! numeric and text. On a little-endian machine a first byte with the low
//! bit set is a 1-byte header (0x01 alone: an external pointer, never read
//! here) holding the size with the header in its upper seven bits; two low
//! bits of zero, a 4-byte header holding the size in its upper 30 bits;
//! two low bits of 10, a compressed value, never read here either.

use std::slice;

#[cfg(not(target_endian = "little"))]
compile_error!("the varlena headers here are little-endian's");

/// The bytes after a varlena's header, or `None` for a compressed or
/// external value or a null pointer.
///
/// # Safety
///
/// `datum` must be null or point to a whole varlena that stays valid and
/// unchanged for `'a`.
#[inline(always)]
pub(super) unsafe fn varlena_data<'a>(datum: u64) -> Option<&'a [u8]> {
    let pointer = datum as usize as *const u8;
    if pointer.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees a whole varlena at `pointer`; its
    // header states the size of the bytes that follow.
    unsafe {
        let first = *pointer;
        if first & 0x01 == 0x01 {
            if first == 0x01 {
                return None;
            }
            let size = usize::from(first >> 1);
            Some(slice::from_raw_parts(pointer.add(1), size - 1))
        } else if first & 0x03 == 0x00 {
            let size = (pointer.cast::<u32>().read_unaligned() >> 2) as usize;
            if size < 4 {
                return None;
            }
            Some(slice::from_raw_parts(pointer.add(4), size - 4))
        } else {
            None
        }
    }
}
