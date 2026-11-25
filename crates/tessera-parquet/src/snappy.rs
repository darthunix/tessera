//! The decoder of Snappy's raw format, by which the reader decompresses
//! the pages of a chunk compressed with Snappy.
//!
//! A raw stream is its decompressed length as a varint of at most 5
//! bytes, then elements. Each element starts with a tag byte whose low
//! two bits give its kind: a literal, whose bytes follow, its length in
//! the tag or in the 1 to 4 bytes after it; or a copy of bytes already
//! written, at an offset back of 1, 2 or 4 bytes after the tag (11 bits
//! with 3 of them in the tag, 16 or 32 bits).
//!
//! The decoding follows that of a whole buffer in Google's snappy 1.2.1
//! (the crate's `LICENSE-snappy`, BSD-3): elements one at a time,
//! every length and offset checked before it is used.

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

const fn damaged(what: &'static str) -> Damaged {
    Damaged { what }
}

/// The decompressed length that the header of `input` gives, and the
/// header's length.
fn header(input: &[u8]) -> Result<(usize, usize), Damaged> {
    let mut len = 0u64;
    for (i, &b) in input.iter().take(5).enumerate() {
        len |= u64::from(b & 0x7f) << (7 * i);
        if b < 0x80 {
            if len > u64::from(u32::MAX) {
                return Err(damaged("a length past 2^32 - 1"));
            }
            return Ok((len as usize, i + 1));
        }
    }
    Err(damaged("its header"))
}

/// The decompressed length that the header of `input` gives.
///
/// # Errors
///
/// [`Damaged`] when `input` is empty, its header is not a varint of at
/// most 5 bytes or gives a length past 2^32 - 1, or a length that no
/// stream of `input`'s size can reach: an element of 3 bytes writes at
/// most 64.
pub fn decompress_len(input: &[u8]) -> Result<usize, Damaged> {
    let (len, _) = header(input)?;
    if len > input.len().saturating_mul(22) {
        return Err(damaged("a length past what its elements can write"));
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
        return Err(damaged("a length other than its output's"));
    }
    let (_, start) = header(input)?;
    careful(&input[start..], output)?;
    Ok(output.len())
}

/// The little-endian number of `bytes`.
fn little(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .rev()
        .fold(0, |n, &b| (n << 8) | usize::from(b))
}

/// Decodes the elements of `input` into the whole of `out`, one at a
/// time, every length and offset checked before it is used.
fn careful(input: &[u8], out: &mut [u8]) -> Result<(), Damaged> {
    let (mut ip, mut op) = (0, 0);
    while let Some(&tag) = input.get(ip) {
        ip += 1;
        if tag & 3 == 0 {
            let mut len = usize::from(tag >> 2) + 1;
            if len > 60 {
                let extra = len - 60;
                let Some(bytes) = input.get(ip..ip + extra) else {
                    return Err(damaged("a literal's length past its end"));
                };
                len = little(bytes) + 1;
                ip += extra;
            }
            let Some(from) = input.get(ip..ip + len) else {
                return Err(damaged("a literal past its end"));
            };
            let Some(to) = out.get_mut(op..op + len) else {
                return Err(damaged("a literal past its length"));
            };
            to.copy_from_slice(from);
            ip += len;
            op += len;
        } else {
            let bytes = [0, 1, 2, 4][usize::from(tag & 3)];
            let Some(after) = input.get(ip..ip + bytes) else {
                return Err(damaged("a copy's offset past its end"));
            };
            let (len, offset) = if tag & 3 == 1 {
                (
                    usize::from((tag >> 2) & 7) + 4,
                    (usize::from(tag >> 5) << 8) | little(after),
                )
            } else {
                (usize::from(tag >> 2) + 1, little(after))
            };
            ip += bytes;
            if offset == 0 || offset > op {
                return Err(damaged("a copy from before its start"));
            }
            if len > out.len() - op {
                return Err(damaged("a copy past its length"));
            }
            copy_back(out, op, offset, len);
            op += len;
        }
    }
    if op != out.len() {
        return Err(damaged("elements short of its length"));
    }
    Ok(())
}

/// `out[op + i] = out[op + i - offset]` for each `i` below `len`: a
/// pattern of `offset` bytes repeated when `offset < len`. Each step
/// copies all of the pattern written so far, twice the last step, so no
/// copy overlaps itself.
///
/// Requires `0 < offset <= op` and `op + len <= out.len()`.
fn copy_back(out: &mut [u8], op: usize, offset: usize, len: usize) {
    let mut done = 0;
    while done < len {
        let n = (offset + done).min(len - done);
        out.copy_within(op - offset..op - offset + n, op + done);
        done += n;
    }
}

#[cfg(test)]
mod tests;
