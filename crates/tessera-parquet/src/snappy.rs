//! The decoder of Snappy's raw format, by which the reader decompresses
//! the pages of a chunk compressed with Snappy.
//!
//! A raw stream is its decompressed length as a varint of at most 5
//! bytes, then elements: literals, whose bytes follow their tag, and
//! copies of bytes already written, at an offset back.

use std::fmt;

/// A stream the decoder cannot decode: what of it failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Damaged {
    what: &'static str,
}

impl fmt::Display for Damaged {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "damaged Snappy stream: {}", self.what)
    }
}

impl std::error::Error for Damaged {}

/// The decompressed length that the header of `input` gives.
///
/// # Errors
///
/// [`Damaged`] when `input` is empty, its header is not a varint of at
/// most 5 bytes or gives a length past 2^32 - 1, or a length that no
/// stream of `input`'s size can reach: an element of 3 bytes writes at
/// most 64.
pub fn decompress_len(input: &[u8]) -> Result<usize, Damaged> {
    if input.is_empty() {
        return Err(Damaged { what: "no header" });
    }
    let len = snap::raw::decompress_len(input).map_err(|_| Damaged { what: "its header" })?;
    if len > input.len().saturating_mul(22) {
        return Err(Damaged {
            what: "a length past what its elements can write",
        });
    }
    Ok(len)
}

/// Decompresses the raw stream `input` into `output`, which must be of
/// the length [`decompress_len`] gives, and returns that length.
///
/// # Errors
///
/// [`Damaged`] for a header [`decompress_len`] refuses or a length other
/// than `output`'s, or when an element runs past the end of `input` or
/// of `output`, copies from before the start of `output`, or the
/// elements end short of it. What `output` holds after an error is
/// unspecified.
pub fn decompress(input: &[u8], output: &mut [u8]) -> Result<usize, Damaged> {
    if decompress_len(input)? != output.len() {
        return Err(Damaged {
            what: "a length other than its output's",
        });
    }
    snap::raw::Decoder::new()
        .decompress(input, output)
        .map_err(|_| Damaged {
            what: "its elements",
        })
}
