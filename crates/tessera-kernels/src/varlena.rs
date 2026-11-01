//! The header of a varlena, PostgreSQL's variable-length value, in the
//! layout of the core the kernels are built for.
//!
//! PostgreSQL on a little-endian machine keeps the flags in the low bits
//! of the first byte: a low bit set is a 1-byte header (`0x01` alone: an
//! external pointer) holding the size with the header in its upper seven
//! bits; two low bits of zero a 4-byte header holding the size in its
//! upper 30 bits; two low bits of `10` a compressed value.
//!
//! Greengage keeps the layout PostgreSQL has on a big-endian machine on
//! every machine (the feature `network-varlena`): the flags in the high
//! bits of the first byte, a 4-byte size in network byte order. A high bit
//! set is a 1-byte header (`0x80` alone: an external pointer) holding the
//! size in its lower seven bits; two high bits of zero a 4-byte header
//! holding the size in its lower 30 bits; two high bits of `01` a
//! compressed value.
//!
//! An external pointer's second byte is its tag in both layouts.

/// What the first byte of a varlena says it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A 1-byte header; the size counts the header.
    Short(usize),
    /// A 4-byte header, not compressed: read the size with [`long_size`].
    Long,
    /// A 4-byte header of a compressed value: [`long_size`] too.
    Compressed,
    /// An external pointer: its tag follows the first byte.
    External,
}

/// The kind of a varlena by its first byte.
#[cfg(not(feature = "network-varlena"))]
#[inline(always)]
pub fn kind(first: u8) -> Kind {
    if first == 0x01 {
        Kind::External
    } else if first & 0x01 == 0x01 {
        Kind::Short(usize::from(first >> 1))
    } else if first & 0x03 == 0x00 {
        Kind::Long
    } else {
        Kind::Compressed
    }
}

/// The kind of a varlena by its first byte.
#[cfg(feature = "network-varlena")]
#[inline(always)]
pub fn kind(first: u8) -> Kind {
    if first == 0x80 {
        Kind::External
    } else if first & 0x80 == 0x80 {
        Kind::Short(usize::from(first & 0x7F))
    } else if first & 0xC0 == 0x00 {
        Kind::Long
    } else {
        Kind::Compressed
    }
}

/// The size, header included, of a 4-byte header's varlena.
#[inline(always)]
pub fn long_size(header: [u8; 4]) -> usize {
    #[cfg(not(feature = "network-varlena"))]
    let size = u32::from_le_bytes(header) >> 2;
    #[cfg(feature = "network-varlena")]
    let size = u32::from_be_bytes(header) & 0x3FFF_FFFF;
    size as usize
}

/// The 4-byte header of an uncompressed varlena of `size` bytes, header
/// included, at most 2^30 - 1.
#[inline(always)]
pub fn long_header(size: usize) -> [u8; 4] {
    debug_assert!(size < 1 << 30);
    #[cfg(not(feature = "network-varlena"))]
    let header = ((size as u32) << 2).to_le_bytes();
    #[cfg(feature = "network-varlena")]
    let header = (size as u32 & 0x3FFF_FFFF).to_be_bytes();
    header
}

/// The 1-byte header of a varlena of `size` bytes, header included, at
/// most 127; for tests and callers that build short values.
#[inline(always)]
pub fn short_header(size: usize) -> u8 {
    debug_assert!((2..=127).contains(&size));
    #[cfg(not(feature = "network-varlena"))]
    let header = ((size as u8) << 1) | 0x01;
    #[cfg(feature = "network-varlena")]
    let header = size as u8 | 0x80;
    header
}

/// The first byte of an external pointer.
#[cfg(not(feature = "network-varlena"))]
pub const EXTERNAL: u8 = 0x01;
/// The first byte of an external pointer.
#[cfg(feature = "network-varlena")]
pub const EXTERNAL: u8 = 0x80;

/// A first byte of a compressed value's 4-byte header.
#[cfg(not(feature = "network-varlena"))]
pub const COMPRESSED: u8 = 0x02;
/// A first byte of a compressed value's 4-byte header.
#[cfg(feature = "network-varlena")]
pub const COMPRESSED: u8 = 0x40;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_read_back_what_they_write() {
        for size in [4_usize, 5, 300, (1 << 30) - 1] {
            let header = long_header(size);
            assert_eq!(kind(header[0]), Kind::Long);
            assert_eq!(long_size(header), size);
        }
    }

    #[cfg(not(feature = "network-varlena"))]
    #[test]
    fn little_endian_first_bytes() {
        assert_eq!(kind(0x07), Kind::Short(3));
        assert_eq!(kind(0x01), Kind::External);
        assert_eq!(kind(0x02), Kind::Compressed);
        assert_eq!(kind(0x18), Kind::Long);
    }

    #[cfg(feature = "network-varlena")]
    #[test]
    fn network_order_first_bytes() {
        assert_eq!(kind(0x83), Kind::Short(3));
        assert_eq!(kind(0x80), Kind::External);
        assert_eq!(kind(0x40), Kind::Compressed);
        assert_eq!(kind(0x00), Kind::Long);
    }
}
