//! A varlena's bytes read in place behind a Datum, for the kernels of
//! numeric and text, by the header layout of the core
//! ([`tessera_kernels::varlena`]): a 1-byte or a 4-byte header is read,
//! an external pointer or a compressed value never is.

use std::slice;

use tessera_kernels::varlena::{Kind, kind, long_size};

#[cfg(all(not(target_endian = "little"), not(feature = "network-varlena")))]
compile_error!("the varlena headers here are little-endian's");

/// The bytes of the body of a TOAST pointer to data on disk after its
/// 2-byte header, by its tag: VARTAG_ONDISK_OID's `varatt_external_oid`
/// of 16 bytes, VARTAG_ONDISK_OID8's `varatt_external_oid8` of 20, as
/// `runtime/spill.c` asserts against PostgreSQL's headers; `None` for
/// another tag.
pub(super) fn ondisk_body(tag: u8) -> Option<usize> {
    match tag {
        18 => Some(16),
        4 => Some(20),
        _ => None,
    }
}

/// The bytes after a varlena's header, or `None` for a compressed or
/// external value or a null pointer.
///
/// # Safety
///
/// `datum` must be null or point to a whole varlena that stays valid and
/// unchanged for `'a`.
#[inline(always)]
pub(super) unsafe fn varlena_data<'a>(datum: u64) -> Option<&'a [u8]> {
    // A Datum is an integer that holds a pointer C made: its provenance
    // is exposed, which Miri takes on trust and says so.
    let pointer = core::ptr::with_exposed_provenance::<u8>(datum as usize);
    if pointer.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees a whole varlena at `pointer`; its
    // header states the size of the bytes that follow.
    unsafe {
        match kind(*pointer) {
            Kind::Short(size) => Some(slice::from_raw_parts(pointer.add(1), size - 1)),
            Kind::Long => {
                let size = long_size(pointer.cast::<[u8; 4]>().read_unaligned());
                if size < 4 {
                    return None;
                }
                Some(slice::from_raw_parts(pointer.add(4), size - 4))
            }
            Kind::Compressed | Kind::External => None,
        }
    }
}

/// The bytes of a varlena with its header, as `VARSIZE_ANY` counts them: a
/// 1-byte header's, a 4-byte header's (a compressed value's too), or an
/// external pointer's own, its 2-byte header and the pointer its tag
/// names. An expanded object is refused: its bytes are a pointer into its
/// process's memory, which a copy would outlive, and its caller flattens
/// it first.
///
/// # Safety
///
/// `pointer` must point to a whole varlena, valid for the call.
#[inline]
pub(super) unsafe fn varlena_size(pointer: *const u8) -> anyhow::Result<usize> {
    // SAFETY: the caller's contract; a header is read before the bytes it
    // says follow.
    unsafe {
        match kind(*pointer) {
            Kind::External => {
                // VARTAG_INDIRECT holds a pointer, VARTAG_ONDISK_OID and
                // _OID8 a pointer to data on disk; VARTAG_EXPANDED_RO and
                // _RW are 2 and 3.
                let body = match *pointer.add(1) {
                    1 => 8,
                    tag => ondisk_body(tag).ok_or_else(|| external_refused(tag))?,
                };
                Ok(2 + body)
            }
            Kind::Short(size) => Ok(size),
            Kind::Long | Kind::Compressed => {
                Ok(long_size(pointer.cast::<[u8; 4]>().read_unaligned()))
            }
        }
    }
}

/// The error of an external varlena a copy cannot take: an expanded
/// object, tag 2 or 3, or a tag none of PostgreSQL's. Cold, so that the
/// size of a varlena stays small enough to inline into the loops that
/// copy values.
#[cold]
#[inline(never)]
fn external_refused(tag: u8) -> anyhow::Error {
    if matches!(tag, 2 | 3) {
        anyhow::anyhow!("an expanded object, not flattened")
    } else {
        anyhow::anyhow!("an external varlena of tag {tag}")
    }
}
